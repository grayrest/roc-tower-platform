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

# Type-check every example against the platform. The e2e/pure/ctxmemo suites
# already build their apps; this also covers `hello`, which no test builds, so
# a platform-API change that breaks an example is caught by `just check`.
check-examples: glue
    #!/usr/bin/env bash
    set -euo pipefail
    for ex in hello e2e pure ctxmemo; do
        "{{ roc }}" check "examples/$ex/main.roc"
    done

# Roc-side helper expects (platform Res/Req modules).
test-roc:
    "{{ roc }}" test platform/main.roc

# FFI round-trip tests under the counting allocator.
test-host:
    cargo test -p tower-platform-host

# End-to-end HTTP tests against the real server binary.
test-e2e:
    ROC="{{ roc }}" cargo test -p tower-platform-e2e

# All test suites.
test: test-roc test-host test-e2e

# Lint gate: clippy with warnings denied. (No rustfmt gate: the generated
# roc_platform_abi.rs is rewritten unformatted by every `just glue`.)
lint:
    cargo clippy --workspace --all-targets -- -D warnings

# Everything a commit must pass.
check: lint check-examples test
