//! In-memory, table-scoped response cache.
//!
//! Stores fully-buffered (already-compressed) response bytes keyed on
//! `(method, path+query, accept-encoding)`. Each entry carries the set of
//! tables its underlying reads touched; a write to any of those tables (routed
//! through `effect.rs`, the single mutation choke point) purges every entry that
//! depends on it — so invalidation is precise, not a TTL guess. The TTL is only
//! a backstop for anything a caller under-declared. `max_entries` bounds memory
//! with FIFO eviction.
//!
//! The store is deliberately decoupled from tower/hyper: it speaks `http` header
//! types + `Bytes`, and the middleware ([`crate::middleware`]) and write path
//! ([`crate::effect`]) drive it. A process-global handle ([`global`]) is
//! installed once at startup when the app enables the `ResponseCache` setting,
//! mirroring the sqlite connection-pool statics.

use std::collections::hash_map::DefaultHasher;
use std::collections::{HashMap, HashSet, VecDeque};
use std::hash::{Hash, Hasher};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};

use bytes::Bytes;
use http::{HeaderName, HeaderValue};

/// Identifies a cached response. `encoding` is the negotiated `Accept-Encoding`
/// (normalized) because the stored body is already compressed — a gzip client
/// and an identity client must not share an entry.
#[derive(Clone, PartialEq, Eq, Hash, Debug)]
pub struct Key {
    pub method: String,
    pub target: String,
    pub encoding: String,
}

/// A stored response, enough to replay verbatim (headers the handler emitted
/// minus the internal cache-control ones the middleware stripped).
#[derive(Clone)]
pub struct Stored {
    pub status: u16,
    pub headers: Vec<(HeaderName, HeaderValue)>,
    pub body: Bytes,
    pub etag: String,
}

struct Entry {
    stored: Stored,
    expires: Instant,
    /// The tables this entry depends on — retained so eviction/expiry can prune
    /// this key out of the reverse index.
    tables: Vec<Box<str>>,
}

struct Inner {
    map: HashMap<Key, Entry>,
    /// Reverse index: table -> keys that must die when that table is written.
    by_table: HashMap<Box<str>, HashSet<Key>>,
    /// Insertion order for FIFO capacity eviction.
    order: VecDeque<Key>,
}

pub struct ResponseCache {
    ttl: Duration,
    max_entries: usize,
    inner: Mutex<Inner>,
}

/// A strong ETag over the exact stored bytes: hashed once at insert, then the
/// middleware answers `If-None-Match` with `304` for free.
pub fn etag_for(body: &[u8]) -> String {
    let mut h = DefaultHasher::new();
    body.hash(&mut h);
    format!("\"{:016x}\"", h.finish())
}

impl ResponseCache {
    pub fn new(ttl: Duration, max_entries: usize) -> Self {
        ResponseCache {
            ttl,
            max_entries: max_entries.max(1),
            inner: Mutex::new(Inner {
                map: HashMap::new(),
                by_table: HashMap::new(),
                order: VecDeque::new(),
            }),
        }
    }

    /// Return a live (unexpired) entry, cloning the stored bytes (cheap — `Bytes`
    /// is refcounted). Expired entries are dropped lazily on the way past.
    pub fn lookup(&self, key: &Key, now: Instant) -> Option<Stored> {
        let mut inner = self.inner.lock().unwrap();
        match inner.map.get(key) {
            Some(entry) if entry.expires > now => Some(entry.stored.clone()),
            Some(_) => {
                remove_key(&mut inner, key);
                None
            }
            None => None,
        }
    }

    /// Insert (or replace) an entry and tag it with `tables`. Enforces the
    /// capacity bound by evicting the oldest entries.
    pub fn insert(&self, key: Key, stored: Stored, tables: Vec<Box<str>>, now: Instant) {
        let mut inner = self.inner.lock().unwrap();
        // Replace cleanly so the reverse index and order never carry duplicates.
        remove_key(&mut inner, &key);

        for table in &tables {
            inner
                .by_table
                .entry(table.clone())
                .or_default()
                .insert(key.clone());
        }
        let expires = now + self.ttl;
        inner.map.insert(
            key.clone(),
            Entry {
                stored,
                expires,
                tables,
            },
        );
        inner.order.push_back(key);

        while inner.map.len() > self.max_entries {
            let Some(oldest) = inner.order.pop_front() else {
                break;
            };
            // `oldest` may already be gone (replaced/purged); only prune if live.
            if inner.map.contains_key(&oldest) {
                remove_key(&mut inner, &oldest);
            }
        }
    }

    /// Drop every entry that read `table`. Called from the write path after a
    /// mutation to that table commits.
    pub fn purge_table(&self, table: &str) {
        let mut inner = self.inner.lock().unwrap();
        let Some(keys) = inner.by_table.remove(table) else {
            return;
        };
        for key in keys {
            remove_key(&mut inner, &key);
        }
    }

    #[cfg(test)]
    fn len(&self) -> usize {
        self.inner.lock().unwrap().map.len()
    }

    #[cfg(test)]
    fn table_index_len(&self, table: &str) -> usize {
        self.inner
            .lock()
            .unwrap()
            .by_table
            .get(table)
            .map_or(0, |s| s.len())
    }
}

/// Remove `key` from the map and scrub it out of the reverse index and the FIFO
/// order, dropping now-empty table buckets so the index can't grow unbounded.
fn remove_key(inner: &mut Inner, key: &Key) {
    if let Some(entry) = inner.map.remove(key) {
        for table in &entry.tables {
            if let Some(set) = inner.by_table.get_mut(table) {
                set.remove(key);
                if set.is_empty() {
                    inner.by_table.remove(table);
                }
            }
        }
    }
    inner.order.retain(|k| k != key);
}

/// What a request's reads and coeffect extractions contribute to caching.
/// Produced by `effects::take_cache_attribution` ON THE HANDLER THREAD (the
/// reaper drops the ctx entry before the router's oneshot resolves — R9),
/// threaded through the join result, and carried to the cache middleware as
/// a response extension.
// TODO(DC4): drop the allow once maybe_store consumes the fields.
#[allow(dead_code)]
#[derive(Clone, Debug)]
pub struct CacheDeps {
    /// Derived (table, columns-read) dependency set from the shadow analyzer.
    pub deps: Vec<(Box<str>, crate::shadow::Cols)>,
    /// False when the request touched a nondeterministic host primitive, a
    /// Must DbWrite/HttpSend effect, an underivable statement, or read-path
    /// DML — the response must not be stored.
    pub cacheable: bool,
    /// (name, value) of each cookie the route READ, in first-read order —
    /// the D13 variant dimension. `None` = the cookie was absent (distinct
    /// from present-but-empty).
    pub cookies: Vec<(String, Option<String>)>,
    /// Hash of the raw body bytes IF the route extracted the body (D14).
    pub body_hash: Option<u64>,
}

// --- process-global handle -------------------------------------------------

static GLOBAL: OnceLock<Arc<ResponseCache>> = OnceLock::new();

/// Install the process cache once at startup (no-op if already set).
pub fn install_global(cache: Arc<ResponseCache>) {
    let _ = GLOBAL.set(cache);
}

/// The process cache, if the app enabled `ResponseCache`.
pub fn global() -> Option<&'static Arc<ResponseCache>> {
    GLOBAL.get()
}

/// Convenience for the write path: purge `table` from the global cache if one
/// is installed.
pub fn purge_table_global(table: &str) {
    if let Some(cache) = GLOBAL.get() {
        cache.purge_table(table);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key(target: &str) -> Key {
        Key {
            method: "GET".into(),
            target: target.into(),
            encoding: "gzip".into(),
        }
    }

    fn stored(body: &str) -> Stored {
        let bytes = Bytes::from(body.to_string());
        let etag = etag_for(&bytes);
        Stored {
            status: 200,
            headers: vec![(
                HeaderName::from_static("content-type"),
                HeaderValue::from_static("application/json"),
            )],
            body: bytes,
            etag,
        }
    }

    #[test]
    fn hit_returns_stored_body_then_expires() {
        let cache = ResponseCache::new(Duration::from_secs(1), 16);
        let now = Instant::now();
        cache.insert(key("/a"), stored("A"), vec!["articles".into()], now);

        let hit = cache.lookup(&key("/a"), now).expect("live entry");
        assert_eq!(hit.body.as_ref(), b"A");

        // Past the TTL the entry is gone (and lazily removed).
        assert!(cache.lookup(&key("/a"), now + Duration::from_secs(2)).is_none());
        assert_eq!(cache.len(), 0, "expired entry should be pruned on lookup");
    }

    #[test]
    fn miss_on_unknown_key() {
        let cache = ResponseCache::new(Duration::from_secs(1), 16);
        assert!(cache.lookup(&key("/nope"), Instant::now()).is_none());
    }

    #[test]
    fn write_to_a_table_purges_only_dependents() {
        let cache = ResponseCache::new(Duration::from_secs(60), 16);
        let now = Instant::now();
        cache.insert(key("/list"), stored("L"), vec!["articles".into(), "users".into()], now);
        cache.insert(key("/tags"), stored("T"), vec!["article_tags".into()], now);

        cache.purge_table("articles");

        assert!(cache.lookup(&key("/list"), now).is_none(), "articles-dependent purged");
        assert!(cache.lookup(&key("/tags"), now).is_some(), "unrelated entry survives");
        // The reverse index for the shared `users` table is scrubbed too.
        assert_eq!(cache.table_index_len("users"), 0, "reverse index leaked");
    }

    #[test]
    fn capacity_evicts_oldest_first() {
        let cache = ResponseCache::new(Duration::from_secs(60), 2);
        let now = Instant::now();
        cache.insert(key("/1"), stored("1"), vec!["t".into()], now);
        cache.insert(key("/2"), stored("2"), vec!["t".into()], now);
        cache.insert(key("/3"), stored("3"), vec!["t".into()], now); // evicts /1

        assert!(cache.lookup(&key("/1"), now).is_none(), "oldest evicted");
        assert!(cache.lookup(&key("/2"), now).is_some());
        assert!(cache.lookup(&key("/3"), now).is_some());
        assert_eq!(cache.len(), 2);
    }

    #[test]
    fn reinsert_same_key_does_not_duplicate_index_entries() {
        let cache = ResponseCache::new(Duration::from_secs(60), 16);
        let now = Instant::now();
        cache.insert(key("/a"), stored("A"), vec!["t".into()], now);
        cache.insert(key("/a"), stored("A2"), vec!["t".into()], now);

        assert_eq!(cache.len(), 1);
        assert_eq!(cache.table_index_len("t"), 1, "no duplicate reverse-index key");
        assert_eq!(cache.lookup(&key("/a"), now).unwrap().body.as_ref(), b"A2");
    }

    #[test]
    fn etag_is_stable_and_content_sensitive() {
        assert_eq!(etag_for(b"hello"), etag_for(b"hello"));
        assert_ne!(etag_for(b"hello"), etag_for(b"world"));
    }
}
