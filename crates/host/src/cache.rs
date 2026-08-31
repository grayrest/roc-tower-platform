//! In-memory, dependency-scoped response cache with variant matching.
//!
//! Stores fully-buffered (already-compressed) response bytes. The BASE key is
//! everything knowable before the handler runs — `(method, path+query,
//! negotiated Accept-Encoding)`. Under each base key live VARIANTS: an entry
//! records which request inputs its response depended on (cookie names read,
//! whether the body was extracted, `Vary`-named headers — decision log
//! D13/D14 + R1), and a lookup matches only when the request's values of
//! exactly those inputs hash to a stored variant.
//!
//! Invalidation is column-scoped: each entry carries the derived
//! `(table, columns-read)` set from the analysis shadow; a committed write
//! purges the entries whose read columns INTERSECT the written columns
//! (`Cols::All` on either side always intersects — the sound floor). A
//! per-table GENERATION counter closes the read-then-store race (R13): reads
//! sample the counter before touching data, writes bump it, and `insert`
//! drops an entry whose sampled generation moved — a response computed from
//! pre-write data can't be stored after the write's purge. `purge_all` bumps
//! a global epoch with the same handshake.
//!
//! The store is deliberately decoupled from tower/hyper: it speaks `http`
//! header types + `Bytes`; the middleware ([`crate::middleware`]) and the
//! write path ([`crate::effect`]) drive it. A process-global handle
//! ([`global`]) is installed once at startup when the app enables the
//! `ResponseCache` setting.

use std::collections::hash_map::{DefaultHasher, RandomState};
use std::collections::{HashMap, HashSet, VecDeque};
use std::hash::{BuildHasher, Hash, Hasher};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};

use bytes::Bytes;
use http::{HeaderName, HeaderValue};

use crate::shadow::Cols;

/// Per-process random seed for every cache hash. `DefaultHasher::new()` is
/// fixed-key SipHash — identical in every process and collidable offline — so a
/// shared cache keyed on it can be poisoned (a crafted body/cookie/query
/// colliding another's variant serves the wrong response). A process-static
/// `RandomState` makes the keys per-process and non-craftable; the cache is
/// in-memory, so per-process stability is all it needs, and the record and
/// lookup sides share this one seed so they still agree.
static HASH_SEED: OnceLock<RandomState> = OnceLock::new();

/// A hasher seeded from the per-process `HASH_SEED`. `domain` domain-separates
/// inputs that share the u64 space (a raw body hash must not collide a raw
/// query hash) by mixing a fixed tag in first.
fn seeded_hasher(domain: u8) -> DefaultHasher {
    let mut h = HASH_SEED.get_or_init(RandomState::new).build_hasher();
    domain.hash(&mut h);
    h
}

/// Base identity of a cached resource: everything knowable BEFORE the
/// handler runs. `encoding` is the negotiated `Accept-Encoding` (normalized)
/// because the stored body is already compressed — a gzip client and an
/// identity client must not share an entry.
#[derive(Clone, PartialEq, Eq, Hash, Debug)]
pub struct Key {
    pub method: String,
    pub target: String,
    pub encoding: String,
}

/// A stored response, enough to replay verbatim.
#[derive(Clone)]
pub struct Stored {
    pub status: u16,
    pub headers: Vec<(HeaderName, HeaderValue)>,
    pub body: Bytes,
    pub etag: String,
}

/// Which request inputs a base key's responses vary on — the D13/D14/`Vary`
/// dimensions. Lookup samples the REQUEST's values of exactly these inputs
/// and hashes them into a variant id. All variants under one base key share
/// one spec (the route's variance is a property of the route; a handler
/// branch that changes it replaces the whole set).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct VariantSpec {
    /// Cookie NAMES the route read, in first-read order.
    pub cookies: Vec<String>,
    /// The route extracted the request body.
    pub body: bool,
    /// Request-header names from the response's `Vary` header (lowercased).
    pub headers: Vec<String>,
}

impl VariantSpec {
    pub fn is_plain(&self) -> bool {
        self.cookies.is_empty() && !self.body && self.headers.is_empty()
    }
}

/// The variant id of a plain (no-variance) entry.
pub const PLAIN_VARIANT: u64 = 0;

/// Request-extension marker the cache middleware attaches when a request is
/// store-eligible (method + bypass rules passed). The router threads it into
/// the handler thread's tracking flag, so BYPASSED traffic (authorization,
/// wrong method, cache disabled) skips dependency attribution entirely —
/// zero extra locks on that path.
#[derive(Clone, Copy)]
pub struct CacheEligible;

/// The variant id for a request under `spec`. The same function computes the
/// id at store time (values from the recorded `CacheDeps`) and at lookup time
/// (values from the incoming request), so the two always agree. `None` when
/// the spec needs a body hash the caller doesn't have — unresolvable, a miss.
pub fn variant_hash(
    spec: &VariantSpec,
    cookie_value: impl Fn(&str) -> Option<String>,
    body_hash: Option<u64>,
    header_value: impl Fn(&str) -> Option<String>,
) -> Option<u64> {
    if spec.is_plain() {
        return Some(PLAIN_VARIANT);
    }
    if spec.body && body_hash.is_none() {
        return None;
    }
    let mut hasher = seeded_hasher(0);
    for name in &spec.cookies {
        name.hash(&mut hasher);
        cookie_value(name).hash(&mut hasher);
    }
    if spec.body {
        body_hash.hash(&mut hasher);
    }
    for name in &spec.headers {
        name.hash(&mut hasher);
        header_value(name).hash(&mut hasher);
    }
    Some(hasher.finish())
}

/// Parse request cookies per RFC 6265 (`a=1; b=2`), header order preserved.
/// Shared by the router (building the ctx's cookie store) and the cache
/// middleware (resolving cookie-variant lookups) so both see identical pairs.
pub(crate) fn parse_cookies(headers: &http::HeaderMap) -> Vec<(String, String)> {
    let mut cookies = Vec::new();
    for value in headers.get_all(http::header::COOKIE) {
        let Ok(text) = value.to_str() else { continue };
        for pair in text.split(';') {
            let pair = pair.trim();
            if pair.is_empty() {
                continue;
            }
            match pair.split_once('=') {
                Some((name, value)) => {
                    cookies.push((name.trim().to_owned(), value.trim().to_owned()));
                }
                None => cookies.push((pair.to_owned(), String::new())),
            }
        }
    }
    cookies
}

/// What a request's reads and coeffect extractions contribute to caching.
/// Produced by `effects::take_cache_attribution` ON THE HANDLER THREAD (the
/// reaper drops the ctx entry before the router's oneshot resolves — R9),
/// threaded through the join result, and carried to the cache middleware as
/// a response extension.
#[derive(Clone, Debug)]
pub struct CacheDeps {
    /// Derived dependencies: (table, columns-read, table generation sampled
    /// BEFORE the first read of that table — the R13 handshake).
    pub deps: Vec<(Box<str>, Cols, u64)>,
    /// Global epoch sampled at the first recorded read (guards `purge_all`).
    pub epoch: u64,
    /// False when the request touched a nondeterministic host primitive, a
    /// Must DbWrite/HttpSend effect, an underivable statement, or read-path
    /// DML — the response must not be stored.
    pub cacheable: bool,
    /// (name, value) of each cookie the route READ, in first-read order —
    /// the D13 variant dimension. `None` = absent (distinct from empty).
    pub cookies: Vec<(String, Option<String>)>,
    /// Hash of the raw body bytes IF the route extracted the body (D14).
    pub body_hash: Option<u64>,
    /// CANONICAL body key — the fold of the structural hashes of what the
    /// route's typed decodes actually produced — present only when EVERY
    /// body read was a typed decode. When set, the body variant keys on it
    /// instead of `body_hash`, so raw spellings of one message share one
    /// entry (the cache-DoS mitigation); `body_hash` then only feeds the
    /// raw→canonical alias table lookups translate through.
    pub body_key: Option<u64>,
    /// CANONICAL query key — the structural hash of the decoded query record
    /// (`Req.query_record!`), present only when the route DECODED its query.
    /// When set, the store aliases this request's raw target to a canonical
    /// target keyed on it, so raw spellings that decode to the same record
    /// collapse onto one entry. A route that never decodes its query is
    /// unaffected (its raw target stays the base key — fail-safe, no drop).
    pub query_key: Option<u64>,
}

struct Entry {
    stored: Stored,
    expires: Instant,
    /// Column-scoped dependency set, retained so purge/eviction can consult
    /// and scrub the reverse index.
    deps: Vec<(Box<str>, Cols)>,
}

struct VariantSet {
    spec: VariantSpec,
    variants: HashMap<u64, Entry>,
}

struct Inner {
    map: HashMap<Key, VariantSet>,
    /// Reverse index: table -> (base, variant) pairs that must be checked
    /// when that table is written.
    by_table: HashMap<Box<str>, HashSet<(Key, u64)>>,
    /// Insertion order over variants, for FIFO capacity eviction.
    order: VecDeque<(Key, u64)>,
    /// Live variant count (order may hold dead pairs).
    len: usize,
    /// Per-table write generations (R13). Bumped by every purge_write, even
    /// when no entry dies — in-flight readers of that table must not store.
    gens: HashMap<Box<str>, u64>,
    /// Global epoch: bumped by purge_all (runtime DDL, fallback flush).
    epoch: u64,
}

pub struct ResponseCache {
    ttl: Duration,
    max_entries: usize,
    inner: Mutex<Inner>,
    /// raw-body-hash → canonical body key, so lookups (which run BEFORE any
    /// decode exists) can translate an incoming raw hash to the canonical
    /// variant stored entries key on. Bounded and cheap (u64 → u64): junk
    /// raw spellings churn only these slots, never stored bodies. Cleared
    /// wholesale at capacity — aliases re-establish on the next store.
    aliases: Mutex<HashMap<u64, u64>>,
    /// Raw request target -> canonical request target, for query-key
    /// canonicalization. Keyed on the FULL raw target (which includes the
    /// path, so entries for different routes cannot collide — review S1).
    /// Bounded with FIFO eviction (not a wholesale clear — review S4) so an
    /// adversarial junk-query flood evicts one alias at a time.
    target_aliases: Mutex<HashMap<String, String>>,
    target_alias_order: Mutex<VecDeque<String>>,
}

/// Hash of raw request-body bytes — the D14 variant dimension. ONE function
/// shared by the host side (recording, in `take_cache_attribution`) and the
/// middleware side (lookup) so the two always agree.
pub fn body_hash_of(bytes: &[u8]) -> u64 {
    let mut hasher = seeded_hasher(1);
    bytes.hash(&mut hasher);
    hasher.finish()
}

/// A strong ETag over the exact stored bytes: hashed once at insert, then the
/// middleware answers `If-None-Match` with `304` for free.
pub fn etag_for(body: &[u8]) -> String {
    let mut h = seeded_hasher(2);
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
                len: 0,
                gens: HashMap::new(),
                epoch: 0,
            }),
            aliases: Mutex::new(HashMap::new()),
            target_aliases: Mutex::new(HashMap::new()),
            target_alias_order: Mutex::new(VecDeque::new()),
        }
    }

    /// Record that raw body hash `raw` decodes to canonical key `canonical`.
    /// Bounded at 4× the entry cap; at capacity the table is cleared
    /// wholesale (aliases re-establish on the next store of each spelling).
    pub fn alias_body_key(&self, raw: u64, canonical: u64) {
        let mut aliases = self.aliases.lock().unwrap();
        if aliases.len() >= self.max_entries.saturating_mul(4) {
            aliases.clear();
        }
        aliases.insert(raw, canonical);
    }

    /// Translate an incoming request's raw body hash to the canonical key
    /// stored entries use, if a store recorded one.
    pub fn body_key_for(&self, raw: u64) -> Option<u64> {
        self.aliases.lock().unwrap().get(&raw).copied()
    }

    /// Record that raw target `raw` canonicalizes to `canonical`. Bounded at
    /// 4x the entry cap; at capacity the OLDEST alias is evicted (FIFO), never
    /// a wholesale clear, so one route's flood cannot wipe every other route's
    /// aliases at once.
    pub fn alias_target(&self, raw: String, canonical: String) {
        if raw == canonical {
            return;
        }
        let mut aliases = self.target_aliases.lock().unwrap();
        let mut order = self.target_alias_order.lock().unwrap();
        if aliases.insert(raw.clone(), canonical).is_none() {
            order.push_back(raw);
            while order.len() > self.max_entries.saturating_mul(4) {
                if let Some(old) = order.pop_front() {
                    aliases.remove(&old);
                }
            }
        }
    }

    /// Translate an incoming request's raw target to the canonical target a
    /// prior store recorded, if any.
    pub fn target_alias_for(&self, raw: &str) -> Option<String> {
        self.target_aliases.lock().unwrap().get(raw).cloned()
    }

    /// Look up a live entry. `resolve` maps the base key's variant spec to
    /// the REQUEST's variant id (`None` = unresolvable right now — e.g. the
    /// spec wants a body hash the caller hasn't buffered — treated as a
    /// miss). Expired variants are dropped lazily on the way past.
    pub fn lookup(
        &self,
        base: &Key,
        resolve: impl FnOnce(&VariantSpec) -> Option<u64>,
        now: Instant,
    ) -> Option<Stored> {
        let mut inner = self.inner.lock().unwrap();
        let set = inner.map.get(base)?;
        let variant = resolve(&set.spec)?;
        match set.variants.get(&variant) {
            Some(entry) if entry.expires > now => Some(entry.stored.clone()),
            Some(_) => {
                remove_variant(&mut inner, base, variant);
                None
            }
            None => None,
        }
    }

    /// Insert an entry under `(base, variant)` with variance `spec` and its
    /// column-scoped `deps`. The R13 handshake: each dep carries the table
    /// generation sampled BEFORE its first read, and `epoch` the global epoch
    /// — if either moved (a write purged / a flush ran since the read), the
    /// response may be stale and is NOT stored. A spec change replaces every
    /// variant under the base (the route's variance changed shape).
    // One call site (maybe_store); a param struct would just re-name the row.
    #[allow(clippy::too_many_arguments)]
    pub fn insert(
        &self,
        base: Key,
        variant: u64,
        spec: VariantSpec,
        stored: Stored,
        deps: Vec<(Box<str>, Cols, u64)>,
        epoch: u64,
        now: Instant,
    ) {
        let mut inner = self.inner.lock().unwrap();

        if inner.epoch != epoch {
            return;
        }
        for (table, _, gen_at_read) in &deps {
            if inner.gens.get(table.as_ref()).copied().unwrap_or(0) != *gen_at_read {
                return;
            }
        }

        match inner.map.get(&base) {
            Some(set) if set.spec == spec => remove_variant(&mut inner, &base, variant),
            Some(_) => remove_base(&mut inner, &base),
            None => {}
        }

        let deps: Vec<(Box<str>, Cols)> =
            deps.into_iter().map(|(table, cols, _)| (table, cols)).collect();
        for (table, _) in &deps {
            inner
                .by_table
                .entry(table.clone())
                .or_default()
                .insert((base.clone(), variant));
        }
        let expires = now + self.ttl;
        let replaced = inner
            .map
            .entry(base.clone())
            .or_insert_with(|| VariantSet {
                spec,
                variants: HashMap::new(),
            })
            .variants
            .insert(
                variant,
                Entry {
                    stored,
                    expires,
                    deps,
                },
            )
            .is_some();
        // Only a NEWLY created (base, variant) grows the set and the eviction
        // queue. Re-storing an existing variant (the steady state under
        // canonical keying: every junk spelling re-stores one canonical entry)
        // must not push a duplicate `order` slot — the drain is gated on `len`,
        // which a re-store leaves unchanged, so duplicates would accumulate
        // without bound.
        if !replaced {
            inner.len += 1;
            inner.order.push_back((base, variant));
        }

        while inner.len > self.max_entries {
            let Some((base, variant)) = inner.order.pop_front() else {
                break;
            };
            // May already be gone (replaced/purged); only prune if live.
            if inner.map.get(&base).is_some_and(|set| set.variants.contains_key(&variant)) {
                remove_variant(&mut inner, &base, variant);
            }
        }
    }

    /// A committed write of `cols` on `table`: bump the table's generation
    /// (unconditionally — in-flight readers must not store pre-write data),
    /// then drop every entry whose read columns intersect the written ones.
    pub fn purge_write(&self, table: &str, cols: &Cols) {
        let mut inner = self.inner.lock().unwrap();
        *inner.gens.entry(Box::from(table)).or_insert(0) += 1;
        let Some(candidates) = inner.by_table.get(table) else {
            return;
        };
        let doomed: Vec<(Key, u64)> = candidates
            .iter()
            .filter(|(base, variant)| {
                inner
                    .map
                    .get(base)
                    .and_then(|set| set.variants.get(variant))
                    .is_some_and(|entry| {
                        entry
                            .deps
                            .iter()
                            .any(|(dep_table, read)| dep_table.as_ref() == table && cols.intersects(read))
                    })
            })
            .cloned()
            .collect();
        for (base, variant) in doomed {
            remove_variant(&mut inner, &base, variant);
        }
    }

    /// Drop everything and bump the epoch (runtime DDL, fallback flush).
    /// In-flight readers fail the epoch check at insert.
    pub fn purge_all(&self) {
        let mut inner = self.inner.lock().unwrap();
        inner.map.clear();
        inner.by_table.clear();
        inner.order.clear();
        inner.len = 0;
        inner.epoch += 1;
    }

    /// The current generation of `table` (0 until first written). Sampled by
    /// the read path BEFORE touching data — the R13 handshake.
    pub fn generation(&self, table: &str) -> u64 {
        let inner = self.inner.lock().unwrap();
        inner.gens.get(table).copied().unwrap_or(0)
    }

    /// The current global epoch. Sampled once per request at first read.
    pub fn epoch(&self) -> u64 {
        self.inner.lock().unwrap().epoch
    }

    #[cfg(test)]
    fn len(&self) -> usize {
        self.inner.lock().unwrap().len
    }

    #[cfg(test)]
    fn table_index_len(&self, table: &str) -> usize {
        self.inner.lock().unwrap().by_table.get(table).map_or(0, |s| s.len())
    }
}

/// Remove one variant: scrub it from the reverse index and the live count,
/// dropping the base when its last variant dies. `order` is left lazy (dead
/// pairs are skipped at eviction time).
fn remove_variant(inner: &mut Inner, base: &Key, variant: u64) {
    let Some(set) = inner.map.get_mut(base) else {
        return;
    };
    let Some(entry) = set.variants.remove(&variant) else {
        return;
    };
    inner.len -= 1;
    let empty = set.variants.is_empty();
    if empty {
        inner.map.remove(base);
    }
    for (table, _) in &entry.deps {
        if let Some(index) = inner.by_table.get_mut(table) {
            index.remove(&(base.clone(), variant));
            if index.is_empty() {
                inner.by_table.remove(table);
            }
        }
    }
}

/// Remove a whole base key (spec change): every variant dies.
fn remove_base(inner: &mut Inner, base: &Key) {
    let variants: Vec<u64> = inner
        .map
        .get(base)
        .map(|set| set.variants.keys().copied().collect())
        .unwrap_or_default();
    for variant in variants {
        remove_variant(inner, base, variant);
    }
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

/// Write-path convenience: purge `table` at full-column width. (The derived
/// write path uses [`purge_write_global`] with real column sets; this is the
/// conservative shim.)
pub fn purge_table_global(table: &str) {
    purge_write_global(table, &Cols::All);
}

/// Write-path convenience: purge a derived (table, columns) write.
pub fn purge_write_global(table: &str, cols: &Cols) {
    if let Some(cache) = GLOBAL.get() {
        cache.purge_write(table, cols);
    }
}

/// Write-path convenience: flush everything (runtime DDL, fallback).
pub fn purge_all_global() {
    if let Some(cache) = GLOBAL.get() {
        cache.purge_all();
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

    fn named(cols: &[&str]) -> Cols {
        Cols::Named(cols.iter().map(|c| Box::from(*c)).collect())
    }

    fn all(tables: &[&str]) -> Vec<(Box<str>, Cols, u64)> {
        tables.iter().map(|t| (Box::from(*t), Cols::All, 0)).collect()
    }

    /// Insert a plain (no-variance) entry with fresh generations.
    fn put(cache: &ResponseCache, target: &str, body: &str, deps: Vec<(Box<str>, Cols, u64)>, now: Instant) {
        let deps = deps
            .into_iter()
            .map(|(table, cols, _)| {
                let generation = cache.generation(&table);
                (table, cols, generation)
            })
            .collect();
        cache.insert(
            key(target),
            PLAIN_VARIANT,
            VariantSpec::default(),
            stored(body),
            deps,
            cache.epoch(),
            now,
        );
    }

    fn get(cache: &ResponseCache, target: &str, now: Instant) -> Option<Stored> {
        cache.lookup(&key(target), |spec| spec.is_plain().then_some(PLAIN_VARIANT), now)
    }

    /// a live entry replays, then lazily expires past the TTL
    #[test]
    fn hit_returns_stored_body_then_expires() {
        let cache = ResponseCache::new(Duration::from_secs(1), 16);
        let now = Instant::now();
        put(&cache, "/a", "A", all(&["articles"]), now);

        assert_eq!(get(&cache, "/a", now).expect("live entry").body.as_ref(), b"A");
        assert!(get(&cache, "/a", now + Duration::from_secs(2)).is_none());
        assert_eq!(cache.len(), 0, "expired entry should be pruned on lookup");
    }

    /// unknown base key is a miss
    #[test]
    fn miss_on_unknown_key() {
        let cache = ResponseCache::new(Duration::from_secs(1), 16);
        assert!(get(&cache, "/nope", Instant::now()).is_none());
    }

    /// an All write purges dependents; unrelated entries survive
    #[test]
    fn write_to_a_table_purges_only_dependents() {
        let cache = ResponseCache::new(Duration::from_secs(60), 16);
        let now = Instant::now();
        put(&cache, "/list", "L", all(&["articles", "users"]), now);
        put(&cache, "/tags", "T", all(&["article_tags"]), now);

        cache.purge_write("articles", &Cols::All);

        assert!(get(&cache, "/list", now).is_none(), "articles-dependent purged");
        assert!(get(&cache, "/tags", now).is_some(), "unrelated entry survives");
        assert_eq!(cache.table_index_len("users"), 0, "reverse index leaked");
    }

    /// a column-disjoint write leaves an entry alone; intersecting kills it
    #[test]
    fn purge_is_column_scoped() {
        let cache = ResponseCache::new(Duration::from_secs(60), 16);
        let now = Instant::now();
        let deps = vec![(Box::from("articles"), named(&["id", "author_id"]), 0)];
        put(&cache, "/comments", "C", deps, now);

        // favorites_count is not read by the entry — spare it (the realized
        // precision: favorites writes don't flush comment lists).
        cache.purge_write("articles", &named(&["favorites_count"]));
        assert!(get(&cache, "/comments", now).is_some(), "disjoint columns survive");

        cache.purge_write("articles", &named(&["author_id"]));
        assert!(get(&cache, "/comments", now).is_none(), "intersecting column purges");
    }

    /// R13: an insert whose sampled generation moved (write raced the read)
    /// is dropped instead of stored
    #[test]
    fn stale_generation_insert_is_dropped() {
        let cache = ResponseCache::new(Duration::from_secs(60), 16);
        let now = Instant::now();

        // Reader samples the generation, then a write lands before store.
        let sampled = cache.generation("articles");
        cache.purge_write("articles", &Cols::All);
        cache.insert(
            key("/a"),
            PLAIN_VARIANT,
            VariantSpec::default(),
            stored("stale"),
            vec![(Box::from("articles"), Cols::All, sampled)],
            cache.epoch(),
            now,
        );
        assert!(get(&cache, "/a", now).is_none(), "post-purge insert of pre-write data");

        // A fresh read (samples the bumped generation) stores fine.
        put(&cache, "/a", "fresh", all(&["articles"]), now);
        assert!(get(&cache, "/a", now).is_some());
    }

    /// purge_all bumps the epoch, so an in-flight pre-flush insert is dropped
    #[test]
    fn stale_epoch_insert_is_dropped() {
        let cache = ResponseCache::new(Duration::from_secs(60), 16);
        let now = Instant::now();
        let epoch = cache.epoch();
        cache.purge_all();
        cache.insert(
            key("/a"),
            PLAIN_VARIANT,
            VariantSpec::default(),
            stored("stale"),
            vec![],
            epoch,
            now,
        );
        assert!(get(&cache, "/a", now).is_none());
    }

    /// two variants coexist under one base and purge independently of others
    #[test]
    fn variants_share_a_base_key() {
        let cache = ResponseCache::new(Duration::from_secs(60), 16);
        let now = Instant::now();
        let spec = VariantSpec {
            cookies: vec!["theme".into()],
            body: false,
            headers: vec![],
        };
        cache.insert(key("/a"), 7, spec.clone(), stored("dark"), vec![], cache.epoch(), now);
        cache.insert(key("/a"), 9, spec.clone(), stored("light"), vec![], cache.epoch(), now);

        let dark = cache.lookup(&key("/a"), |s| (*s == spec).then_some(7), now);
        let light = cache.lookup(&key("/a"), |s| (*s == spec).then_some(9), now);
        assert_eq!(dark.unwrap().body.as_ref(), b"dark");
        assert_eq!(light.unwrap().body.as_ref(), b"light");
        assert_eq!(cache.len(), 2);
    }

    /// a spec change replaces every variant under the base (the route's
    /// variance changed shape)
    #[test]
    fn spec_change_replaces_the_variant_set() {
        let cache = ResponseCache::new(Duration::from_secs(60), 16);
        let now = Instant::now();
        let cookie_spec = VariantSpec {
            cookies: vec!["theme".into()],
            body: false,
            headers: vec![],
        };
        cache.insert(key("/a"), 7, cookie_spec, stored("dark"), vec![], cache.epoch(), now);
        cache.insert(
            key("/a"),
            PLAIN_VARIANT,
            VariantSpec::default(),
            stored("plain"),
            vec![],
            cache.epoch(),
            now,
        );

        assert_eq!(cache.len(), 1, "old-spec variants replaced");
        assert_eq!(get(&cache, "/a", now).unwrap().body.as_ref(), b"plain");
    }

    /// FIFO capacity eviction counts variants
    #[test]
    fn capacity_evicts_oldest_first() {
        let cache = ResponseCache::new(Duration::from_secs(60), 2);
        let now = Instant::now();
        put(&cache, "/1", "1", all(&["t"]), now);
        put(&cache, "/2", "2", all(&["t"]), now);
        put(&cache, "/3", "3", all(&["t"]), now);

        assert!(get(&cache, "/1", now).is_none(), "oldest evicted");
        assert!(get(&cache, "/2", now).is_some());
        assert!(get(&cache, "/3", now).is_some());
        assert_eq!(cache.len(), 2);
    }

    /// reinsert of the same (base, variant) replaces without duplicate index
    #[test]
    fn reinsert_same_key_does_not_duplicate_index_entries() {
        let cache = ResponseCache::new(Duration::from_secs(60), 16);
        let now = Instant::now();
        put(&cache, "/a", "A", all(&["t"]), now);
        put(&cache, "/a", "A2", all(&["t"]), now);

        assert_eq!(cache.len(), 1);
        assert_eq!(cache.table_index_len("t"), 1, "no duplicate reverse-index key");
        assert_eq!(get(&cache, "/a", now).unwrap().body.as_ref(), b"A2");
    }

    /// re-storing an existing entry under capacity pressure must not inflate
    /// the eviction counter or queue (the order-growth bug): five re-stores of
    /// /a leave capacity for exactly one more, and adding /b then /c evicts
    /// the genuine oldest (/a), not a phantom duplicate.
    #[test]
    fn restore_under_pressure_does_not_inflate_eviction() {
        let cache = ResponseCache::new(Duration::from_secs(60), 2);
        let now = Instant::now();
        put(&cache, "/a", "1", all(&["t"]), now);
        for spelling in ["2", "3", "4", "5", "6"] {
            put(&cache, "/a", spelling, all(&["t"]), now);
        }
        assert_eq!(cache.len(), 1, "five re-stores are still one entry");
        put(&cache, "/b", "B", all(&["t"]), now);
        put(&cache, "/c", "C", all(&["t"]), now);
        assert_eq!(cache.len(), 2);
        assert!(get(&cache, "/a", now).is_none(), "genuine oldest evicted");
        assert!(get(&cache, "/b", now).is_some());
        assert!(get(&cache, "/c", now).is_some());
    }

    /// target aliases translate a raw target to its canonical one, are scoped
    /// so different routes never collide, and evict FIFO (never wholesale).
    #[test]
    fn target_alias_translates_scopes_and_evicts() {
        let cache = ResponseCache::new(Duration::from_secs(60), 1); // 4x cap = 4 aliases
        cache.alias_target("/a?x=1".into(), "/a?~qDEAD".into());
        cache.alias_target("/a?x=1&junk=2".into(), "/a?~qDEAD".into());
        assert_eq!(cache.target_alias_for("/a?x=1").as_deref(), Some("/a?~qDEAD"));
        assert_eq!(
            cache.target_alias_for("/a?x=1&junk=2").as_deref(),
            Some("/a?~qDEAD"),
            "a junk spelling of the same query maps to the same canonical target"
        );
        // A different route's alias never collides (the raw key carries the path).
        cache.alias_target("/b?x=1".into(), "/b?~qBEEF".into());
        assert_eq!(cache.target_alias_for("/b?x=1").as_deref(), Some("/b?~qBEEF"));
        assert_eq!(cache.target_alias_for("/a?x=1").as_deref(), Some("/a?~qDEAD"));
        // Flood past 4x cap: FIFO evicts the oldest, not everything.
        for i in 0..6 {
            cache.alias_target(format!("/flood?n={i}"), format!("/flood?~q{i}"));
        }
        let survivors = (0..6)
            .filter(|i| cache.target_alias_for(&format!("/flood?n={i}")).is_some())
            .count();
        assert!(survivors > 0 && survivors <= 4, "bounded, not wiped: {survivors}");
        // an identity alias is a no-op (never stored)
        cache.alias_target("/same".into(), "/same".into());
        assert!(cache.target_alias_for("/same").is_none());
    }

    /// per-process seeding: a hash is stable within the process (record and
    /// lookup agree) and domain-separated across input kinds.
    #[test]
    fn body_hash_is_stable_within_process() {
        assert_eq!(body_hash_of(b"same"), body_hash_of(b"same"));
        assert_ne!(body_hash_of(b"a"), body_hash_of(b"b"));
    }

    /// etag is stable for identical bytes, distinct for different bytes
    #[test]
    fn etag_is_stable_and_content_sensitive() {
        assert_eq!(etag_for(b"hello"), etag_for(b"hello"));
        assert_ne!(etag_for(b"hello"), etag_for(b"world"));
    }
}
