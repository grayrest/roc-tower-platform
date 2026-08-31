# UPSTREAM ISSUE (roc-lang/roc): deriving a record parser crashes when a field's nominal supplies `parser_for`

Ready to file. Body below the line is the issue text; everything above it is
our own context.

## What changed from the 2026-08-04 draft

`examples/repro/CustomFormatDrivesCustomNominalParser.roc` blamed the wrong
thing, and its name is now wrong too. Three of its claims did not survive
minimisation:

- **"custom format × custom nominal parser"** — no custom format is involved.
  `Req.json_body!` goes through `Req.json`, which calls the BUILTIN
  `Json.parse`. `JsonDec` never enters the picture. The builtin alone crashes.
- **"inside an OPTIONAL field"** — not required. `{ bio : Nullable }` crashes
  the same way as `{ bio : Try(Nullable, [Missing]) }`.
- **"the builtin `Json.parse` driving the SAME NullableStr parser builds
  fine"** — no longer true, if it ever was. `examples/realworld` and
  `roc test platform/main.roc` both fail with this exact signature today.

What IS required: a **record**, and a **field whose type is a nominal that
supplies its own `parser_for`**. Drop the record (parse straight into the
nominal) and it builds; keep the record but use a plain `Str` field and it
builds.

## Status 2026-08-28 evening: two of three fixed locally

Three distinct bugs sat in a stack, each hidden behind the one before it.

**1. `codec method result was not a named Try type`** — FIXED, local-fixes
`ee6aef3d0d`. The record-field path resolved the method by name and so carried
no instantiation; `checkedGeneratedCodecCallee` already reconstructs one from
the checked contract, it was just called after the payload reads. Hoisted.

**2. `checked generated codec contract had ambiguous subject method calls`** —
FIXED, local-fixes `cad2210f93`. This was the real realworld blocker. A record
with two fields of the same nominal records one call PER FIELD, and the entries
are byte-identical (same dispatcher, callable, subject, nested evidence). The
scan treated a second match as ambiguity. Repro is two fields, not one:

    body : { bio : Nullable, note : Nullable }

**3. `active Monotype TypeId requested for an unresolved instantiation graph
node`** — NOT fixed, and NOT codec-related. It lives in `Res.roc` and `Req.roc`,
in direct-dispatch template lowering (`methodTargetCalleeDirectAtNode` ->
`lowerDraftTemplateFromContext`). Per-module: every codec module passes; only
Res and Req fail.

Delta-minimised to an INTERACTING PAIR of expects in `Res.roc`. Each is clean
on its own and any single one can be deleted while the panic remains, but
expects #9 and #11 together are enough:

    Res.fx(DbWrite({ db: "d", sql: "insert", params: [] })).opt(Log("also")).text(200, "done")
    Res.fx_opt(Log("first")).opt(DbWrite({ ... })).text(200, "ok")

`opt` is annotated and monomorphic, so the suspect is the terminal `text`,
which `Res.roc:153` documents as deliberately UNANNOTATED (annotating it makes
`effects` come back empty — a separate bug, already worked around). Two
attempts at a standalone repro (an unannotated builder terminal, then one with
a boxed handler in the accumulated list) did NOT reproduce it, so it needs more
of Res's real structure.

**It is Debug-only.** Under a ReleaseFast compiler `roc test platform/main.roc`
passes all 114 tests; the invariant is compiled out. So it blocks DIAGNOSIS
rather than the build, in the same way the `app_for_host` bug did.

With 1 and 2 fixed, on a ReleaseFast build of local-fixes:

    roc build examples/realworld/main.roc   0 errors
    cargo test -p tower-platform-e2e        23 + 1 + 1 passed, realworld_full_journey ok
    roc test platform/main.roc              114 passed

## Why it blocks us

Three things in this repo fail on it: `examples/realworld`, `roc test
platform/main.roc`, and the phase-2 decode work. It is not specific to
`NullableStr` — any nominal with its own `parser_for` used as a record field
hits it, which is the whole point of `NullableStr`, `NullableI64` and
`JsonBool`.

Repro lives at `scratchpad/codecrepro/repro.roc` (session 4b14272c).

---

## Deriving a record parser crashes when a field's nominal supplies its own `parser_for`

`roc check` accepts the program. `roc build` segfaults in the compiler.

### Repro

Needs only basic-cli and the builtin `Json`. ~30 lines.

```roc
app [main!] {
	pf: platform "https://github.com/roc-lang/basic-cli/releases/download/0.21.0/4rAQg8kUYZ3Vksr4qMQHpaFYNiHSn9GgS7gVxghd1XYV.tar.zst",
}

import pf.OsStr exposing [OsStr]
import pf.Stdout

## A nominal that supplies its own `parser_for`.
Nullable := [Null, Val(Str)].{
	parser_for = |encoding| |state| match Json.parse_null(encoding, state) {
		Ok(rest) => Ok({ value: Nullable.Null, rest: rest })
		Err(_) => match Json.parse_str(encoding, state) {
			Ok({ value, rest }) => Ok({ value: Nullable.Val(value), rest: rest })
			Err(problem) => Err(problem)
		}
	}
}

main! : List(OsStr) => Try({}, _)
main! = |_args| {
	## Deriving a RECORD parser whose field is that nominal is the trigger.
	body : { bio : Nullable }
	body = Json.parse("{\"bio\":null}") ? |_| Failed

	bio = body.bio
	txt = match bio {
		Nullable.Val(s) => s
		Nullable.Null => "null"
	}

	Stdout.line!(txt)?
	Ok({})
}
```

```
roc check repro.roc     # No errors found in 32ms
roc build repro.roc     # Segmentation fault (SIGSEGV) in the Roc compiler.
```

### What happens

A ReleaseFast compiler segfaults with a garbage slice:

```
Segmentation fault (SIGSEGV) in the Roc compiler.
Fault address: 0x10

src/postcheck/monotype/lower.zig:1128   graphTagByName
src/postcheck/monotype/lower.zig:39643  graphParserResultNodes
src/postcheck/monotype/lower.zig:39955  prepareCustomCodecCall
src/postcheck/monotype/lower.zig:41582  prepareCustomCodecCallsAtNode
```

A Debug-built compiler names the actual cause, one frame earlier:

```
postcheck invariant violated: codec method result was not a named Try type
    src/postcheck/monotype/lower.zig:39625  graphTryPayloads
```

`graphTryPayloads` switches on the codec method's result node and requires
`.named`:

```zig
const named = switch (self.graph.content(try_node)) {
    .named => |named| named,
    .redirect, .unresolved, ... => Common.invariant("codec method result was not a named Try type"),
};
```

In release that arm is compiled out, so lowering proceeds on an unset slice
and `graphTagByName` dereferences it. The segfault is that invariant with its
check removed, which is why the release trace bottoms out two frames below the
real problem with no cause attached.

### Root cause

Traced with a Debug compiler instrumented to print the node kind and the
lookup. The failing node is a **structural** `tag_union`; the four derived
parsers lowered before it are all `.named`. And the lookup that produced it
carries no instantiation:

```
DBGTRY content=named        (x4, the derived parsers)
DBGTRY content=tag_union    <- the user's parser_for, panics here
DBGLOOKUP kind=parser instantiation=NULL
```

The chain:

1. `prepareCustomCodecCallsAtNode` (lower.zig:41813) recurses into each record
   field and calls `customCodecLookupAtNode` for a field whose node is `.named`.
2. `customCodecLookupAtNode` (39587) resolves the method by NAME:
   `lookupMethodTargetByName` -> `findMethodTargetByName` -> `methodTargetInView`
   (5734), which returns `.{ .view = view, .target = target }` and leaves
   `instantiation` at its `null` default (the field default at lower.zig:537).
   It is a registry lookup of the declaration; no call-site type is attached.
3. In `prepareCustomCodecCall` (39955) the guard
   `if (lookup.instantiation) |instantiation|` is therefore skipped, so
   `relateFunctionRequestInterface` never runs and `target_node` stays the
   method's own inferred type.
4. That inferred type is an OPEN STRUCTURAL `[Ok(...), Err(...), ..]`, because
   the nominal's `parser_for` is unannotated and nothing ever forced it to the
   nominal `Try` the codec protocol declares.
5. `graphTryPayloads` (39622) requires `.named`, hits the invariant, and in
   release derefs the unset slice.

The result nodes are read at 39954-39955 and only related to the outer parser
afterwards at 39956-39958, so the target is inspected while still structural.
The relating is deliberately partial (`relateCustomParserErrorInjection` says
it must not equate the two error rows), so simply reordering is not obviously
safe.

**The record is what makes the difference.** Parsing straight into the nominal
never reaches this code: with the same instrumentation, the record repro prints
`DBGLOOKUP` once, and the no-record variant prints it zero times. The top-level
shape is resolved through checked evidence, which carries the instantiation;
only the record-FIELD walk falls back to the name-based lookup that drops it.

### Corroboration from the checker

The checker can be made to print that structural type directly. Route the same
nominal through a second format so the two disagree, and the mismatch report
shows the inferred result is structural where the protocol wants the nominal:

```
The method parser_for has the type:
    JsonEncoding -> (JsonState -> [Err([InvalidJson(Str), ..]), Ok({ rest: JsonState, value: NullableStr }), ..])
But I need it to have the type:
    JsonDec -> (JsonDecState -> Try({ rest: JsonDecState, value: NullableStr }, _b))
```

Unification accepts the structural union, so `roc check` passes. Lowering then
demands a named `Try` and has no fallback for the structural form.

### Minimisation

Each row is one edit from the repro above, verified separately:

| variant | result |
|---|---|
| as above | **SIGSEGV** |
| field wrapped: `{ bio : Try(Nullable, [Missing]) }` | **SIGSEGV** (optionality is irrelevant) |
| no record: `body : Nullable`, parse `"null"` | builds |
| record, no custom nominal: `{ bio : Try(Str, [Missing]) }` | builds |
| a userland format instead of builtin `Json` | **SIGSEGV** (format is irrelevant) |

So the trigger is precisely a derived **record** parser over a field whose
nominal supplies `parser_for`.

### No userland workaround

Annotating `parser_for` to return a named `Try` would presumably fix it, but
the annotation cannot be written: it has to name the builtin's encoding and
cursor types, and neither is reachable from user code. `JsonState` and
`JsonEncoding` are declared inside `Builtin.roc` (`JsonState :: [Input(Str)]`,
`JsonEncoding :: [Default, CamelCase, TrailingCommas]`) and are not in scope
unqualified or as `Json.JsonState` / `Json.JsonEncoding`. Substituting rigid
variables (`enc -> (state -> Try(...))`) fails to unify with the concrete
builtin types.

### Versions

Reproduced on:

- `release-fast-40fe7ddc` — upstream `main`, no local patches
- `release-fast-5543f6e0` and `debug-5543f6e0` — the same plus unrelated local
  fixes; the Debug diagnostic above comes from this one

macOS arm64, zig 0.16.0.

### Suggested fix direction

The narrow fix is to give the record-field path the instantiation the
top-level path already has, so `relateFunctionRequestInterface` runs and the
target is related to the declared `Try` before its payloads are read.

Failing that: accept a structural `[Ok, Err]` in `graphTryPayloads` by reading
the payloads off the tag row directly rather than requiring `.named`, or reject
the structural form at check time with a real diagnostic so it never reaches
lowering. Whichever way it goes, the invariant should not be reachable from
type-checked user code.
