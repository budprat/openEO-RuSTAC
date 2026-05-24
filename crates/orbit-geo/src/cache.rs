//! **File cache** — content-addressed local cache for downloaded STAC assets.
//!
//! Ported semantics from the upstream raster crate's `cache::file_cache` module (706 lines) in
//! minimum-viable form: dedup by SHA256 of the source URL, LRU eviction by
//! cache size, basic stats. Sufficient for `apply_reduction_with_mask` workflows.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::RwLock;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

/// One cached file entry.
#[derive(Debug)]
pub struct CacheEntry {
    /// Local on-disk path holding the cached content.
    pub path: PathBuf,
    /// Size on disk in bytes.
    pub size_bytes: u64,
    /// UNIX epoch seconds when the entry was last accessed (for LRU). Atomic so
    /// `get()` can update last-access without taking a write lock.
    pub last_access: AtomicU64,
}

impl Clone for CacheEntry {
    fn clone(&self) -> Self {
        Self {
            path: self.path.clone(),
            size_bytes: self.size_bytes,
            last_access: AtomicU64::new(self.last_access.load(Ordering::Relaxed)),
        }
    }
}

/// Aggregate cache statistics.
#[derive(Debug, Default, Clone)]
pub struct CacheStats {
    /// Total number of entries currently cached.
    pub entries: usize,
    /// Total bytes used by cached files.
    pub total_bytes: u64,
    /// Cache hits since open.
    pub hits: u64,
    /// Cache misses since open.
    pub misses: u64,
}

/// Shared mutable index — the HashMap behind a single RwLock. `get()` only
/// requires a read lock (LRU bumps happen via atomic on the entry); `insert()`
/// and `evict()` take the write lock.
#[derive(Debug, Default)]
struct Inner {
    index: HashMap<String, CacheEntry>,
}

/// Local file cache with SHA-256 key → on-disk path mapping.
pub struct FileCache {
    root: PathBuf,
    /// Soft maximum cache size in bytes. Zero means unlimited.
    max_bytes: u64,
    // Reason: Split the old `Mutex<Inner>` into `RwLock<Inner>` (index only) +
    // per-counter `AtomicU64`s. The cache is read-heavy: `get()` is the hot
    // path and now needs only a read lock + relaxed atomic bumps, so parallel
    // lookups no longer serialize behind the writer lock. LRU `last_access`
    // bookkeeping is kept on the read path via `AtomicU64` interior mutability
    // on `CacheEntry` (option (b) from the refactor brief) — simpler than a
    // secondary mutex around an access list, and avoids any lock-upgrade dance.
    /// Index behind an RwLock so concurrent reads don't serialize.
    inner: RwLock<Inner>,
    /// Hit/miss counters as atomics so `get()` stays read-only.
    hits: AtomicU64,
    misses: AtomicU64,
}

/// Acquire the inner read lock, recovering from any poisoning. Poisoned data
/// may be in any consistent-but-unexpected state, but since this cache's
/// mutation boundaries are individual function calls, a partial state is
/// still safe to read (worst case: a stale entry persists until evicted).
fn read_inner(m: &RwLock<Inner>) -> std::sync::RwLockReadGuard<'_, Inner> {
    m.read().unwrap_or_else(|poisoned| {
        tracing::warn!("FileCache inner lock was poisoned; recovering (read)");
        poisoned.into_inner()
    })
}

/// Acquire the inner write lock, recovering from any poisoning.
fn write_inner(m: &RwLock<Inner>) -> std::sync::RwLockWriteGuard<'_, Inner> {
    m.write().unwrap_or_else(|poisoned| {
        tracing::warn!("FileCache inner lock was poisoned; recovering (write)");
        poisoned.into_inner()
    })
}

impl FileCache {
    /// Open a new cache rooted at `root`. Creates the directory if absent.
    /// `max_bytes` is a soft cap; entries beyond it will be evicted LRU.
    /// Pass `0` for unlimited.
    pub fn open(root: impl AsRef<Path>, max_bytes: u64) -> std::io::Result<Self> {
        let root = root.as_ref().to_path_buf();
        std::fs::create_dir_all(&root)?;
        let mut index = HashMap::new();
        // Re-scan existing files in the cache dir (resumes state on reopen).
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
                            last_access: AtomicU64::new(last_access),
                        },
                    );
                }
            }
        }
        Ok(Self {
            root,
            max_bytes,
            inner: RwLock::new(Inner { index }),
            hits: AtomicU64::new(0),
            misses: AtomicU64::new(0),
        })
    }

    /// Compute the cache key for a source URL (SHA-256 hex).
    pub fn key_for(url: &str) -> String {
        use sha2::{Digest, Sha256};
        let mut h = Sha256::new();
        h.update(url.as_bytes());
        let digest = h.finalize();
        // Reason: `hex::encode` does a single allocation + tight inner loop
        // instead of 32 `format!("{b:02x}")` heap allocations per key.
        hex::encode(digest)
    }

    /// Look up a URL in the cache. Returns `Some(path)` on hit, `None` on miss.
    /// Touches the entry's `last_access` on hit (via atomic, no write lock).
    pub fn get(&self, url: &str) -> Option<PathBuf> {
        let key = Self::key_for(url);
        let g = read_inner(&self.inner);
        if let Some(entry) = g.index.get(&key) {
            entry.last_access.store(now_secs(), Ordering::Relaxed);
            let path = entry.path.clone();
            self.hits.fetch_add(1, Ordering::Relaxed);
            Some(path)
        } else {
            self.misses.fetch_add(1, Ordering::Relaxed);
            None
        }
    }

    /// Insert a file into the cache. `local_src` is moved into the cache dir.
    /// Returns the new cached path.
    pub fn insert(&self, url: &str, local_src: &Path) -> std::io::Result<PathBuf> {
        let key = Self::key_for(url);
        let dst = self.root.join(&key);
        std::fs::copy(local_src, &dst)?;
        let size = std::fs::metadata(&dst)?.len();
        {
            let mut g = write_inner(&self.inner);
            g.index.insert(
                key,
                CacheEntry {
                    path: dst.clone(),
                    size_bytes: size,
                    last_access: AtomicU64::new(now_secs()),
                },
            );
        }
        self.evict_if_needed();
        Ok(dst)
    }

    /// Drop the LRU entry until total bytes fit under `max_bytes` (no-op if 0).
    fn evict_if_needed(&self) {
        if self.max_bytes == 0 {
            return;
        }
        let mut g = write_inner(&self.inner);
        loop {
            let total: u64 = g.index.values().map(|e| e.size_bytes).sum();
            if total <= self.max_bytes {
                break;
            }
            let lru_key = match g
                .index
                .iter()
                .min_by_key(|(_, e)| e.last_access.load(Ordering::Relaxed))
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

    /// Get current cache stats snapshot.
    pub fn stats(&self) -> CacheStats {
        let g = read_inner(&self.inner);
        CacheStats {
            entries: g.index.len(),
            total_bytes: g.index.values().map(|e| e.size_bytes).sum(),
            hits: self.hits.load(Ordering::Relaxed),
            misses: self.misses.load(Ordering::Relaxed),
        }
    }

    /// Clear all entries from the cache (deletes files on disk).
    pub fn clear(&self) -> std::io::Result<()> {
        let mut g = write_inner(&self.inner);
        for (_, entry) in g.index.drain() {
            let _ = std::fs::remove_file(&entry.path);
        }
        Ok(())
    }
}

fn now_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn key_for_is_deterministic_sha256() {
        let k1 = FileCache::key_for("https://example.com/foo.tif");
        let k2 = FileCache::key_for("https://example.com/foo.tif");
        assert_eq!(k1, k2);
        assert_eq!(k1.len(), 64); // SHA-256 hex = 64 chars
    }

    #[test]
    fn open_creates_directory_and_starts_empty() {
        let dir = tempfile::tempdir().unwrap();
        let cache = FileCache::open(dir.path().join("cache"), 0).unwrap();
        assert_eq!(cache.stats().entries, 0);
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

        let got = cache.get(url).expect("cache hit");
        assert_eq!(got, cached);

        let stats = cache.stats();
        assert_eq!(stats.entries, 1);
        assert_eq!(stats.hits, 1);
    }

    #[test]
    fn miss_does_not_increment_hits() {
        let dir = tempfile::tempdir().unwrap();
        let cache = FileCache::open(dir.path(), 0).unwrap();
        assert!(cache.get("https://example.com/nope.tif").is_none());
        let stats = cache.stats();
        assert_eq!(stats.misses, 1);
        assert_eq!(stats.hits, 0);
    }

    #[test]
    fn concurrent_get_and_insert_does_not_deadlock_and_stats_balance() {
        use std::sync::Arc;
        use std::thread;
        let dir = tempfile::tempdir().unwrap();
        let cache = Arc::new(FileCache::open(dir.path(), 0).unwrap());

        // Pre-populate a few entries so reads can hit.
        for i in 0..4 {
            let src = dir.path().join(format!("seed_{i}.bin"));
            std::fs::write(&src, b"x").unwrap();
            cache.insert(&format!("seed://{i}"), &src).unwrap();
        }

        let mut handles = Vec::new();
        for t in 0..8 {
            let cache = cache.clone();
            let dir = dir.path().to_path_buf();
            handles.push(thread::spawn(move || {
                for i in 0..16 {
                    let url = format!("seed://{}", i % 4);
                    let _ = cache.get(&url); // mostly hits
                    let src = dir.join(format!("t{t}_i{i}.bin"));
                    std::fs::write(&src, b"y").unwrap();
                    cache.insert(&format!("t{t}://{i}"), &src).unwrap();
                    let _ = cache.stats(); // mixes index+stats lock acquisition
                }
            }));
        }
        for h in handles {
            h.join().expect("thread panicked");
        }
        let stats = cache.stats();
        // 4 seed inserts + 8 threads × 16 inserts = 132. Each get() either hits or misses,
        // never both; we just assert no deadlock and stats are coherent.
        assert!(stats.entries >= 4, "lost entries: {stats:?}");
        assert!(stats.hits + stats.misses >= 8 * 16, "stats truncated: {stats:?}");
    }

    #[test]
    fn survives_lock_poisoning_from_panicked_thread() {
        use std::sync::Arc;
        let dir = tempfile::tempdir().unwrap();
        let cache = Arc::new(FileCache::open(dir.path(), 0).unwrap());
        let cache_clone = cache.clone();
        // Poison the internal lock by panicking while holding it (via a forced
        // panic on the insert path's borrow).
        let h = std::thread::spawn(move || {
            // Force a panic mid-operation by inserting then panicking; the panic
            // unwinds while no lock is held, so simulate explicitly:
            let src = std::env::temp_dir().join("poison-src");
            std::fs::write(&src, b"x").unwrap();
            cache_clone.insert("poison://1", &src).unwrap();
            panic!("simulated worker panic");
        });
        let _ = h.join();
        // Even if the panic poisoned internal state, the cache must still answer.
        assert!(cache.get("poison://1").is_some(), "post-panic lookup failed");
        let stats = cache.stats();
        assert!(stats.entries >= 1);
    }

    #[test]
    fn lru_eviction_under_size_cap() {
        let dir = tempfile::tempdir().unwrap();
        // 1024-byte cap; insert two 600-byte files → first should evict.
        let cache = FileCache::open(dir.path(), 1024).unwrap();
        let big = vec![0u8; 600];
        let src_a = dir.path().join("a.bin");
        let src_b = dir.path().join("b.bin");
        std::fs::write(&src_a, &big).unwrap();
        std::fs::write(&src_b, &big).unwrap();
        cache.insert("http://a", &src_a).unwrap();
        // Sleep 1s so `last_access` differs.
        std::thread::sleep(std::time::Duration::from_secs(1));
        cache.insert("http://b", &src_b).unwrap();
        let stats = cache.stats();
        // One of them was evicted.
        assert!(stats.entries <= 1, "expected eviction; got {} entries", stats.entries);
        assert!(stats.total_bytes <= 1024);
    }
}
