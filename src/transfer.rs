//! 接收只写用户选择的位置；临时文件由所有权管理，失败或取消自动清理。
use std::{
    fs::File,
    io,
    path::{Path, PathBuf},
};

pub struct ReceiveTarget {
    pub file: File,
    temporary: Option<tempfile::NamedTempFile>,
    destination: Option<PathBuf>,
    cleanup: Option<Box<dyn FnOnce() + Send>>,
}
impl std::fmt::Debug for ReceiveTarget {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("ReceiveTarget")
    }
}
impl ReceiveTarget {
    pub fn path(path: &Path, size: u64) -> io::Result<Self> {
        let parent = path
            .parent()
            .filter(|p| !p.as_os_str().is_empty())
            .unwrap_or(Path::new("."));
        if fs2::available_space(parent)? < size.saturating_add(16 * 1024 * 1024) {
            return Err(io::Error::other("磁盘空间不足，请选择其他位置"));
        }
        let temporary = tempfile::Builder::new()
            .prefix(".spark-")
            .suffix(".part")
            .tempfile_in(parent)?;
        Ok(Self {
            file: temporary.reopen()?,
            temporary: Some(temporary),
            destination: Some(path.to_owned()),
            cleanup: None,
        })
    }
    pub fn document(file: File, cleanup: impl FnOnce() + Send + 'static) -> Self {
        Self {
            file,
            temporary: None,
            destination: None,
            cleanup: Some(Box::new(cleanup)),
        }
    }
    pub fn commit(mut self) -> io::Result<()> {
        self.file.sync_all()?;
        if let Some(temporary) = self.temporary.take() {
            temporary
                .persist(self.destination.as_ref().unwrap())
                .map_err(|e| e.error)?;
        }
        self.cleanup = None;
        Ok(())
    }
}
impl Drop for ReceiveTarget {
    fn drop(&mut self) {
        if let Some(cleanup) = self.cleanup.take() {
            cleanup();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;
    #[test]
    fn cancelled_and_failed_receives_preserve_existing_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("existing.txt");
        std::fs::write(&path, b"original").unwrap();
        let mut target = ReceiveTarget::path(&path, 3).unwrap();
        target.file.write_all(b"new").unwrap();
        drop(target);
        assert_eq!(std::fs::read(&path).unwrap(), b"original");
        assert_eq!(std::fs::read_dir(dir.path()).unwrap().count(), 1);
        let mut target = ReceiveTarget::path(&path, 3).unwrap();
        target.file.write_all(b"new").unwrap();
        target.commit().unwrap();
        assert_eq!(std::fs::read(path).unwrap(), b"new");
    }
}
