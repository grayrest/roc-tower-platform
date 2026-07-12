# Coeffect/effect pure handlers — design log

**Date:** 2026-07-11
**Status:** Design agreed via `/grill-me`; validated through two adversarial
reviews and two rounds of spikes (Phase 0 mechanics + Phase 0.5 integration —
see the plan for outcomes); **unbuilt**. Implementation plan lives at
[`plans/2026-07-11-coeffect-effect-handlers.md`](../plans/2026-07-11-coeffect-effect-handlers.md).
Post-design corrections that supersede text below: D1 = **host runs effects**
(the handler return carries `effects`; one unified return type for ALL handlers
incl. the escape hatch, which returns `effects: []`); the memo store is scoped by
a **host-minted `ctx : U64`** threaded through the request record and stores
boxed **thunks** (never raw `Box(a)` — unsound to free host-side); teardown is a
Drop guard on the async side of `spawn_blocking`; `Opt`-"post-flush" semantics
must be redefined (no post-flush hook exists); DB access moves to a connection
pool. Details + evidence in the plan's "Spike outcomes", "Second review", and
Phase 0.5 sections.
**Scope:** A second, sanctioned handler style for the platform in which handlers
are *pure* functions of declared **coeffects** returning a **response plus a
list of effects**, modelled on Clojure Re-frame and translated to synchronous
HTTP. The existing effectful `Request => Response` style is retained as an
escape hatch.

This is the structured-decision dump from the grill-me session. It exists so
future-you can skim "why is the handler model shaped like this?" without
replaying the interview.

---

## Goal

Two properties for handlers:

1. **Purity** — a handler performs no I/O. In Roc this is not a convention but a
   *compiler-enforced* property: a function typed with the pure arrow `->`
   cannot call an effectful (`=>`) hosted function; it won't type-check. This is
   the decisive advantage over Re-frame, where `reg-event-db` purity is enforced
   only by code review.
2. **Standalone** — a handler is free of the plumbing of parsing parameters *in*
   from a request and serialising *out* to a response. It receives already-typed
   inputs and returns already-structured outputs; the framework does the edges.

The mechanism is Re-frame's: replace direct actions with a pure function that
consumes **coeffects** (its declared inputs, injected before it runs) and
produces **effects** (a description of I/O, executed after it returns). The
dividend is that a pure handler is trivially unit-testable — coeffect values in,
`(response, effects)` data out, zero I/O, zero host.

## Constraints

- **Synchronous request/response, not an event loop.** Re-frame is
  asynchronous: its handler returns effects into a loop, and the HTTP "response"
  would be a *later* dispatched event. A synchronous handler must produce its
  response in one shot. This mismatch drives most decisions below.
- **Effect primitives are closed.** All I/O bottoms out in the six hosted FFI
  functions (`log`, `env_var`, `now_ms`, `random_bytes`, `http_send`,
  `sqlite_exec`), which exist because they have Rust implementations behind the
  ABI. No Roc-only code can reach the outside world otherwise. So the *leaves*
  of any effect vocabulary are necessarily platform-defined.
- **Concurrent execution.** Handlers resolve on tokio's blocking pool, 64–128 in
  flight (stress-tested in v0.1). Any shared mutable state races — unlike a
  Re-frame SPA, whose single atom is safe only because JavaScript serialises the
  event loop.
- **Reuse, don't discard, the shipped extractor/`Res` layer.** The argsN
  extractors are already coeffects in miniature (pure, request-only). The design
  must *evolve* them, not throw them away.

---

## Q1 — Where does the HTTP response live in the handler's output?

Options:
- (a) Response-as-effect (purest Re-frame): `Coeffects -> Effects`, response is a
  `Respond(...)` variant inside the returned effect collection; framework
  requires exactly one.
- (b) Response-as-return (distinguished): `Coeffects -> { response, effects }`.
- (c) Response-from-terminal-effect: response produced by an effect that can see
  prior effect results (the Re-frame "dispatch another event with the result"
  pattern made synchronous).

**Choice: (b).** In an async SPA the handler has no meaningful return, so
response-as-effect is natural — but it makes "exactly one response" a *runtime*
check (an effect map with zero or two `Respond`s is a request-time bug). In
synchronous HTTP the response *is* the return, so making it the return *type*
gives "exactly one response, always" as a *compile-time* guarantee. Everything
else Re-frame — the effect list for writes/logs/outbound — is kept. (c) is the
only option that handles "response depends on an effect's result," but that is
resolved on the coeffect side instead (Q2), so it shouldn't drive the
representation.

Final shape (refined through Q3/Q4):
`handler : |coeffect_a, coeffect_b, ...| -> Response(http_code, headers, body, List(Effect))`.

---

## Q2 — Can the response depend on a value only I/O can produce, and how does a *pure* handler get it?

The handler computes its response before any returned effect runs, so anything
the response depends on must already be in hand. Three stress cases: a read-
shaped `GET /users/{id}` (body from a DB read parameterised by the path param),
a proxy-shaped `GET /proxy` (body from an outbound read), and a write-shaped
`POST /users` (body from a value the write produces).

**Choice: reads-before / writes-after, plus inject-nondeterminism-as-coeffects.**
Every value the response can depend on is gathered as a **coeffect before** the
pure handler (by effectful injection at the edge); effects returned by the
handler are only outward writes, run **after**. Read- and proxy-shaped cases fit
directly. Nondeterminism (time, random, ids) enters as coeffects.

**Coeffect gathering is (2b): effectful injectors extending the current
extractors**, not (2a) declared-as-data. `from_request : Request -> Try(a, Response)`
becomes `inject : Ctx => Try(a, Response)` (the `=>` allows pre-handler I/O).
Chosen over declarative-data because it reuses the shipped static-dispatch
extractor mechanism and keeps read logic as ordinary Roc rather than a DSL to
interpret.

### Rejected: client-generated UUIDs for the write-shaped case

The tidy functional fix for "insert returns an id" is to inject a client UUID as
a coeffect. **Rejected by the user** — id generation should stay a database
concern. Instead the id is a **DB-native reserved read**: Postgres
`SELECT nextval('seq')` reserves an id with no row. **Caveat:** SQLite has no
first-class `nextval` (rowid is assigned *at* insert); reserving before insert
needs an own counter table (`UPDATE counters SET v=v+1 ... RETURNING v`), which
is itself a small write. So "reads-before" is framed as "gather all inputs via
whatever I/O — including a sequence bump — then run the pure handler; the
handler's *own* declared effects run after."

---

## Q3 — When a write-effect fails, how does that reach the response?

Reads-before means the handler returns `{ response: 201, effects: [Insert] }`
*before* the insert runs. If the insert fails, the 201 is a lie. Re-frame gives
no help (SPA effects are fire-and-forget with no coupled status). So the response
is **provisional, contingent on the effects succeeding.**

**Choice:** effects split into two classes, encoded on each effect rather than
in separate lists:

```
Effect : Must(Tag, err_handler) | Opt(Tag)
```

- `Must` — critical; must succeed before the response is flushed. Failure
  short-circuits to the effect's **pure** `err_handler`.
- `Opt` — fire-and-forget; runs after flush; failure only logged.

`err_handler` keeps the handler pure: it's an ordinary `Err -> ...` function the
handler *constructs* but never runs; the platform runs the effect and, on `Err`,
calls the supplied pure function to get the response.

Naming: `Must`/`Opt`, chosen to avoid colliding with the existing `Req` module
(an earlier `Req(...)` proposal would have read terribly next to `Req.json`).

---

## Q4 — Is `Tag` (the effect type) a closed platform union or app-extensible?

Re-frame's `reg-fx` lets app code register new effect handlers. Reproducing that
in static Roc is the "closed tag union vs. open registry of erased closures"
tension.

**Choice: closed, platform-owned union** — `Tag : [DbWrite(...), HttpSend(...),
Log(...)]` (the *write* subset of the hosted functions; reads live on the
coeffect side). The deciding fact: effect **primitives** need Rust
implementations behind the ABI, so the leaves are necessarily platform-defined —
`reg-fx`'s core value (new primitive effects) is inexpressible here regardless of
type design. App "extensibility" could only ever mean *composing* primitives,
which `List(Effect)` already provides. A `Custom(Box(() => Try(_, Str)))` variant
(a pure handler can construct a boxed thunk without running it) is **deferred**
until something concrete demands it.

Also settled: `Response` gains a `headers` field —
`Response(http_code, headers, body, List(Effect))` — because content-type,
`Location`, `Set-Cookie`, and `Cache-Control` can't all be derived from the body
constructor.

---

## Q5 — How does a handler declare coeffects, and what is its signature?

Options: (5a) positional typed coeffect args (extend argsN), or (5b) a single
`coeffects` record (`|cofx| -> ...`, closer to Re-frame's one cofx map).

**Choice: (5a) positional typed coeffect args with effectful injectors.** The
handler is `|a, b, c| -> Response(...)`, pure `->`. Each coeffect type carries an
effectful `inject : Ctx => Try(a, Response)`. The argsN wrapper becomes the
interceptor: it runs injectors (effectful), and on success calls the pure handler
and runs its effects. The FFI-boxed thing stays `=>`; the *user's* handler is
`->`. (5b) is rejected because a single record forces a field→injector registry
(reg-cofx) to populate each field, more machinery than positional injectors and
discarding the type-directed dispatch already shipped.

Coeffect failure is already solved by reuse: `inject` returns `Try(a, Response)`;
an `Err` rejects with a full response *before* the handler runs ("user not
found" → the `User` coeffect's injector returns `Err(404)`). This is the shipped
extractor rejection, now able to reject on a *read*, not just parsing.

Handlers never see the raw request — only injectors take `Ctx`. This is what
delivers the "standalone" goal.

---

## Q6 — How do coeffect dependencies and reuse work?

The Re-frame mapping is not coeffects but the **subscription graph** (`reg-sub`
with `:<-` input signals, deduplicated by reagent's reaction cache). Brought into
the coeffect layer, that is a **per-request memoized DAG of derived reads.**

**Choice:** one per-request memoized resolver `Ctx`.
- `inject : Ctx => Try(a, Response)`. `Ctx.request` for root coeffects,
  `Ctx.get(Other)` for derived coeffects (static-dispatched on the requested
  type, resolving its injector).
- `Ctx.get` is **memoized per request.** The wrapper resolves the handler's
  positional coeffects through one shared `Ctx`; a derived injector resolves its
  dependencies through the *same* `Ctx`. A shared dependency resolves once.

Memoization is **correctness, not just speed**: the reserved-id coeffect is
*non-idempotent* (each `nextval` reserves a new id), so two coeffects that both
need the id must see the *same* one — only a shared, memoized resolver
guarantees that. (This reverses an earlier "keep injectors independent"
suggestion, which is broken for non-idempotent reads.)

Settled parameters:
- **Dependencies expressed in-body** via `Ctx.get`, not declared as data. Less
  machinery; reuses static dispatch. The cost — the platform can't see the graph
  ahead of time to parallelise independent reads — is accepted (see next).
- **Serial resolution.** "Good enough"; most handlers have few, fast reads.
- **Memo store: host-side, per-request, keyed by injector function identity.**
  Roc has no `typeof`/reflection (confirmed against the compiler), so a value-
  level key can't be a type name automatically. Since `Ctx.get(User)`
  static-dispatches to `User`'s single `inject` function, the *function identity*
  is the key — same type → same `inject` → same slot. No author-supplied keys,
  no `TypeId`. It is a small closed-keyspace map, not a general `anymap`.
- **Rejected: declarative deps + dense static slot vector + parallel
  resolution.** A true static slot layout requires the transitive DAG reified as
  data (declarative deps) plus a coeffect registry for dense indices. Not worth
  it against a pointer-keyed map, given serial-is-fine.

---

## Q7 — Execution semantics of the `Must` list: atomicity and `err_handler` shape

`Must` effects run serially, in order; first failure short-circuits.

**Choice — atomicity:** DB-targeting `Must` effects run **first**, in **one
per-request transaction** (list order preserved among them; atomic rollback on
any failure). External `Must` effects run **after commit.** Ordering DB-first
resolves the atomicity concern: a DB failure rolls back cleanly before any
external effect fires. The residual **dual-write gap** (an external effect fails
after the DB commits) is acknowledged and left to the **outbox pattern** later
(write an outbox row inside the transaction, deliver as an `Opt`); not built in
v1. Atomicity is DB-only — no distributed transactions exist.

**Choice — `err_handler` is effect-less:** `err_handler : Err -> (code, headers,
body)`, returning a plain response, *not* a full `Response`-with-effects. This
kills the recursion/nested-transaction question (an error path that triggers more
`Must` effects). The common "log the failure" need is covered by the platform
**auto-logging every `Must` failure.** If a handler genuinely needs error-path
effects, that's a signal to restructure.

---

## Q8 — What is the `Err` type an `err_handler` receives?

Options: (8a) uniform `Str` (matches today's `Try(_, Str)` hosted surface, but
distinguishing "unique violation → 409" means substring-matching driver text),
(8b) fully typed per-effect union (clean, but forces exhaustive modelling and
SQLite/Postgres code reconciliation), (8c) small typed union + `Other(Str)`
escape.

**Choice: (8c).** Per effect kind, a small union of the *actionable* failures
plus an escape. DB: `[UniqueViolation(Str), ForeignKeyViolation(Str), Busy,
Other(Str)]`. The host classifies known codes; unknowns degrade to `Other(Str)`
(→ typically 500). This makes the one error-code decision every CRUD API needs
(constraint → 409) type-safe without string-parsing or exhaustive modelling. The
retrofit cost of starting with `Str` (rewriting handlers written against it) is
what tilts this to doing it now.

---

## Q9 — Where does mutable state live? In-memory app-db, or SQL as the `:db`?

Options: (9a) a literal in-memory app-db atom (most faithful to Re-frame's `:db`,
but a shared atom **races** under concurrent requests — Re-frame's is safe only
because JS serialises the event loop — and is volatile), or (9b) the SQL database
*is* the `:db`.

**Choice: (9b).** SQL is the source-of-truth state: read granularly through
coeffects (queries), written granularly through effects (statements), with the
**per-request transaction as the atomic-swap analog** to Re-frame's atom swap. No
in-memory atom. The read-only Model stays a *separate* config coeffect. In-memory
shared state (a cache, counters) becomes an explicit opt-in coeffect+effect pair
over a concurrency-safe host structure — never the default.

Departure to note: unlike Re-frame, there is **no implicit whole-`:db`
coeffect** handed to every handler (you can't load a whole SQL database into a
map). Every read is a declared, granular coeffect, consistent with the
positional-typed-coeffect model.

---

## Q10 — Replace, coexist, or layer over the current effectful handler style?

Structural fact: the pure model is **built on** the effectful primitives
(injectors are `=>` and call `Host.sqlite_exec!`; the effect executor calls
`Host.*!`). The hosted functions remain the substrate regardless. So this changes
the handler *contract*, not the engine.

Options: (A) replace — pure only (strands sagas/streaming/interleaved-I/O
handlers, which become unwritable), (B) coexist as peers (dilutes the point;
authors default to the effectful one), (C) layer — pure is sanctioned, effectful
`Request => Response` is a **marked escape hatch.**

**Choice: (C).** Matches the structural reality, keeps sagas/streaming writable,
and makes the shipped work an *evolution* rather than a throwaway:
- Extractors survive; injectors gain I/O (`from_request` → `inject`). `Req.Body`,
  `Req.JsonBody`, `Req.Model`, and bare-tag destructuring carry over.
- `Res.*` helpers grow an empty effect list (`Res.json(200, x)` →
  `Response(200, …, [])`).
- argsN grows up into the coeffect wrapper (positional resolution + the
  post-handler effect phase).
- Most examples migrate to pure; one raw effectful handler is kept as the
  escape-hatch demonstration.

---

## The request lifecycle (synthesis)

The effectful wrapper runs three phases around the pure handler:

1. **Inject (reads-before).** Resolve each positional coeffect through a shared
   per-request `Ctx` (memoized by injector identity). Any `Err` short-circuits to
   that response; the handler never runs. Serial.
2. **Run the pure handler.** Coeffect values in → `Response(code, headers, body,
   List(Effect))` out. No I/O possible (`->` enforced).
3. **Execute effects (writes-after).** DB-`Must` effects first in one
   transaction (atomic rollback); external `Must` after commit; first failure →
   that effect's effect-less `err_handler` (auto-logged). Flush the response.
   Then `Opt` effects, fire-and-forget.

Illustrative handler:

```roc
create_user : ReservedId, JsonBody(NewUser), Now -> Response
create_user = |ReservedId(id), JsonBody(new), Now(ts)|
    Response(
        201,
        [{ name: "content-type", value: "application/json" }],
        Json.to_str({ id, name: new.name, created_at: ts }),
        [
            # err_handler MUST be boxed — an unboxed lambda glues to an
            # uninvocable pointer (adversarial-review finding, 2026-07-11).
            Must(DbWrite("INSERT INTO users …", params), Box.box(|err|
                match err {
                    UniqueViolation(_) => (409, [], "user exists")
                    _ => (500, [], "internal error")
                })),
            Opt(Log("created user ${id}")),
        ],
    )
```

> **Post-design review (2026-07-11).** An independent adversarial pass confirmed
> the risky mechanics (purity enforcement, `=>`-wrapper-over-`->`-handler,
> effectful `Ctx.get!` dispatch, and boxed-closure effect lists crossing the FFI)
> are all feasible, but surfaced two unresolved design decisions promoted to
> Gate 0 in the plan: **D1** — whether effects run in the Roc wrapper (FFI
> response unchanged) or the host (FFI response gains the effect list); and **D2**
> — coeffect memo key/value safety (identity keying is likely infeasible →
> author `key : Str` + an unchecked value downcast). Effectful methods take the
> `!` suffix (`inject!`, `Ctx.get!`). See the plan's "Review outcome" section.

---

## Conventions established

- **Pure `->` is the sanctioned handler arrow.** New handlers are pure; the
  effectful `=>` `Request => Response` form is an explicitly-labelled escape
  hatch, not a peer.
- **Coeffects are nominal types with an `inject : Ctx => Try(a, Response)`
  method.** This supersedes the extractor `from_request : Request -> Try(...)`.
  Pure request-only coeffects ignore the effect capability.
- **All coeffects are explicit and positional.** Nothing ambient is handed to a
  handler — no whole-request, no whole-`:db`.
- **`Ctx.get(dep)` is the one way to express a coeffect dependency**, and it is
  always memoized per request. Non-idempotent reads (id reservation) rely on this
  for correctness.
- **Effects are inert data in a closed union**, classified `Must`/`Opt` at the
  construction site, with `Must` carrying a pure `err_handler`.
- **DB writes are transactional and ordered first**; external writes follow
  commit.

---

## What's deferred

| Feature | Note |
|---|---|
| `Custom(Box(() => Try))` app-defined effects | Closed union suffices; add only when a concrete need appears |
| Declarative coeffect deps + parallel resolution | In-body `Ctx.get` + serial chosen; declarative needed for a static slot layout and parallel reads |
| Dense static slot store (vs identity-keyed map) | Requires declarative deps + a coeffect registry; not worth it vs a pointer-keyed map |
| Outbox pattern for the dual-write gap | External-effect-after-DB-commit failure; the mature fix, not v1 |
| In-memory app-state atom | Opt-in coeffect+effect pair; owner accepts concurrency semantics |
| Streaming / SSE / WebSocket | Bodies remain fully buffered; the pure model is buffered-oriented |
| Re-frame `:dispatch` / event-chaining | Dropped — no event loop in synchronous request/response |
