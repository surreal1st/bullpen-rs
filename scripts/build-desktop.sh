#!/usr/bin/env bash
# Build the Windows desktop bundle and report where the binary landed.
# Mirrors build-client.sh's shape for the desktop platform.
set -euo pipefail
cd "$(dirname "$0")/.."
# Desktop is a Windows binary: use Josh's Windows dx/rust (Git Bash /c/… or WSL /mnt/c/…).
if [[ -x /mnt/c/Users/rain/.cargo/bin/dx.exe ]]; then
  export PATH="/mnt/c/Users/rain/.cargo/bin:$PATH"
  DX=dx.exe
elif [[ -x /c/Users/rain/.cargo/bin/dx ]]; then
  export PATH="/c/Users/rain/.cargo/bin:$PATH"
  DX=dx
else
  echo "Missing Windows dioxus CLI (dx). Install: cargo install dioxus-cli" >&2
  exit 1
fi
"$DX" bundle --platform desktop --release --package client --no-default-features --features desktop

# 🔴 S5-F-02's lesson (build-client.sh already carries it, this script
# reuses it): `dx` writes into $CARGO_TARGET_DIR when it is exported, so a
# hardcoded `target/` here would silently report a STALE binary instead of
# the one just built.
OUT_DIR="${CARGO_TARGET_DIR:-target}/dx/client/release/windows/app"
NSIS_DIR="${CARGO_TARGET_DIR:-target}/dx/client/bundle/windows/nsis"
echo "desktop bundle built"
echo "binary: $(find "$OUT_DIR" -maxdepth 1 -iname '*.exe' | head -1)"
if compgen -G "$NSIS_DIR"/*-setup.exe >/dev/null; then
  echo "installer: $(find "$NSIS_DIR" -maxdepth 1 -iname '*-setup.exe' | head -1)"
fi
