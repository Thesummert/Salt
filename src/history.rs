use anyhow::{Context, Result};
use directories::ProjectDirs;
use serde::{Deserialize, Serialize};
use std::{
    fs,
    io::Write,
    path::{Path, PathBuf},
};

const LIMIT: usize = 50;

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct HistoryEntry {
    pub server: String,
    pub share: String,
    pub username: String,
}

pub struct HistoryStore {
    path: PathBuf,
    pub entries: Vec<HistoryEntry>,
}

impl HistoryStore {
    pub fn load_default() -> Result<Self> {
        let dirs = ProjectDirs::from("", "", "salt-smb").context("无法确定配置目录")?;
        Self::load(dirs.config_dir().join("history.json"))
    }

    fn load(path: PathBuf) -> Result<Self> {
        let entries = match fs::read(&path) {
            Ok(bytes) => serde_json::from_slice(&bytes).context("历史记录格式无效")?,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Vec::new(),
            Err(e) => return Err(e).context("无法读取历史记录"),
        };
        Ok(Self { path, entries })
    }

    pub fn record(&mut self, entry: HistoryEntry) -> Result<()> {
        self.entries.retain(|old| old != &entry);
        self.entries.insert(0, entry);
        self.entries.truncate(LIMIT);
        self.save()
    }

    pub fn delete(&mut self, index: usize) -> Result<()> {
        if index < self.entries.len() {
            let removed = self.entries.remove(index);
            if let Err(error) = self.save() {
                self.entries.insert(index, removed);
                return Err(error);
            }
        }
        Ok(())
    }

    fn save(&self) -> Result<()> {
        let parent = self.path.parent().unwrap_or_else(|| Path::new("."));
        fs::create_dir_all(parent).context("无法创建配置目录")?;
        let mut file = tempfile::NamedTempFile::new_in(parent).context("无法创建历史记录文件")?;
        serde_json::to_writer_pretty(&mut file, &self.entries)?;
        file.write_all(b"\n")?;
        file.flush()?;
        file.persist(&self.path)
            .map_err(|e| e.error)
            .context("无法保存历史记录")?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(server: &str) -> HistoryEntry {
        HistoryEntry {
            server: server.into(),
            share: "共享".into(),
            username: "用户".into(),
        }
    }

    #[test]
    fn history_persists_deduplicates_and_deletes() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("history.json");
        let mut store = HistoryStore::load(path.clone()).unwrap();
        store.record(entry("a")).unwrap();
        store.record(entry("b")).unwrap();
        store.record(entry("a")).unwrap();
        let mut reloaded = HistoryStore::load(path.clone()).unwrap();
        assert_eq!(reloaded.entries, vec![entry("a"), entry("b")]);
        reloaded.delete(0).unwrap();
        assert_eq!(HistoryStore::load(path).unwrap().entries, vec![entry("b")]);
    }

    #[test]
    fn corrupt_history_is_not_silently_replaced() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("history.json");
        fs::write(&path, "not json").unwrap();
        assert!(HistoryStore::load(path.clone()).is_err());
        assert_eq!(fs::read_to_string(path).unwrap(), "not json");
    }

    #[test]
    fn history_is_bounded_and_does_not_store_passwords() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("history.json");
        let mut store = HistoryStore::load(path.clone()).unwrap();
        for i in 0..55 {
            store.record(entry(&i.to_string())).unwrap();
        }
        let saved = fs::read_to_string(path).unwrap();
        assert_eq!(store.entries.len(), LIMIT);
        assert!(!saved.contains("password"));
        assert_eq!(store.entries[0].server, "54");
    }
}
