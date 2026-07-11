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

# Regenerate the Rust glue bindings from the platform header. Glue is written
# to a temp dir and copied into place only when the content actually changed,
# so an unchanged platform doesn't bump the file's mtime and force cargo to
# recompile the host crate. (`roc glue` always rewrites its output.)
glue:
    mkdir -p target/glue-tmp
    "{{ roc }}" glue "{{ rust_glue }}" target/glue-tmp ./platform/main.roc
    for f in target/glue-tmp/*; do \
        dest="crates/host/src/$(basename "$f")"; \
        cmp -s "$f" "$dest" 2>/dev/null || cp "$f" "$dest"; \
    done

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
check: lint test
