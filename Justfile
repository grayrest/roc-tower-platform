# tower-platform build & test tasks. Run `just` to list recipes.
#
# The Roc toolchain locations can be overridden per invocation:
#   ROC=/path/to/roc just build

roc := env("ROC", home_directory() / "Repositories/roc/zig-out/bin/roc")
rust_glue := env("RUST_GLUE", home_directory() / "Repositories/roc/src/glue/src/RustGlue.roc")

# Platform target directory expected by `roc build` for the current host.
target_dir := if os() + "/" + arch() == "macos/aarch64" { "arm64mac" } else if os() + "/" + arch() == "macos/x86_64" { "x64mac" } else { error("unsupported host platform: " + os() + "/" + arch()) }

default:
    @just --list

# Regenerate the Rust glue bindings — but only when an input actually changed.
# The `roc glue` run takes ~60s, and its output is a pure function of three
# things: the roc compiler, the glue spec (RustGlue.roc), and the platform
# sources. We fingerprint those and skip the run entirely when unchanged, so
# app-only iteration (`just run` after editing an example) pays nothing here.
# On a regen, output is copied into place only when its content differs, so an
# unchanged ABI doesn't bump the file mtime and force a cargo host rebuild.
# Force a regen with: rm target/glue.fingerprint
glue:
    #!/usr/bin/env bash
    set -euo pipefail
    abi="crates/host/src/roc_platform_abi.rs"
    stamp="target/glue.fingerprint"
    fingerprint=$( { stat -f '%m %z' "{{ roc }}" "{{ rust_glue }}"; cat platform/*.roc; } | shasum -a 256 | cut -d' ' -f1 )
    if [[ -f "$abi" && -f "$stamp" && "$(cat "$stamp")" == "$fingerprint" ]]; then
        echo "glue: up to date, skipping regen ($fingerprint)"
        exit 0
    fi
    echo "glue: inputs changed, regenerating"
    mkdir -p target/glue-tmp
    "{{ roc }}" glue "{{ rust_glue }}" target/glue-tmp ./platform/main.roc
    for f in target/glue-tmp/*; do
        dest="crates/host/src/$(basename "$f")"
        cmp -s "$f" "$dest" 2>/dev/null || cp "$f" "$dest"
    done
    mkdir -p target
    echo "$fingerprint" > "$stamp"

# Build the host staticlib and install it as the platform's target input.
host: glue
    cargo build --release -p tower-platform-host
    mkdir -p "platform/targets/{{ target_dir }}"
    cp target/release/libtower_platform_host.a "platform/targets/{{ target_dir }}/libhost.a"

# Full pipeline: glue -> host staticlib -> link a Roc app (output: ./main).
build app="examples/hello/main.roc": host
    "{{ roc }}" build "{{ app }}"

# Build and run an example app.
run app="examples/hello/main.roc": (build app)
    ./main

# Type-check every example against the platform. The e2e/ctxmemo/realworld
# suites already build their apps; this also covers `hello`, which no test
# builds, so a platform-API change that breaks an example is caught by
# `just check`.
check-examples: glue
    #!/usr/bin/env bash
    set -euo pipefail
    for ex in hello e2e ctxmemo realworld; do
        "{{ roc }}" check "examples/$ex/main.roc"
    done

# RealWorld conformance: the official Hurl suite (the spec repo's source of
# truth; the old Postman collection is retired). Needs `hurl` (brew install
# hurl) and network for the first spec download. NOT part of `just check` —
# run manually when touching examples/realworld.
realworld-conformance: (build "examples/realworld/main.roc")
    #!/usr/bin/env bash
    set -euo pipefail
    spec_dir="target/realworld-spec"
    if [[ ! -d "$spec_dir/hurl" ]]; then
        mkdir -p "$spec_dir"
        curl -fsSL "https://github.com/realworld-apps/realworld/archive/refs/heads/main.tar.gz" \
            | tar xz --strip-components=3 -C "$spec_dir" "realworld-main/specs/api"
    fi
    rm -f /tmp/tower-platform-realworld.db*
    PORT=8199 ./main & server=$!
    trap 'kill $server 2>/dev/null || true' EXIT
    for _ in $(seq 50); do nc -z 127.0.0.1 8199 2>/dev/null && break; sleep 0.1; done
    hurl --test --jobs 1 \
        --variable "host=http://127.0.0.1:8199" \
        --variable "uid=$(date +%s)$$" \
        "$spec_dir"/hurl/*.hurl

# Roc-side helper expects: the platform modules AND the realworld example
# (the only example with its own expects — `roc check` alone would let its
# expect blocks silently vanish).
test-roc:
    "{{ roc }}" test platform/main.roc
    "{{ roc }}" test examples/realworld/main.roc

# FFI round-trip tests under the counting allocator.
test-host:
    cargo test -p tower-platform-host

# End-to-end HTTP tests against the real server binary.
test-e2e:
    ROC="{{ roc }}" cargo test -p tower-platform-e2e

# sqlgen analyzer + scanner + emitter tests.
test-sqlgen:
    cargo test -p sqlgen

# All test suites.
test: test-roc test-host test-e2e test-sqlgen

# Regenerate the typed-SQL modules (examples/realworld/*Gen.roc) from the
# `## @sql` queries. Commit the output.
sqlgen:
    cargo build --release -p sqlgen
    ./target/release/sqlgen gen examples/realworld

# Fail if the committed *Gen.roc are stale vs a fresh regen (drift gate).
sqlgen-check:
    cargo build --release -p sqlgen
    ./target/release/sqlgen check examples/realworld

# Lint gate: clippy with warnings denied. (No rustfmt gate: the generated
# roc_platform_abi.rs is rewritten unformatted by every `just glue`.)
lint:
    cargo clippy --workspace --all-targets -- -D warnings

# Everything a commit must pass.
check: lint sqlgen-check check-examples test
