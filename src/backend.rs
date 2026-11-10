use anyhow::{bail, Result};
use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use std::path::Path;

/// All paths are UTF-8, relative to the configured remote root, slash-separated.
/// The root is the empty string. Backends must validate decoded server paths too.
pub fn normalize(path: &str) -> Result<String> {
    if path.contains('\\') || path.contains('\0') || path.starts_with('/') {
        bail!("remote path must be relative and use forward slashes");
    }
    let mut parts = Vec::new();
    for part in path.split('/') {
        match part {
            "" | "." => (),
            ".." => bail!("parent traversal is not allowed"),
            _ => parts.push(part),
        }
    }
    Ok(parts.join("/"))
}

pub fn join(parent: &str, name: &str) -> Result<String> {
    let parent = normalize(parent)?;
    let name = normalize(name)?;
    if parent.is_empty() {
        return Ok(name);
    }
    if name.is_empty() {
        return Ok(parent);
    }
    Ok(format!("{parent}/{name}"))
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct Entry {
    pub path: String,
    pub is_dir: bool,
    pub size: u64,
    /// Unix milliseconds, if supplied by the server.
    pub modified: Option<i64>,
    pub etag: Option<String>,
}

impl Entry {
    pub fn same_revision(&self, other: &Self) -> bool {
        if self.is_dir != other.is_dir || self.size != other.size {
            return false;
        }
        match (&self.etag, &other.etag) {
            (Some(a), Some(b)) => a == b,
            (None, None) => self.modified.is_some() && self.modified == other.modified,
            _ => false,
        }
    }
}

#[derive(Debug, Clone)]
pub enum WriteCondition {
    /// Only explicit user overwrite operations may use this.
    Any,
    CreateNew,
    Match(Entry),
}

#[derive(Debug, Clone, Serialize)]
pub struct Capabilities {
    pub protocol: &'static str,
    pub conditional_write: bool,
    pub server_copy: bool,
    pub locking: bool,
    pub notes: Vec<String>,
}
#[derive(Debug,Clone,Serialize,Deserialize)]
pub struct ChangeEvent {pub path:String,pub kind:String}

#[async_trait]
pub trait Backend: Send + Sync {
    fn capabilities(&self) -> Capabilities;
    fn diagnostics(&self)->serde_json::Value {serde_json::json!({})}
    /// Return a validated content hash only for a protocol token that embeds it.
    fn content_sha256(&self, _entry: &Entry) -> Option<String> {
        None
    }
    /// Read-only check before sync filters an internal recovery receipt. An
    /// unfinished namespace replacement must never be inferred as user deletion.
    async fn check_sync_recovery(&self, _receipt: &str) -> Result<()> {
        Ok(())
    }
    async fn change_feed(&self,_path:&str,_recursive:bool,_cancel:tokio_util::sync::CancellationToken)->Result<tokio::sync::mpsc::Receiver<Result<ChangeEvent>>> {
        bail!("server change notifications are unavailable for this backend")
    }
    async fn stat(&self, path: &str) -> Result<Option<Entry>>;
    /// Metadata-only view for on-demand enumeration; tokens may be weak and
    /// must never be used as content-write guards without a verified snapshot.
    async fn stat_metadata(&self,path:&str)->Result<Option<Entry>> {self.stat(path).await}
    /// Immediate children only. Each entry has a complete root-relative path.
    async fn list(&self, path: &str) -> Result<Vec<Entry>>;
    async fn list_metadata(&self,path:&str)->Result<Vec<Entry>> {self.list(path).await}
    /// Stream to a caller-owned temporary path. Must not follow local symlinks.
    async fn download(&self, remote: &str, local: &Path) -> Result<()>;
    /// Use an atomic read precondition where the protocol supports it. Callers
    /// still verify metadata after transfer; the default has no atomic guard.
    async fn download_checked(&self, remote: &str, local: &Path, _expected: &Entry) -> Result<()> {
        self.download(remote, local).await
    }
    /// Append a verified range to an existing regular partial file of exactly
    /// `offset` bytes. Retain and fsync completed bytes on interruption. Never
    /// truncate/restart implicitly if the server revision/range does not match.
    async fn download_resume(
        &self,
        _remote: &str,
        _local: &Path,
        _expected: &Entry,
        _offset: u64,
    ) -> Result<()> {
        bail!("verified ranged downloads are unavailable for this backend")
    }
    async fn upload(&self, local: &Path, remote: &str, condition: WriteCondition) -> Result<Entry>;
    /// Resume an immutable local snapshot using a backend-owned durable JSON
    /// checkpoint. Persist the session before creating remote staging objects;
    /// validate checkpoint identity/source/revision, never append to a user file.
    async fn upload_resume(
        &self,
        _local: &Path,
        _remote: &str,
        _condition: WriteCondition,
        _checkpoint: &Path,
    ) -> Result<Entry> {
        bail!("resumable uploads require server/protocol support; no upload was started")
    }
    async fn mkdir(&self, path: &str) -> Result<()>;
    async fn remove(&self, path: &str, recursive: bool) -> Result<()>;
    async fn remove_checked(&self, _path: &str, _expected: &Entry) -> Result<()> {
        bail!("server-side conditional deletion is unavailable; remote file was preserved")
    }
    async fn rename(&self, from: &str, to: &str, overwrite: bool) -> Result<()>;
    /// Move exactly the guarded source to an absent destination. The source
    /// revision must be checked by the server/on the same protected SMB handle.
    /// A transport error may mean publication completed; callers journal intent.
    async fn rename_checked(&self, _from:&str, _to:&str, _expected:&Entry)->Result<Entry> {
        bail!("unsupported: server-enforced guarded rename is unavailable")
    }
    /// Reconcile a journalled move without adopting an unrelated replacement.
    /// Directories also require a separately verified complete child manifest.
    fn namespace_matches(&self,expected:&Entry,actual:&Entry)->bool{expected.same_revision(actual)}
    async fn copy(&self, from: &str, to: &str, overwrite: bool) -> Result<()>;
    async fn lock(&self, _path: &str, _seconds: u32) -> Result<String> {
        bail!("this backend does not support WebDAV locks")
    }
    async fn unlock(&self, _path: &str, _token: &str) -> Result<()> {
        bail!("this backend does not support WebDAV locks")
    }
}

pub async fn walk(backend: &dyn Backend, root: &str) -> Result<Vec<Entry>> {
    let root = normalize(root)?;
    let mut dirs = vec![root];
    let mut result = Vec::new();
    let mut seen = std::collections::HashSet::new();
    while let Some(dir) = dirs.pop() {
        if !seen.insert(dir.clone()) {
            bail!("remote directory cycle at {dir}");
        }
        for entry in backend.list(&dir).await? {
            let p = normalize(&entry.path)?;
            let parent = p.rsplit_once('/').map(|x| x.0).unwrap_or("");
            if parent != dir || p == dir {
                bail!("server returned a child outside requested directory");
            }
            if entry.is_dir {
                dirs.push(p);
            }
            result.push(entry);
        }
    }
    result.sort_by(|a, b| a.path.cmp(&b.path));
    Ok(result)
}

pub async fn mkdir_all(backend: &dyn Backend, path: &str) -> Result<()> {
    let path = normalize(path)?;
    let mut current = String::new();
    for part in path.split('/').filter(|s| !s.is_empty()) {
        current = join(&current, part)?;
        match backend.stat(&current).await? {
            Some(e) if e.is_dir => (),
            Some(_) => bail!("{current} is a file"),
            None => backend.mkdir(&current).await?,
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn remote_paths_are_contained() {
        for p in ["../escape", "a/../../escape", "/tmp/a", "a\\b", "a\0b"] {
            assert!(normalize(p).is_err());
        }
        assert_eq!(normalize("a/./b//").unwrap(), "a/b");
        assert!(join("safe", "/escape").is_err());
    }
}
