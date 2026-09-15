#!/usr/bin/env bash
# Build only the web client and refresh dist/client/. Used when the server
# crates are mid-edit in the shared tree and a full build.sh would fail on
# them. `dx` has no --manifest-path, so this script owns the cd.
set -euo pipefail
cd "$(dirname "$0")/.."
export PATH="/c/Users/rain/.cargo/bin:$PATH"
dx build --platform web --package client --release
mkdir -p dist/client
# 🔴 No `rm -rf`: this workspace is not backed up. Copy over the top.
# `dx` writes into $CARGO_TARGET_DIR when it is exported; copying from a
# hardcoded `target/` then ships a STALE client silently (trap found in S5-F-02).
cp -r "${CARGO_TARGET_DIR:-target}"/dx/client/release/web/public/* dist/client/
echo "client built; dist/client refreshed"
grep -n "assets/" dist/client/index.html
