use anyhow::{Context, Result, bail};
use slint::{ComponentHandle, ModelRc, VecModel};
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
async fn listing(c: &mut Connection) -> Result<Vec<Entry>> {
    let mut items = c.client.list_directory(&mut c.tree, &c.path).await?;
    items.retain(|e| e.name != "." && e.name != "..");
    items.sort_by(|a, b| {
        b.is_directory
            .cmp(&a.is_directory)
            .then_with(|| a.name.to_lowercase().cmp(&b.name.to_lowercase()))
    });
    Ok(items
        .into_iter()
        .map(|e| Entry {
            name: e.name.into(),
            directory: e.is_directory,
            detail: if e.is_directory {
                "文件夹".into()
            } else {
                format_size(e.size).into()
            },
        })
        .collect())
}
fn show_listing(ui: &slint::Weak<App>, c: &Connection, entries: Vec<Entry>) {
    let path = c.path.clone();
    update(ui, move |app| {
        app.set_current_path(path.into());
        app.set_entries(ModelRc::new(VecModel::from(entries)));
        app.set_selected("".into());
        app.set_connected(true);
        app.set_preview_text("选择文件后点击预览".into());
        app.set_showing_image(false);
    });
}
async fn run_command(
    cmd: Command,
    state: &mut Option<Connection>,
    ui: &slint::Weak<App>,
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
            *state = Some(c);
            status(ui, "连接成功".into());
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
    thread::spawn(move || {
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .build()
            .expect("创建运行时失败");
        let mut state = None;
        for cmd in rx {
            let result = runtime.block_on(run_command(cmd, &mut state, &weak));
            if let Err(e) = result {
                status(&weak, format!("操作失败：{e:#}"));
            }
            update(&weak, |app| app.set_busy(false));
        }
    });
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
                send(Command::Preview(name.to_string()), &a, &tx);
            }
        });
    }
    app.run()?;
    Ok(())
}
