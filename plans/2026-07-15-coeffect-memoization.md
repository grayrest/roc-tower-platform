# Design: coeffect memoization (the principled response cache)

> Outcome of a grill-me session, 2026-07-15. Supersedes the HTTP-layer
> table-scoped response cache (commits 5c08f22 / c858386) with a design that
> lives at the coeffect/effect layer instead of the tower layer. Not yet
> implemented — this is the agreed shape.

## The reframe

The response cache shipped this session follows a normal web-framework pattern:
an outer HTTP layer keyed on `(method, path+query, Accept-Encoding)`, invalidated
by table. That works, but it's the wrong layer for *this* platform. Here a
handler is a **pure function of its resolved coeffects** — so caching its output
keyed on its inputs isn't "response caching," it's **memoization**. And the
coeffect/effect split is exactly the structure that makes the key and the
invalidation *derivable and un-forgeable* rather than hand-declared:

- the **coeffects** (the "in" side) can declare the identity of what they read,
- the **effects** (the "out" side) can declare, in the same token vocabulary,
  what they changed,

so a write invalidates precisely the memo entries whose reads it invalidated —
using the *same* token functions on both sides.

This also dissolves the anonymous special-case, the `bypass_headers` config, and
the spurious-`follows` over-purge: none of them are needed once the key reflects
exactly the inputs the handler actually depended on.

## The model

A coeffect becomes `(key, resolve!)` — the key is intrinsic, exactly as the
resolver is. **Every** combinator (`map`, `and_then`, `map2`, `withN`) threads
*both* faces uniformly: it composes resolvers as it does today, and folds the
keys the same way. There is no `cachedWithN` family and no bundle/lift
migration — key handling is just one more thing the combinators carry.

### The key face

```
key : req -> [Cacheable(List(U64)), Uncacheable]
```

- **Read-free.** The key face runs in the *key pass* — before any reads — so it
  may only use request-available data: path, params, the bearer token, query
  values. This is the one real constraint (below), not an ergonomic choice.
- **Returns a per-request decision.** A keyfn can return `Uncacheable`
  dynamically based on the request (a 50-term search, a debug header, any shape
  it can't form a safe key for), not only as a static property. `Req.now!`, an
  RNG read, etc. carry a keyfn that always returns `Uncacheable`.
- **`Uncacheable` poisons the fold.** Composing any `Uncacheable` coeffect makes
  the whole handler uncacheable *for that request*. Opt-out is therefore uniform
  and impossible to get wrong — you either describe your inputs or you don't get
  memoized.
- The tokens are `U64` (a hash of the logical dependency, `hash("article", slug)`),
  never strings. The composed identity is a **rolling hash** — one scalar, zero
  heap on the hot path. Because `withN` folds the identity from exactly the
  coeffects it binds, the key provably includes every input the handler saw:
  you cannot omit `viewer` and serve one user's response to another.

### The dep side (invalidation)

Dependency tokens are **not** carried in any Roc type. They are accumulated into
the request `ctx` by the leaf read primitives as they run — the natural extension
of the sqlgen `_tables` work already in the tree: a generated reader knows its
tables; given the resolved row id it records `hash("article", 5)` into the ctx
set host-side. So:

- deps are a host-side side effect of *reading*, built only on a **miss**;
- the union across coeffects is free (the host set dedups);
- nothing is marshaled — no `List(Str)`, no per-request token collection crossing
  the FFI.

Writes **over-emit**: an effect names every identity it holds (`article:foo` and
`article:5`) via the same `Cache.tok` functions the reads use; over-emitting only
over-purges, never goes stale. The viewer case is the one token that is
inherently post-read: `user:42` is known only after auth resolves, so it is a
*dep* (ctx, miss-time) even though the viewer's *identity* token
(`session:hash(bearer)`) is request-derived and read-free.

## Architecture: key pass, then maybe-handler

```
request
  → key pass  (run only the composed keyfns; read-free; returns identity:U64
               or Uncacheable)
      Uncacheable → run the handler normally, don't store
      Cacheable(id) → host memo lookup on `id`
          HIT  → serve stored (already-compressed) bytes; skip reads, handler,
                 serialize entirely  (304 on If-None-Match, as today)
          MISS → run coeffect resolution + pure handler; reads accumulate dep
                 tokens into ctx; store (id → { bytes, etag, deps }); serve
```

- The key pass is a second lightweight Roc entrypoint per route (pure, no reads).
- The memo store is host-side (reuse `cache.rs`), re-keyed on `identity: U64`
  with a reverse `depToken(U64) → entries` index. ETag = hash of the stored
  bytes, computed once at insert.
- Invalidation stays in `effect.rs`: after a `DbWrite` commits, the write's
  over-emitted `U64` tokens purge the reverse index. (Same choke point as today,
  tokens instead of table names.)

## Representation summary (alloc discipline)

| thing | shape | when built | heap |
|---|---|---|---|
| token | `U64` (hash) | — | none |
| identity (lookup key) | `U64` rolling hash | key pass, every request | none |
| deps (invalidation) | ctx set of `U64` | miss only, from reads | arena set, cold path |
| stored entry | bytes + etag + dep tokens | miss only | host-side |

Hit path allocates nothing token-related: compute one rolling `U64`, look up,
serve bytes.

## The one real constraint

The key face is read-free, so a coeffect's *lookup identity* can only depend on
request data, never on the result of an earlier read in the same request
(chained reads where a later key needs an earlier value). Such a coeffect returns
`Uncacheable` — which is the safe answer. This is a genuine boundary of the
design, not a representational choice; everything else (bundle vs variant, string
vs int, list vs tuple) was ergonomics we resolved away.

## Relationship to what exists

- **Supersedes** the HTTP-layer table cache (`Res.cacheable` + `x-tp-cache-tables`
  + table-scoped purge). That was the framework-pattern version; this is the
  platform-native one. The host store (`cache.rs`), the effect.rs purge hook, and
  the sqlgen dep emission are all reusable — they get re-keyed from table
  strings to `U64` tokens and from HTTP key to coeffect identity.
- **Extends** `Cofx.get!` (today's per-request, string-keyed memo): same "a
  coeffect carries a key" idea, lifted to cross-request with invalidation and to
  `U64` identities.

## Open (post-write-up) — deferred, not part of this design

- Trigger-cascade deps (the materialized `tag_list` edge): the read that splices
  a trigger-maintained column can't see the cascade. Under coeffect memoization
  the reader still emits the physical table's tokens; the same hand-declared
  supplement (or a SQLite `update_hook` capturing cascaded writes) applies.
- Whether the key pass is a distinct FFI entrypoint or a phase flag on the
  existing dispatch — an implementation choice, measured when built.
