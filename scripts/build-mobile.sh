#!/usr/bin/env bash
# S13-01: Dioxus iOS bundle (requires macOS + Xcode + dx CLI).
set -euo pipefail
cd "$(dirname "$0")/.."

if [[ "$(uname -s)" != "Darwin" ]]; then
  echo "iOS mobile bundle must be built on macOS (Dioxus mobile / Xcode)." >&2
  exit 2
fi

export CARGO_TARGET_DIR="${CARGO_TARGET_DIR:-target}"

dx bundle --platform ios --release --package client --no-default-features --features mobile

echo "Mobile bundle completed. Open the generated Xcode project to sign and run on device."
