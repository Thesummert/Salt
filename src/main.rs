mod history;

use anyhow::{Context, Result, bail};
use history::{HistoryEntry, HistoryStore};
use slint::{ComponentHandle, Model, ModelRc, VecModel};
use std::{
    path::{Path, PathBuf},
    sync::mpsc,
    thread,
    time::{Duration, Instant},
};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

slint::include_modules!();

const PREVIEW_LIMIT: u64 = 8 * 1024 * 1024;
const TEXT_LIMIT: usize = 256 * 1024;

struct Connection {
    client: smb2::SmbClient,
    tree: smb2::Tree,
    path: String,
}
enum Command {
    Connect(String, String, String, String),
    Navigate(String),
    Upload(PathBuf),
    Download(String, PathBuf),
    Preview(String),
    SelectHistory(usize),
    DeleteHistory(usize),
    Thumbnail(String, String, usize, i32),
}
fn remote_path(dir: &str, name: &str) -> String {
    if dir.is_empty() {
        name.to_owned()
    } else {
        format!("{dir}/{name}")
    }
}
fn format_size(size: u64) -> String {
    if size < 1024 {
        format!("{size} B")
    } else if size < 1024 * 1024 {
        format!("{:.1} KB", size as f64 / 1024.)
    } else if size < 1024 * 1024 * 1024 {
        format!("{:.1} MB", size as f64 / 1048576.)
    } else {
        format!("{:.1} GB", size as f64 / 1073741824.)
    }
}
fn update(ui: &slint::Weak<App>, f: impl FnOnce(App) + Send + 'static) {
    let weak = ui.clone();
    let _ = slint::invoke_from_event_loop(move || {
        if let Some(ui) = weak.upgrade() {
            f(ui);
        }
    });
}
fn status(ui: &slint::Weak<App>, message: String) {
    update(ui, move |app| app.set_status(message.into()));
}
async fn listing(c: &mut Connection) -> Result<Vec<smb2::DirectoryEntry>> {
    let mut items = c.client.list_directory(&mut c.tree, &c.path).await?;
    items.retain(|e| e.name != "." && e.name != "..");
    Ok(items)
}

fn sort_entries(app: &App) {
    let model = app.get_entries();
    let mut entries: Vec<Entry> = model.iter().collect();
    let rule = app.get_sort_index();
    let descending = app.get_sort_descending();
    entries.sort_by(|a, b| compare_entries(a, b, rule, descending));
    app.set_listing_generation(app.get_listing_generation().wrapping_add(1));
    app.set_entries(ModelRc::new(VecModel::from(entries)));
}

fn compare_entries(a: &Entry, b: &Entry, rule: i32, descending: bool) -> std::cmp::Ordering {
    let key = match rule {
        1 => a.kind.cmp(&b.kind),
        2 => a.size_key.cmp(&b.size_key),
        3 => a.modified_key.cmp(&b.modified_key),
        _ => a.name.to_lowercase().cmp(&b.name.to_lowercase()),
    };
    b.directory
        .cmp(&a.directory)
        .then_with(|| if descending { key.reverse() } else { key })
        .then_with(|| a.name.to_lowercase().cmp(&b.name.to_lowercase()))
}

fn show_listing(ui: &slint::Weak<App>, c: &Connection, entries: Vec<smb2::DirectoryEntry>) {
    let path = c.path.clone();
    update(ui, move |app| {
        app.set_current_path(path.into());
        let entries: Vec<Entry> = entries
            .into_iter()
            .map(|e| {
                let kind = if e.is_directory {
                    "文件夹".to_owned()
                } else {
                    Path::new(&e.name)
                        .extension()
                        .and_then(|s| s.to_str())
                        .filter(|s| !s.is_empty())
                        .map(|s| format!("{} 文件", s.to_uppercase()))
                        .unwrap_or_else(|| "文件".to_owned())
                };
                let modified = e
                    .modified
                    .to_system_time()
                    .map(|time| {
                        let date: chrono::DateTime<chrono::Local> = time.into();
                        date.format("%Y-%m-%d %H:%M").to_string()
                    })
                    .unwrap_or_else(|| "—".to_owned());
                Entry {
                    name: e.name.into(),
                    directory: e.is_directory,
                    detail: if e.is_directory {
                        "—".into()
                    } else {
                        format_size(e.size).into()
                    },
                    kind: kind.into(),
                    modified: modified.into(),
                    size_key: format!("{:020}", e.size).into(),
                    modified_key: format!("{:020}", e.modified.0).into(),
                    thumbnail: Default::default(),
                    has_thumbnail: false,
                }
            })
            .collect();
        app.set_entries(ModelRc::new(VecModel::from(entries)));
        sort_entries(&app);
        app.set_selected("".into());
        app.set_selected_directory(false);
        app.set_connected(true);
        app.set_preview_text("".into());
        app.set_preview_image(Default::default());
        app.set_showing_image(false);
    });
}
fn show_history(ui: &slint::Weak<App>, history: &HistoryStore) {
    let mut labels = vec![slint::SharedString::from("选择历史连接")];
    labels.extend(
        history.entries.iter().map(|entry| {
            format!("{} / {}  ·  {}", entry.server, entry.share, entry.username).into()
        }),
    );
    update(ui, move |app| {
        app.set_history_labels(ModelRc::new(VecModel::from(labels)));
        app.set_history_index(0);
    });
}
async fn run_command(
    cmd: Command,
    state: &mut Option<Connection>,
    ui: &slint::Weak<App>,
    history: &mut Option<HistoryStore>,
) -> Result<()> {
    match cmd {
        Command::Connect(server, share, user, password) => {
            if server.trim().is_empty() || share.trim().is_empty() {
                bail!("请填写服务器和共享名称");
            }
            let server = server.trim();
            let address = if server.parse::<std::net::SocketAddr>().is_ok() {
                server.to_owned()
            } else if server.parse::<std::net::Ipv6Addr>().is_ok() {
                format!("[{server}]:445")
            } else if server.contains(':') {
                server.to_owned()
            } else {
                format!("{server}:445")
            };
            let mut client = smb2::connect(&address, &user, &password)
                .await
                .context("连接或身份验证失败")?;
            let tree = client
                .connect_share(share.trim())
                .await
                .context("无法连接共享")?;
            let mut c = Connection {
                client,
                tree,
                path: String::new(),
            };
            let entries = listing(&mut c).await.context("无法读取共享目录")?;
            show_listing(ui, &c, entries);
            update(ui, |app| app.set_show_connection(false));
            *state = Some(c);
            if let Some(history) = history {
                let record = HistoryEntry {
                    server: server.to_owned(),
                    share: share.trim().to_owned(),
                    username: user,
                };
                let saved = history.record(record);
                show_history(ui, history);
                match saved {
                    Ok(()) => status(ui, "连接成功".into()),
                    Err(e) => status(ui, format!("连接成功，但历史记录保存失败：{e:#}")),
                }
            } else {
                status(ui, "连接成功，但历史记录不可用".into());
            }
        }
        Command::Thumbnail(..) => unreachable!("thumbnail requests are handled by the worker"),
        Command::SelectHistory(index) => {
            let entry = history
                .as_ref()
                .and_then(|h| h.entries.get(index))
                .context("历史记录不存在")?
                .clone();
            update(ui, move |app| {
                app.set_server(entry.server.into());
                app.set_share(entry.share.into());
                app.set_username(entry.username.into());
                app.set_password("".into());
                app.set_status("已填入历史连接，请输入密码".into());
            });
        }
        Command::DeleteHistory(index) => {
            let history = history.as_mut().context("历史记录不可用")?;
            history.delete(index)?;
            show_history(ui, history);
            status(ui, "历史记录已删除".into());
        }
        Command::Navigate(name) => {
            let c = state.as_mut().context("尚未连接")?;
            let new_path = match name.as_str() {
                "." => c.path.clone(),
                ".." => c
                    .path
                    .rsplit_once('/')
                    .map(|(p, _)| p.to_owned())
                    .unwrap_or_default(),
                _ => remote_path(&c.path, &name),
            };
            let old = std::mem::replace(&mut c.path, new_path);
            match listing(c).await {
                Ok(entries) => {
                    show_listing(ui, c, entries);
                    status(ui, "目录已更新".into());
                }
                Err(e) => {
                    c.path = old;
                    return Err(e);
                }
            }
        }
        Command::Upload(local) => {
            let c = state.as_mut().context("尚未连接")?;
            let name = local
                .file_name()
                .and_then(|x| x.to_str())
                .context("不支持此文件名")?;
            let mut input = tokio::fs::File::open(&local).await?;
            let total = input.metadata().await?.len();
            let mut writer = c
                .client
                .create_file_writer_exclusive(&c.tree, &remote_path(&c.path, name))
                .await
                .context("无法创建远程文件（可能已存在同名文件）")?;
            let mut buf = vec![0u8; 1024 * 1024];
            let mut sent = 0u64;
            let mut last = Instant::now();
            loop {
                let n = input
                    .read(&mut buf)
                    .await
                    .context("读取失败，远程可能留下未完成文件")?;
                if n == 0 {
                    break;
                }
                writer
                    .write_chunk(&buf[..n])
                    .await
                    .context("上传失败，远程可能留下未完成文件")?;
                sent += n as u64;
                if last.elapsed() >= Duration::from_millis(250) {
                    status(
                        ui,
                        format!("上传中：{} / {}", format_size(sent), format_size(total)),
                    );
                    last = Instant::now();
                }
            }
            writer
                .finish()
                .await
                .context("完成上传失败，远程可能留下未完成文件")?;
            let entries = listing(c).await?;
            show_listing(ui, c, entries);
            status(ui, format!("上传完成：{name}"));
        }
        Command::Download(name, local) => {
            let c = state.as_mut().context("尚未连接")?;
            let mut download = c
                .client
                .open_file_download(&c.tree, &remote_path(&c.path, &name))
                .await?;
            let total = download.size();
            let mut output = tokio::fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(&local)
                .await
                .context("无法创建本地文件（请确认目标不存在）")?;
            let result: Result<()> = async {
                let mut last = Instant::now();
                while let Some(chunk) = download.next_chunk().await {
                    let bytes = chunk?;
                    output.write_all(&bytes).await?;
                    if last.elapsed() >= Duration::from_millis(250) {
                        status(
                            ui,
                            format!(
                                "下载中：{} / {}",
                                format_size(download.bytes_received()),
                                format_size(total)
                            ),
                        );
                        last = Instant::now();
                    }
                }
                output.flush().await?;
                Ok(())
            }
            .await;
            drop(output);
            if result.is_err() {
                let _ = tokio::fs::remove_file(&local).await;
            }
            result?;
            status(ui, format!("下载完成：{name}"));
        }
        Command::Preview(name) => {
            let c = state.as_mut().context("尚未连接")?;
            let reader = c
                .client
                .open_file_reader(&c.tree, &remote_path(&c.path, &name))
                .await?;
            let ext = Path::new(&name)
                .extension()
                .and_then(|x| x.to_str())
                .unwrap_or("")
                .to_ascii_lowercase();
            let image = matches!(
                ext.as_str(),
                "png" | "jpg" | "jpeg" | "gif" | "webp" | "bmp"
            );
            let limit = if image {
                PREVIEW_LIMIT
            } else {
                TEXT_LIMIT as u64
            };
            if image && reader.size() > limit {
                reader.close().await?;
                bail!("图片超过 8 MB，请下载后查看");
            }
            let size = reader.size();
            let data = reader.read_at(0, size.min(limit)).await?;
            reader.close().await?;
            if image {
                // Decode on the UI thread from an isolated temporary file, then remove it.
                let temp = std::env::temp_dir().join(format!(
                    "salt-preview-{}-{}.{}",
                    std::process::id(),
                    std::time::SystemTime::now()
                        .duration_since(std::time::UNIX_EPOCH)?
                        .as_nanos(),
                    ext
                ));
                tokio::fs::write(&temp, data).await?;
                update(ui, move |app| {
                    let loaded = slint::Image::load_from_path(&temp);
                    let _ = std::fs::remove_file(&temp);
                    match loaded {
                        Ok(img) => {
                            app.set_preview_image(img);
                            app.set_showing_image(true);
                            app.set_status(format!("预览：{name}").into());
                        }
                        Err(e) => {
                            app.set_showing_image(false);
                            app.set_preview_text(format!("无法预览图片：{e}").into());
                        }
                    }
                });
            } else {
                let text = match std::str::from_utf8(&data) {
                    Ok(text) => text,
                    Err(e) if size > limit && e.error_len().is_none() => {
                        std::str::from_utf8(&data[..e.valid_up_to()])?
                    }
                    Err(_) => bail!("此文件不是 UTF-8 文本，无法预览"),
                };
                if text.contains('\0') {
                    bail!("此文件不是文本，无法预览");
                }
                let text = format!(
                    "{}{}",
                    text,
                    if size > limit {
                        "\n\n（仅显示前 256 KB）"
                    } else {
                        ""
                    }
                );
                update(ui, move |app| {
                    app.set_preview_text(text.into());
                    app.set_showing_image(false);
                });
                status(ui, format!("预览：{name}"));
            }
        }
    }
    Ok(())
}
fn main() -> Result<()> {
    let app = App::new()?;
    let weak = app.as_weak();
    let (tx, rx) = mpsc::channel::<Command>();
    let current_generation = std::sync::Arc::new(std::sync::atomic::AtomicI32::new(0));
    let worker_generation = current_generation.clone();
    thread::spawn(move || {
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .build()
            .expect("创建运行时失败");
        let mut state: Option<Connection> = None;
        let mut history = match HistoryStore::load_default() {
            Ok(history) => {
                show_history(&weak, &history);
                Some(history)
            }
            Err(e) => {
                status(&weak, format!("无法加载历史记录：{e:#}"));
                None
            }
        };
        let permits = std::sync::Arc::new(tokio::sync::Semaphore::new(4));
        let pending = std::sync::Arc::new(std::sync::Mutex::new(std::collections::HashSet::new()));
        for cmd in rx {
            if let Command::Thumbnail(ref name, ref path, index, generation) = cmd {
                let ext = Path::new(name)
                    .extension()
                    .and_then(|e| e.to_str())
                    .unwrap_or("")
                    .to_ascii_lowercase();
                if !matches!(
                    ext.as_str(),
                    "png" | "jpg" | "jpeg" | "gif" | "webp" | "bmp"
                ) {
                    continue;
                }
                if generation != worker_generation.load(std::sync::atomic::Ordering::Relaxed) {
                    continue;
                }
                let Some(c) = state.as_mut() else {
                    continue;
                };
                if &c.path != path {
                    continue;
                }
                let key = (generation, index);
                {
                    let mut pending = pending.lock().unwrap();
                    if pending.contains(&key) {
                        continue;
                    }
                    if pending.len() >= 64 {
                        let retry_name = name.clone();
                        update(&weak, move |app| {
                            let retry_ui = app.as_weak();
                            slint::Timer::single_shot(Duration::from_millis(300), move || {
                                if let Some(app) = retry_ui.upgrade() {
                                    if app.get_listing_generation() == generation {
                                        app.invoke_thumbnail_request(
                                            retry_name.into(),
                                            index as i32,
                                            generation,
                                        );
                                    }
                                }
                            });
                        });
                        continue;
                    }
                    pending.insert(key);
                }
                let reader =
                    runtime.block_on(c.client.open_file_reader(&c.tree, &remote_path(path, name)));
                let Ok(reader) = reader else {
                    pending.lock().unwrap().remove(&key);
                    continue;
                };
                let permits = permits.clone();
                let pending = pending.clone();
                let weak = weak.clone();
                let current = worker_generation.clone();
                let name = name.clone();
                runtime.spawn(async move {
                    let _permit = match permits.acquire_owned().await {
                        Ok(permit) => permit,
                        Err(_) => return,
                    };
                    let result: Result<_> = async {
                        if generation != current.load(std::sync::atomic::Ordering::Relaxed) {
                            return Ok(None);
                        }
                        let size = reader.size();
                        if size > 16 * 1024 * 1024 {
                            reader.close().await?;
                            return Ok(None);
                        }
                        let data = reader.read_at(0, size).await?;
                        reader.close().await?;
                        if generation != current.load(std::sync::atomic::Ordering::Relaxed) {
                            return Ok(None);
                        }
                        let pixels = tokio::task::spawn_blocking(move || -> Result<_> {
                            let mut decoder = image::ImageReader::new(std::io::Cursor::new(data))
                                .with_guessed_format()?;
                            let mut limits = image::Limits::default();
                            limits.max_alloc = Some(64 * 1024 * 1024);
                            limits.max_image_width = Some(8192);
                            limits.max_image_height = Some(8192);
                            decoder.limits(limits);
                            let pixels = decoder
                                .decode()?
                                .resize(96, 96, image::imageops::FilterType::Triangle)
                                .to_rgba8();
                            Ok((pixels.width(), pixels.height(), pixels.into_raw()))
                        })
                        .await??;
                        Ok(Some(pixels))
                    }
                    .await;
                    pending.lock().unwrap().remove(&key);
                    if let Ok(Some((width, height, bytes))) = result {
                        update(&weak, move |app| {
                            if app.get_listing_generation() != generation {
                                return;
                            }
                            let model = app.get_entries();
                            if let Some(mut item) = model.row_data(index) {
                                if item.name.as_str() != name {
                                    return;
                                }
                                let buffer =
                                    slint::SharedPixelBuffer::<slint::Rgba8Pixel>::clone_from_slice(
                                        &bytes, width, height,
                                    );
                                item.thumbnail = slint::Image::from_rgba8(buffer);
                                item.has_thumbnail = true;
                                model.set_row_data(index, item);
                            }
                        });
                    }
                });
                continue;
            }
            let preview_name = match &cmd {
                Command::Preview(name) => Some(name.clone()),
                _ => None,
            };
            let result = runtime.block_on(run_command(cmd, &mut state, &weak, &mut history));
            if let Err(e) = result {
                let message = format!("操作失败：{e:#}");
                update(&weak, move |app| {
                    app.set_status(message.clone().into());
                    if preview_name.as_deref() == Some(app.get_selected().as_str()) {
                        app.set_preview_text(message.into());
                        app.set_showing_image(false);
                    }
                });
            }
            update(&weak, |app| app.set_busy(false));
        }
    });
    {
        let current_generation = current_generation.clone();
        app.on_generation_changed(move |generation| {
            current_generation.store(generation, std::sync::atomic::Ordering::Relaxed);
        });
    }
    {
        let w = app.as_weak();
        app.on_sort_changed(move || {
            if let Some(app) = w.upgrade() {
                sort_entries(&app);
            }
        });
    }
    let send = |cmd: Command, app: &App, tx: &mpsc::Sender<Command>| {
        app.set_busy(true);
        app.set_status("正在处理…".into());
        if tx.send(cmd).is_err() {
            app.set_busy(false);
            app.set_status("工作线程已停止".into());
        }
    };
    {
        let w = app.as_weak();
        let tx = tx.clone();
        app.on_connect_request(move |s, sh, u, p| {
            if let Some(a) = w.upgrade() {
                send(
                    Command::Connect(s.to_string(), sh.to_string(), u.to_string(), p.to_string()),
                    &a,
                    &tx,
                );
            }
        });
    }
    {
        let w = app.as_weak();
        let tx = tx.clone();
        app.on_select_history(move |index| {
            if let Some(a) = w.upgrade() {
                if let Ok(index) = usize::try_from(index) {
                    send(Command::SelectHistory(index), &a, &tx);
                }
            }
        });
    }
    {
        let w = app.as_weak();
        let tx = tx.clone();
        app.on_delete_history(move |index| {
            if let Some(a) = w.upgrade() {
                if let Ok(index) = usize::try_from(index) {
                    send(Command::DeleteHistory(index), &a, &tx);
                }
            }
        });
    }
    {
        let w = app.as_weak();
        let tx = tx.clone();
        app.on_thumbnail_request(move |name, index, generation| {
            if let Some(a) = w.upgrade() {
                let _ = tx.send(Command::Thumbnail(
                    name.to_string(),
                    a.get_current_path().to_string(),
                    index as usize,
                    generation,
                ));
            }
        });
    }
    {
        let w = app.as_weak();
        let tx = tx.clone();
        app.on_navigate(move |name| {
            if let Some(a) = w.upgrade() {
                send(Command::Navigate(name.to_string()), &a, &tx);
            }
        });
    }
    {
        let w = app.as_weak();
        let tx = tx.clone();
        app.on_upload(move || {
            if let Some(a) = w.upgrade() {
                if let Some(path) = rfd::FileDialog::new().pick_file() {
                    send(Command::Upload(path), &a, &tx);
                }
            }
        });
    }
    {
        let w = app.as_weak();
        let tx = tx.clone();
        app.on_download(move |name| {
            if let Some(a) = w.upgrade() {
                if let Some(path) = rfd::FileDialog::new()
                    .set_file_name(name.as_str())
                    .save_file()
                {
                    send(Command::Download(name.to_string(), path), &a, &tx);
                }
            }
        });
    }
    {
        let w = app.as_weak();
        let tx = tx.clone();
        app.on_preview(move |name| {
            if let Some(a) = w.upgrade() {
                a.set_preview_text("正在加载预览…".into());
                a.set_preview_image(Default::default());
                a.set_showing_image(false);
                send(Command::Preview(name.to_string()), &a, &tx);
            }
        });
    }
    app.run()?;
    Ok(())
}

#[cfg(test)]
mod listing_tests {
    use super::*;

    #[test]
    fn sort_uses_numeric_keys_and_keeps_directories_first() {
        let entry = |name: &str, directory, size: u64, modified: u64| Entry {
            name: name.into(),
            directory,
            size_key: format!("{size:020}").into(),
            modified_key: format!("{modified:020}").into(),
            ..Default::default()
        };
        let mut items = vec![
            entry("big", false, 100, 2),
            entry("small", false, 9, 10),
            entry("dir", true, 0, 0),
        ];
        items.sort_by(|a, b| compare_entries(a, b, 2, false));
        assert_eq!(
            items.iter().map(|e| e.name.as_str()).collect::<Vec<_>>(),
            ["dir", "small", "big"]
        );
        items.sort_by(|a, b| compare_entries(a, b, 3, true));
        assert_eq!(
            items.iter().map(|e| e.name.as_str()).collect::<Vec<_>>(),
            ["dir", "small", "big"]
        );
    }
}
