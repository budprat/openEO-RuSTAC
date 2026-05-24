//! **orbit-cache** — content-addressed cache for downloaded assets.
//!
//! Layout:
//! - [`key_for`] / [`Stats`] — primitives, no I/O.
//! - [`FileCache`] — on-disk LRU with content-addressed paths, single-Mutex
//!   inner state (poison-tolerant), Prometheus-friendly stat counters.
//!
//! Will gain a [`moka`] in-memory tier in a subsequent iteration.

#![cfg_attr(test, allow(clippy::unwrap_used, clippy::expect_used))]
#![cfg_attr(not(test), forbid(unsafe_code))]
#![warn(missing_docs)]

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Mutex;
use std::time::{SystemTime, UNIX_EPOCH};

use sha2::{Digest, Sha256};

// ─────────────────────────────────────────────────────────────────────
// key_for / Stats (existing primitives)
// ─────────────────────────────────────────────────────────────────────

/// Compute the cache key for a source URL (SHA-256 hex, 64 chars).
#[must_use]
pub fn key_for(url: &str) -> String {
    let mut h = Sha256::new();
    h.update(url.as_bytes());
    let digest = h.finalize();
    digest.iter().map(|b| format!("{b:02x}")).collect()
}

/// Atomic counter set for cache observability.
#[derive(Debug, Default)]
pub struct Stats {
    hits: AtomicU64,
    misses: AtomicU64,
    bytes_in: AtomicU64,
    bytes_out: AtomicU64,
}

impl Stats {
    /// Construct an empty stats register.
    #[must_use]
    pub const fn new() -> Self {
        Self {
            hits: AtomicU64::new(0),
            misses: AtomicU64::new(0),
            bytes_in: AtomicU64::new(0),
            bytes_out: AtomicU64::new(0),
        }
    }

    /// Increment hit count by 1 and bytes-out by `n`.
    pub fn record_hit(&self, n: u64) {
        self.hits.fetch_add(1, Ordering::Relaxed);
        self.bytes_out.fetch_add(n, Ordering::Relaxed);
    }
    /// Increment miss count by 1 and bytes-in by `n`.
    pub fn record_miss(&self, n: u64) {
        self.misses.fetch_add(1, Ordering::Relaxed);
        self.bytes_in.fetch_add(n, Ordering::Relaxed);
    }
    /// Current hit count.
    pub fn hits(&self) -> u64 { self.hits.load(Ordering::Relaxed) }
    /// Current miss count.
    pub fn misses(&self) -> u64 { self.misses.load(Ordering::Relaxed) }
    /// Bytes served from the cache so far.
    pub fn bytes_out(&self) -> u64 { self.bytes_out.load(Ordering::Relaxed) }
    /// Bytes ingested into the cache so far.
    pub fn bytes_in(&self) -> u64 { self.bytes_in.load(Ordering::Relaxed) }
    /// Hit-ratio in [0.0, 1.0]; 0 when no requests yet.
    #[must_use]
    pub fn hit_ratio(&self) -> f64 {
        let h = self.hits() as f64;
        let m = self.misses() as f64;
        if h + m > 0.0 { h / (h + m) } else { 0.0 }
    }
}

// ─────────────────────────────────────────────────────────────────────
// FileCache
// ─────────────────────────────────────────────────────────────────────

/// One cached file entry.
#[derive(Debug, Clone)]
pub struct CacheEntry {
    /// Local on-disk path holding the cached content.
    pub path: PathBuf,
    /// Size on disk in bytes.
    pub size_bytes: u64,
    /// UNIX epoch seconds when the entry was last accessed (for LRU).
    pub last_access: u64,
}

/// Snapshot of cache state.
#[derive(Debug, Default, Clone)]
pub struct CacheSnapshot {
    /// Total number of entries currently cached.
    pub entries: usize,
    /// Total bytes used by cached files.
    pub total_bytes: u64,
    /// Cache hits since open.
    pub hits: u64,
    /// Cache misses since open.
    pub misses: u64,
}

#[derive(Debug, Default)]
struct Inner {
    index: HashMap<String, CacheEntry>,
    hits: u64,
    misses: u64,
}

/// On-disk LRU file cache keyed by SHA-256 of source URL.
///
/// State is held behind a single `Mutex<Inner>` with poison recovery —
/// concurrent get/insert never deadlocks and a panic in one task doesn't
/// kill the cache.
pub struct FileCache {
    root: PathBuf,
    max_bytes: u64,
    inner: Mutex<Inner>,
}

fn lock_inner(m: &Mutex<Inner>) -> std::sync::MutexGuard<'_, Inner> {
    m.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
}

fn now_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

impl FileCache {
    /// Open or create a cache rooted at `root`. `max_bytes` is a soft size
    /// cap; pass `0` for unlimited. Existing files in the directory are
    /// rescanned so the cache resumes across process restarts.
    pub fn open(root: impl AsRef<Path>, max_bytes: u64) -> std::io::Result<Self> {
        let root = root.as_ref().to_path_buf();
        std::fs::create_dir_all(&root)?;
        let mut index = HashMap::new();
        for entry in std::fs::read_dir(&root)? {
            let entry = entry?;
            let meta = entry.metadata()?;
            if meta.is_file() {
                let path = entry.path();
                if let Some(key) = path.file_name().and_then(|n| n.to_str()) {
                    let last_access = meta
                        .accessed()
                        .ok()
                        .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
                        .map(|d| d.as_secs())
                        .unwrap_or(0);
                    index.insert(
                        key.to_string(),
                        CacheEntry {
                            path: path.clone(),
                            size_bytes: meta.len(),
                            last_access,
                        },
                    );
                }
            }
        }
        Ok(Self {
            root,
            max_bytes,
            inner: Mutex::new(Inner { index, hits: 0, misses: 0 }),
        })
    }

    /// Compute the cache key for a URL (delegates to module fn).
    #[must_use]
    pub fn key_for(url: &str) -> String { key_for(url) }

    /// Look up a URL. Returns `Some(path)` on hit, `None` on miss. Updates
    /// LRU access time atomically.
    pub fn get(&self, url: &str) -> Option<PathBuf> {
        let key = key_for(url);
        let mut g = lock_inner(&self.inner);
        if let Some(entry) = g.index.get_mut(&key) {
            entry.last_access = now_secs();
            let path = entry.path.clone();
            g.hits += 1;
            Some(path)
        } else {
            g.misses += 1;
            None
        }
    }

    /// Insert a file into the cache. Copies `local_src` into the cache
    /// dir (so the source is untouched) and triggers eviction if needed.
    pub fn insert(&self, url: &str, local_src: &Path) -> std::io::Result<PathBuf> {
        let key = key_for(url);
        let dst = self.root.join(&key);
        std::fs::copy(local_src, &dst)?;
        let size = std::fs::metadata(&dst)?.len();
        {
            let mut g = lock_inner(&self.inner);
            g.index.insert(
                key,
                CacheEntry {
                    path: dst.clone(),
                    size_bytes: size,
                    last_access: now_secs(),
                },
            );
        }
        self.evict_if_needed();
        Ok(dst)
    }

    /// LRU eviction down to `max_bytes`. No-op if `max_bytes == 0`.
    fn evict_if_needed(&self) {
        if self.max_bytes == 0 { return; }
        let mut g = lock_inner(&self.inner);
        loop {
            let total: u64 = g.index.values().map(|e| e.size_bytes).sum();
            if total <= self.max_bytes { break; }
            let lru_key = match g
                .index
                .iter()
                .min_by_key(|(_, e)| e.last_access)
                .map(|(k, _)| k.clone())
            {
                Some(k) => k,
                None => break,
            };
            if let Some(entry) = g.index.remove(&lru_key) {
                let _ = std::fs::remove_file(&entry.path);
            }
        }
    }

    /// Snapshot of current cache state.
    pub fn snapshot(&self) -> CacheSnapshot {
        let g = lock_inner(&self.inner);
        CacheSnapshot {
            entries: g.index.len(),
            total_bytes: g.index.values().map(|e| e.size_bytes).sum(),
            hits: g.hits,
            misses: g.misses,
        }
    }

    /// Drop all entries (deletes files on disk).
    pub fn clear(&self) -> std::io::Result<()> {
        let mut g = lock_inner(&self.inner);
        for (_, entry) in g.index.drain() {
            let _ = std::fs::remove_file(&entry.path);
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn key_for_is_deterministic_64char_hex() {
        let k = key_for("https://example.com/a.tif");
        assert_eq!(k.len(), 64);
        assert_eq!(k, key_for("https://example.com/a.tif"));
    }

    #[test]
    fn key_for_distinguishes_different_urls() {
        assert_ne!(key_for("a"), key_for("b"));
    }

    #[test]
    fn stats_default_is_zero() {
        let s = Stats::new();
        assert_eq!(s.hits(), 0);
        assert_eq!(s.misses(), 0);
        assert_eq!(s.hit_ratio(), 0.0);
    }

    #[test]
    fn record_hit_and_miss_count_correctly() {
        let s = Stats::new();
        s.record_hit(100);
        s.record_hit(50);
        s.record_miss(200);
        assert_eq!(s.hits(), 2);
        assert_eq!(s.misses(), 1);
        assert_eq!(s.bytes_out(), 150);
        assert_eq!(s.bytes_in(), 200);
        assert!((s.hit_ratio() - 2.0 / 3.0).abs() < 1e-9);
    }

    // ── FileCache ───────────────────────────────────────────────────

    #[test]
    fn open_creates_dir_starts_empty() {
        let dir = tempfile::tempdir().unwrap();
        let cache = FileCache::open(dir.path().join("cache"), 0).unwrap();
        let s = cache.snapshot();
        assert_eq!(s.entries, 0);
        assert_eq!(s.hits, 0);
    }

    #[test]
    fn insert_then_get_returns_cached_path() {
        let dir = tempfile::tempdir().unwrap();
        let cache = FileCache::open(dir.path(), 0).unwrap();
        let src = dir.path().join("source.tif");
        std::fs::write(&src, b"fake tiff bytes").unwrap();

        let url = "https://example.com/foo.tif";
        let cached = cache.insert(url, &src).unwrap();
        assert!(cached.exists());

        let got = cache.get(url).expect("hit");
        assert_eq!(got, cached);

        let s = cache.snapshot();
        assert_eq!(s.entries, 1);
        assert_eq!(s.hits, 1);
        assert_eq!(s.misses, 0);
    }

    #[test]
    fn miss_increments_miss_counter() {
        let dir = tempfile::tempdir().unwrap();
        let cache = FileCache::open(dir.path(), 0).unwrap();
        assert!(cache.get("https://example.com/nope").is_none());
        assert_eq!(cache.snapshot().misses, 1);
    }

    #[test]
    fn lru_eviction_drops_least_recent() {
        let dir = tempfile::tempdir().unwrap();
        let cache = FileCache::open(dir.path(), 1024).unwrap();
        let big = vec![0u8; 600];
        let src_a = dir.path().join("a.bin");
        let src_b = dir.path().join("b.bin");
        std::fs::write(&src_a, &big).unwrap();
        std::fs::write(&src_b, &big).unwrap();
        cache.insert("http://a", &src_a).unwrap();
        std::thread::sleep(std::time::Duration::from_secs(1));
        cache.insert("http://b", &src_b).unwrap();
        let s = cache.snapshot();
        assert!(s.entries <= 1);
        assert!(s.total_bytes <= 1024);
        // Newer entry should still be there.
        assert!(cache.get("http://b").is_some());
    }

    #[test]
    fn reopen_resumes_from_disk() {
        let dir = tempfile::tempdir().unwrap();
        let src = dir.path().join("source.tif");
        std::fs::write(&src, b"persistent bytes").unwrap();
        {
            let cache = FileCache::open(dir.path().join("cache"), 0).unwrap();
            cache.insert("http://x", &src).unwrap();
        }
        let cache2 = FileCache::open(dir.path().join("cache"), 0).unwrap();
        assert_eq!(cache2.snapshot().entries, 1);
        assert!(cache2.get("http://x").is_some());
    }

    #[test]
    fn clear_removes_all_entries() {
        let dir = tempfile::tempdir().unwrap();
        let cache = FileCache::open(dir.path(), 0).unwrap();
        let src = dir.path().join("source.tif");
        std::fs::write(&src, b"x").unwrap();
        cache.insert("http://a", &src).unwrap();
        cache.insert("http://b", &src).unwrap();
        assert_eq!(cache.snapshot().entries, 2);
        cache.clear().unwrap();
        assert_eq!(cache.snapshot().entries, 0);
    }

    #[test]
    fn concurrent_get_insert_does_not_deadlock() {
        use std::sync::Arc;
        let dir = tempfile::tempdir().unwrap();
        let cache = Arc::new(FileCache::open(dir.path(), 0).unwrap());
        let src = dir.path().join("source.tif");
        std::fs::write(&src, b"x").unwrap();
        cache.insert("http://seed", &src).unwrap();

        let mut handles = vec![];
        for t in 0..8 {
            let cache = cache.clone();
            let dir = dir.path().to_path_buf();
            handles.push(std::thread::spawn(move || {
                for i in 0..16 {
                    let _ = cache.get("http://seed");
                    let s = dir.join(format!("t{t}_i{i}.bin"));
                    std::fs::write(&s, b"y").unwrap();
                    cache.insert(&format!("t{t}://{i}"), &s).unwrap();
                    let _ = cache.snapshot();
                }
            }));
        }
        for h in handles {
            h.join().unwrap();
        }
        let s = cache.snapshot();
        assert!(s.entries >= 1);
        assert!(s.hits + s.misses >= 8 * 16);
    }
}
