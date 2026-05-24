//! Pluggable file storage for `/files/{user_id}/{path}` routes.
//!
//! `FileStore` is a tiny async trait sized for the needs of the openEO
//! file routes: put bytes, get bytes back, list, delete. Implementations
//! land out-of-tree:
//!
//! - [`InMemoryFileStore`] — sized by an upper bound, suitable for tests.
//! - (future) `ObjectStoreBackend` wrapping `object_store::ObjectStore`
//!   so the same routes target S3, GCS, Azure, or disk transparently.

use std::collections::BTreeMap;
use std::sync::Arc;
use std::sync::Mutex;

use async_trait::async_trait;
use bytes::Bytes;
use object_store::path::Path as ObjPath;
use object_store::{ObjectStore, ObjectStoreExt, PutPayload};
use thiserror::Error;
use futures::TryStreamExt;

/// Identity of one stored file.
#[derive(Clone, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct FileKey {
    /// openEO user id.
    pub user_id: String,
    /// User-visible path component.
    pub path: String,
}

impl FileKey {
    /// New key.
    pub fn new(user_id: impl Into<String>, path: impl Into<String>) -> Self {
        Self { user_id: user_id.into(), path: path.into() }
    }
}

/// Listing entry returned by [`FileStore::list`].
#[derive(Clone, Debug, PartialEq)]
pub struct FileEntry {
    /// Path component (relative to the user's root).
    pub path: String,
    /// Size in bytes.
    pub size: u64,
}

/// Errors a file store can surface.
#[derive(Debug, Error)]
pub enum FileError {
    /// The (user_id, path) tuple isn't stored.
    #[error("file not found")]
    NotFound,
    /// Path contained a disallowed character (e.g. "..").
    #[error("forbidden path: {0}")]
    Forbidden(String),
    /// Backend I/O failure.
    #[error("io error: {0}")]
    Io(String),
}

/// Async file storage surface.
#[async_trait]
pub trait FileStore: Send + Sync {
    /// Store `bytes` under `(user_id, path)`. Replaces existing content.
    async fn put(&self, key: &FileKey, bytes: Vec<u8>) -> Result<(), FileError>;
    /// Read the full file content. Streaming variants come later.
    async fn get(&self, key: &FileKey) -> Result<Vec<u8>, FileError>;
    /// Remove the file. No-op if absent.
    async fn delete(&self, key: &FileKey) -> Result<(), FileError>;
    /// List paths for the user.
    async fn list(&self, user_id: &str) -> Result<Vec<FileEntry>, FileError>;
}

/// In-memory file store backed by a `BTreeMap<FileKey, Vec<u8>>`.
#[derive(Debug, Default)]
pub struct InMemoryFileStore {
    inner: Mutex<BTreeMap<FileKey, Vec<u8>>>,
}

impl InMemoryFileStore {
    /// New empty store.
    #[must_use]
    pub fn new() -> Self { Self::default() }
}

/// Validate a path component. **Audit P1-6 fix**: rejects `..`,
/// absolute paths, `\\`, control chars, drive letters, leading `-`,
/// Windows-reserved chars, and single-`.` segments. Also percent-decodes
/// once and re-validates so `%2F`, `%5C`, `%00`, `..%c0%af` are all
/// caught.
pub fn validate_path(path: &str) -> Result<(), FileError> {
    // First-pass percent-decode so encoded traversal attempts are
    // normalised to their literal form.
    let decoded = percent_decode_once(path);
    let target = if decoded != path { decoded.as_str() } else { path };

    if target.is_empty() {
        return Err(FileError::Forbidden("empty path".into()));
    }
    if target.starts_with('/') || target.starts_with('\\') {
        return Err(FileError::Forbidden("absolute path".into()));
    }
    if target.starts_with('-') {
        return Err(FileError::Forbidden("leading dash (option-injection risk)".into()));
    }
    if target.starts_with('~') {
        return Err(FileError::Forbidden("leading tilde (home-dir injection)".into()));
    }
    // Drive letters: `C:`, `c:` etc.
    if let Some(c) = target.chars().nth(1) {
        if c == ':' && target.chars().next().is_some_and(|x| x.is_ascii_alphabetic()) {
            return Err(FileError::Forbidden("drive-letter prefix".into()));
        }
    }
    for c in target.chars() {
        if c.is_control() {
            return Err(FileError::Forbidden(format!("control char U+{:04X}", c as u32)));
        }
        if matches!(c, '\\' | ':' | '<' | '>' | '"' | '|' | '?' | '*' | '\0') {
            return Err(FileError::Forbidden(format!("disallowed char {c:?}")));
        }
    }
    for seg in target.split('/') {
        if seg == ".." || seg == "." || seg.is_empty() {
            return Err(FileError::Forbidden(path.into()));
        }
    }
    Ok(())
}

/// Single-pass percent-decode. Returns the original string if no `%`
/// escapes are present. Stops at invalid escapes (returns original).
fn percent_decode_once(s: &str) -> String {
    if !s.contains('%') {
        return s.to_string();
    }
    let bytes = s.as_bytes();
    let mut out: Vec<u8> = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' && i + 2 < bytes.len() {
            let hi = (bytes[i + 1] as char).to_digit(16);
            let lo = (bytes[i + 2] as char).to_digit(16);
            if let (Some(hi), Some(lo)) = (hi, lo) {
                out.push(((hi << 4) | lo) as u8);
                i += 3;
                continue;
            }
        }
        out.push(bytes[i]);
        i += 1;
    }
    // Decoded bytes may not be valid UTF-8 (e.g. `%c0%af`). Treat any
    // invalid sequence as a marker char that fails the control-char /
    // disallowed-char check downstream.
    String::from_utf8(out).unwrap_or_else(|_| "\\0invalid".to_string())
}

#[async_trait]
impl FileStore for InMemoryFileStore {
    async fn put(&self, key: &FileKey, bytes: Vec<u8>) -> Result<(), FileError> {
        validate_path(&key.path)?;
        let mut g = self.inner.lock().unwrap_or_else(|p| p.into_inner());
        g.insert(key.clone(), bytes);
        Ok(())
    }

    async fn get(&self, key: &FileKey) -> Result<Vec<u8>, FileError> {
        validate_path(&key.path)?;
        let g = self.inner.lock().unwrap_or_else(|p| p.into_inner());
        g.get(key).cloned().ok_or(FileError::NotFound)
    }

    async fn delete(&self, key: &FileKey) -> Result<(), FileError> {
        validate_path(&key.path)?;
        let mut g = self.inner.lock().unwrap_or_else(|p| p.into_inner());
        g.remove(key);
        Ok(())
    }

    async fn list(&self, user_id: &str) -> Result<Vec<FileEntry>, FileError> {
        let g = self.inner.lock().unwrap_or_else(|p| p.into_inner());
        Ok(g.iter()
            .filter(|(k, _)| k.user_id == user_id)
            .map(|(k, v)| FileEntry { path: k.path.clone(), size: v.len() as u64 })
            .collect())
    }
}

/// Pluggable backend that delegates to any `object_store::ObjectStore`.
///
/// Path layout: each file is keyed as `{user_id}/{path}` underneath the
/// configured store root. Construct with `new(store)` for an arbitrary
/// store, or with `local_disk(root_dir)` for the bundled disk backend.
pub struct ObjectStoreBackend {
    inner: Arc<dyn ObjectStore>,
}

impl std::fmt::Debug for ObjectStoreBackend {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ObjectStoreBackend").finish_non_exhaustive()
    }
}

impl ObjectStoreBackend {
    /// Wrap any `ObjectStore`.
    #[must_use]
    pub fn new(store: Arc<dyn ObjectStore>) -> Self { Self { inner: store } }

    /// Build a disk-backed instance rooted at `root_dir`.
    pub fn local_disk(root_dir: impl AsRef<std::path::Path>) -> Result<Self, FileError> {
        let store = object_store::local::LocalFileSystem::new_with_prefix(root_dir)
            .map_err(|e| FileError::Io(e.to_string()))?;
        Ok(Self { inner: Arc::new(store) })
    }

    fn key_to_path(key: &FileKey) -> Result<ObjPath, FileError> {
        validate_path(&key.path)?;
        let joined = format!("{}/{}", key.user_id, key.path);
        ObjPath::parse(&joined).map_err(|e| FileError::Forbidden(e.to_string()))
    }
}

#[async_trait]
impl FileStore for ObjectStoreBackend {
    async fn put(&self, key: &FileKey, bytes: Vec<u8>) -> Result<(), FileError> {
        let p = Self::key_to_path(key)?;
        let payload = PutPayload::from(Bytes::from(bytes));
        self.inner
            .put(&p, payload)
            .await
            .map(|_| ())
            .map_err(|e| FileError::Io(e.to_string()))
    }

    async fn get(&self, key: &FileKey) -> Result<Vec<u8>, FileError> {
        let p = Self::key_to_path(key)?;
        let result = self.inner.get(&p).await.map_err(|e| match e {
            object_store::Error::NotFound { .. } => FileError::NotFound,
            other => FileError::Io(other.to_string()),
        })?;
        let bytes = result
            .bytes()
            .await
            .map_err(|e| FileError::Io(e.to_string()))?;
        Ok(bytes.to_vec())
    }

    async fn delete(&self, key: &FileKey) -> Result<(), FileError> {
        let p = Self::key_to_path(key)?;
        match self.inner.delete(&p).await {
            Ok(_) | Err(object_store::Error::NotFound { .. }) => Ok(()),
            Err(e) => Err(FileError::Io(e.to_string())),
        }
    }

    async fn list(&self, user_id: &str) -> Result<Vec<FileEntry>, FileError> {
        let prefix = ObjPath::parse(user_id)
            .map_err(|e| FileError::Forbidden(e.to_string()))?;
        let stream = self.inner.list(Some(&prefix));
        let metas: Vec<_> = stream
            .try_collect()
            .await
            .map_err(|e| FileError::Io(e.to_string()))?;
        let prefix_str = format!("{}/", user_id);
        Ok(metas
            .into_iter()
            .filter_map(|m| {
                let s = m.location.to_string();
                s.strip_prefix(&prefix_str)
                    .map(|rest| FileEntry { path: rest.to_string(), size: m.size as u64 })
            })
            .collect())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test(flavor = "current_thread")]
    async fn put_then_get_roundtrips() {
        let s = InMemoryFileStore::new();
        let k = FileKey::new("alice", "data/scene.tif");
        s.put(&k, b"hello".to_vec()).await.unwrap();
        assert_eq!(s.get(&k).await.unwrap(), b"hello");
    }

    #[tokio::test(flavor = "current_thread")]
    async fn get_missing_returns_not_found() {
        let s = InMemoryFileStore::new();
        let k = FileKey::new("alice", "ghost.tif");
        assert!(matches!(s.get(&k).await, Err(FileError::NotFound)));
    }

    #[tokio::test(flavor = "current_thread")]
    async fn delete_is_idempotent() {
        let s = InMemoryFileStore::new();
        let k = FileKey::new("alice", "x");
        s.delete(&k).await.unwrap();
        s.put(&k, vec![1]).await.unwrap();
        s.delete(&k).await.unwrap();
        assert!(matches!(s.get(&k).await, Err(FileError::NotFound)));
    }

    #[tokio::test(flavor = "current_thread")]
    async fn list_filters_by_user() {
        let s = InMemoryFileStore::new();
        s.put(&FileKey::new("alice", "a"), vec![1, 2, 3]).await.unwrap();
        s.put(&FileKey::new("alice", "b"), vec![4]).await.unwrap();
        s.put(&FileKey::new("bob", "x"), vec![1; 100]).await.unwrap();
        let alice = s.list("alice").await.unwrap();
        assert_eq!(alice.len(), 2);
        assert!(alice.iter().any(|e| e.path == "a" && e.size == 3));
    }

    #[tokio::test(flavor = "current_thread")]
    async fn rejects_dotdot_traversal() {
        let s = InMemoryFileStore::new();
        let k = FileKey::new("alice", "../etc/passwd");
        assert!(matches!(s.put(&k, vec![]).await, Err(FileError::Forbidden(_))));
        assert!(matches!(s.get(&k).await, Err(FileError::Forbidden(_))));
    }

    #[tokio::test(flavor = "current_thread")]
    async fn rejects_absolute_path() {
        let s = InMemoryFileStore::new();
        let k = FileKey::new("alice", "/abs/path");
        assert!(matches!(s.put(&k, vec![]).await, Err(FileError::Forbidden(_))));
    }

    #[tokio::test(flavor = "current_thread")]
    async fn rejects_empty_path() {
        let s = InMemoryFileStore::new();
        let k = FileKey::new("alice", "");
        assert!(matches!(s.put(&k, vec![]).await, Err(FileError::Forbidden(_))));
    }

    // ---------- P1-6: hardened path validator ----------

    fn rejected(p: &str) -> bool {
        matches!(validate_path(p), Err(FileError::Forbidden(_)))
    }

    #[test]
    fn rejects_percent_encoded_traversal() {
        assert!(rejected("%2e%2e/passwd"));
        assert!(rejected("etc/%2e%2e/passwd"));
    }

    #[test]
    fn rejects_percent_encoded_slash() {
        assert!(rejected("..%2fetc%2fpasswd"));
    }

    #[test]
    fn rejects_backslash() {
        assert!(rejected("..\\etc\\passwd"));
        assert!(rejected("a\\b"));
    }

    #[test]
    fn rejects_nul_byte() {
        assert!(rejected("foo\0bar"));
        assert!(rejected("foo%00bar"));
    }

    #[test]
    fn rejects_unicode_overlong_traversal() {
        assert!(rejected("..%c0%afetc%c0%afpasswd"));
    }

    #[test]
    fn rejects_windows_drive_letter() {
        assert!(rejected("C:/Windows/system32"));
        assert!(rejected("d:secret"));
    }

    #[test]
    fn rejects_leading_dash() {
        assert!(rejected("-config"));
        assert!(rejected("--upload"));
    }

    #[test]
    fn rejects_leading_tilde() {
        assert!(rejected("~/passwd"));
    }

    #[test]
    fn rejects_windows_reserved_chars() {
        for s in &["a<b", "a>b", "a|b", "a\"b", "a?b", "a*b"] {
            assert!(rejected(s), "should reject: {s}");
        }
    }

    #[test]
    fn rejects_single_dot_segment() {
        assert!(rejected("./foo"));
        assert!(rejected("a/./b"));
    }

    #[test]
    fn accepts_normal_paths() {
        for s in &["foo.tif", "data/scene.tif", "alice-2024/result.png"] {
            assert!(!rejected(s), "should accept: {s}");
        }
    }

    #[test]
    fn percent_decode_round_trip_for_simple_cases() {
        assert_eq!(percent_decode_once("foo"), "foo");
        assert_eq!(percent_decode_once("a%20b"), "a b");
        assert_eq!(percent_decode_once("..%2fpasswd"), "../passwd");
        // Invalid byte sequence falls back to marker.
        let r = percent_decode_once("%c0%af");
        assert!(r.contains("\\0invalid"));
    }

    #[tokio::test(flavor = "current_thread")]
    async fn put_overwrites_existing() {
        let s = InMemoryFileStore::new();
        let k = FileKey::new("alice", "x");
        s.put(&k, vec![1]).await.unwrap();
        s.put(&k, vec![2, 2, 2]).await.unwrap();
        assert_eq!(s.get(&k).await.unwrap(), vec![2, 2, 2]);
    }

    // ------------------------------------------------------------------
    // ObjectStoreBackend — disk-backed integration tests using tempdir.
    // ------------------------------------------------------------------

    fn tempdir() -> std::path::PathBuf {
        let mut p = std::env::temp_dir();
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos();
        p.push(format!("orbit-openeo-test-{nanos}"));
        std::fs::create_dir_all(&p).unwrap();
        p
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn object_store_disk_roundtrips() {
        let dir = tempdir();
        let s = ObjectStoreBackend::local_disk(&dir).unwrap();
        let k = FileKey::new("alice", "scene.tif");
        s.put(&k, b"obj-store-bytes".to_vec()).await.unwrap();
        assert_eq!(s.get(&k).await.unwrap(), b"obj-store-bytes");
        std::fs::remove_dir_all(&dir).ok();
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn object_store_disk_get_missing_is_not_found() {
        let dir = tempdir();
        let s = ObjectStoreBackend::local_disk(&dir).unwrap();
        let k = FileKey::new("alice", "ghost.tif");
        assert!(matches!(s.get(&k).await, Err(FileError::NotFound)));
        std::fs::remove_dir_all(&dir).ok();
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn object_store_disk_delete_is_idempotent() {
        let dir = tempdir();
        let s = ObjectStoreBackend::local_disk(&dir).unwrap();
        let k = FileKey::new("alice", "x.bin");
        s.delete(&k).await.unwrap();
        s.put(&k, vec![1, 2, 3]).await.unwrap();
        s.delete(&k).await.unwrap();
        assert!(matches!(s.get(&k).await, Err(FileError::NotFound)));
        std::fs::remove_dir_all(&dir).ok();
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn object_store_disk_list_filters_by_user() {
        let dir = tempdir();
        let s = ObjectStoreBackend::local_disk(&dir).unwrap();
        s.put(&FileKey::new("alice", "a"), vec![1, 2, 3]).await.unwrap();
        s.put(&FileKey::new("alice", "b"), vec![4]).await.unwrap();
        s.put(&FileKey::new("bob", "x"), vec![1; 100]).await.unwrap();
        let alice = s.list("alice").await.unwrap();
        assert_eq!(alice.len(), 2);
        let bob = s.list("bob").await.unwrap();
        assert_eq!(bob.len(), 1);
        std::fs::remove_dir_all(&dir).ok();
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn object_store_disk_rejects_dotdot() {
        let dir = tempdir();
        let s = ObjectStoreBackend::local_disk(&dir).unwrap();
        let k = FileKey::new("alice", "../etc/passwd");
        assert!(matches!(s.put(&k, vec![]).await, Err(FileError::Forbidden(_))));
        std::fs::remove_dir_all(&dir).ok();
    }
}
