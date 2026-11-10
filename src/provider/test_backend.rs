//! A deterministic storage peer backed only by a caller-owned temporary tree.
use super::*;
use async_trait::async_trait;
use std::sync::atomic::{AtomicUsize, Ordering};
pub(super) struct Disk {
    pub root: PathBuf,
    pub connections: AtomicUsize,
    pub removals: AtomicUsize,
    pub protocol: &'static str,
    pub conditional: bool,
}
impl Disk {
    pub fn new(root: PathBuf) -> Arc<Self> {
        fs::create_dir(&root).unwrap();
        Arc::new(Self {
            root,
            connections: AtomicUsize::new(0),
            removals: AtomicUsize::new(0),
            protocol: "test",
            conditional: true,
        })
    }
    fn path(&self, path: &str) -> Result<PathBuf> {
        Ok(self.root.join(backend::normalize(path)?))
    }
}
#[async_trait]
impl Backend for Disk {
    fn capabilities(&self) -> backend::Capabilities {
        backend::Capabilities {
            protocol: self.protocol,
            conditional_write: self.conditional,
            server_copy: true,
            locking: false,
            notes: vec![],
        }
    }
    async fn stat(&self, path: &str) -> Result<Option<Entry>> {
        let local = self.path(path)?;
        let meta = match fs::metadata(&local) {
            Ok(meta) => meta,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(error) => return Err(error.into()),
        };
        Ok(Some(Entry {
            path: path.into(),
            is_dir: meta.is_dir(),
            size: if meta.is_dir() { 0 } else { meta.len() },
            modified: None,
            etag: Some(if meta.is_dir() {
                "\"directory\"".into()
            } else {
                format!("\"{}\"", state::fingerprint(&local)?)
            }),
        }))
    }
    async fn list(&self, path: &str) -> Result<Vec<Entry>> {
        let mut entries = vec![];
        for entry in fs::read_dir(self.path(path)?)? {
            let entry = entry?;
            let remote = backend::join(
                path,
                entry.file_name().to_str().context("fixture filename")?,
            )?;
            entries.push(self.stat(&remote).await?.context("fixture vanished")?);
        }
        entries.sort_by(|a, b| a.path.cmp(&b.path));
        Ok(entries)
    }
    async fn download(&self, path: &str, local: &Path) -> Result<()> {
        use std::io::Write;
        let mut output = fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(local)?;
        output.write_all(&fs::read(self.path(path)?)?)?;
        Ok(())
    }
    async fn upload(&self, local: &Path, path: &str, condition: WriteCondition) -> Result<Entry> {
        let previous = self.stat(path).await?;
        match condition {
            WriteCondition::CreateNew if previous.is_some() => bail!("fixture create conflict"),
            WriteCondition::Match(expected)
                if !previous.is_some_and(|entry| entry.same_revision(&expected)) =>
            {
                bail!("fixture revision conflict")
            }
            _ => (),
        };
        fs::write(self.path(path)?, fs::read(local)?)?;
        self.stat(path).await?.context("fixture upload missing")
    }
    async fn remove_checked(&self, path: &str, expected: &Entry) -> Result<()> {
        if !self
            .stat(path)
            .await?
            .is_some_and(|entry| entry.same_revision(expected))
        {
            bail!("version_conflict: fixture deletion guard");
        }
        self.removals.fetch_add(1, Ordering::SeqCst);
        self.remove(path, false).await
    }
    async fn mkdir(&self, path: &str) -> Result<()> {
        fs::create_dir(self.path(path)?)?;
        Ok(())
    }
    async fn remove(&self, path: &str, recursive: bool) -> Result<()> {
        let path = self.path(path)?;
        if path.is_dir() {
            if recursive {
                fs::remove_dir_all(path)?
            } else {
                fs::remove_dir(path)?
            }
        } else {
            fs::remove_file(path)?
        };
        Ok(())
    }
    async fn rename(&self, from: &str, to: &str, overwrite: bool) -> Result<()> {
        let target = self.path(to)?;
        if !overwrite && target.exists() {
            bail!("fixture destination exists");
        }
        fs::rename(self.path(from)?, target)?;
        Ok(())
    }
    async fn copy(&self, from: &str, to: &str, overwrite: bool) -> Result<()> {
        let target = self.path(to)?;
        if !overwrite && target.exists() {
            bail!("fixture destination exists");
        }
        fs::copy(self.path(from)?, target)?;
        Ok(())
    }
}
pub(super) struct Connected(pub Arc<Disk>);
#[async_trait]
impl Connector for Connected {
    async fn connect(
        &self,
        _: &config::Profile,
        _: &str,
        _: crate::transfer::TransferContext,
    ) -> Result<Arc<dyn Backend>> {
        self.0.connections.fetch_add(1, Ordering::SeqCst);
        Ok(self.0.clone())
    }
}
