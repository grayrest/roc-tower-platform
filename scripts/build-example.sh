#!/usr/bin/env bash
# Build pipeline: glue generation -> host static library -> Roc app binary.
set -euo pipefail
cd "$(dirname "$0")/.."

ROC=${ROC:-$HOME/Repositories/roc/zig-out/bin/roc}
RUST_GLUE=${RUST_GLUE:-$HOME/Repositories/roc/src/glue/src/RustGlue.roc}
APP=${1:-examples/hello/main.roc}

case "$(uname -sm)" in
    "Darwin arm64") TARGET_DIR=arm64mac ;;
    "Darwin x86_64") TARGET_DIR=x64mac ;;
    *) echo "unsupported host platform: $(uname -sm)" >&2; exit 1 ;;
esac

"$ROC" glue "$RUST_GLUE" crates/host/src ./platform/main.roc

cargo build --release -p tower-platform-host

mkdir -p "platform/targets/$TARGET_DIR"
cp target/release/libtower_platform_host.a "platform/targets/$TARGET_DIR/libhost.a"

"$ROC" build "$APP"
