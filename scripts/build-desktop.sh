#!/usr/bin/env bash
# Build the Windows desktop bundle and report where the binary landed.
# Mirrors build-client.sh's shape for the desktop platform.
set -euo pipefail
cd "$(dirname "$0")/.."
export PATH="/c/Users/rain/.cargo/bin:$PATH"
dx bundle --platform desktop --release --package client --no-default-features --features desktop

# 🔴 S5-F-02's lesson (build-client.sh already carries it, this script
# reuses it): `dx` writes into $CARGO_TARGET_DIR when it is exported, so a
# hardcoded `target/` here would silently report a STALE binary instead of
# the one just built.
OUT_DIR="${CARGO_TARGET_DIR:-target}/dx/client/release/windows/app"
echo "desktop bundle built"
echo "binary: $(find "$OUT_DIR" -maxdepth 1 -iname '*.exe' | head -1)"
