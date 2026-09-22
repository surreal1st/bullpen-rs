#!/usr/bin/env bash
# Package a local release tree for handoff (same artifacts as ship.sh, no scp).
# Idempotent: re-run overwrites dist/bullpen-rs-<sha>/ and tarball.
set -euo pipefail

cd "$(dirname "$0")/.."
export PATH="${HOME}/.cargo/bin:${PATH}"

SHA="$(git rev-parse --short HEAD)"
PKG="dist/bullpen-rs-${SHA}"
TARBALL="dist/bullpen-rs-${SHA}.tar.gz"

need() {
  if [[ ! -e "$1" ]]; then
    echo "Missing $1 — run a successful build first (see scripts/ship.sh / gate release build)." >&2
    exit 1
  fi
}

BINARY="target/x86_64-unknown-linux-musl/release/bullpen"
CLIENT_DIR="dist/client"

need "$BINARY"
need "$CLIENT_DIR"
need "deploy/install.sh"
need "deploy/bullpen-rs.service"

rm -rf "$PKG"
mkdir -p "$PKG/client"

cp "$BINARY" "$PKG/bullpen"
chmod 755 "$PKG/bullpen"
cp -r "$CLIENT_DIR/." "$PKG/client/"
cp deploy/install.sh "$PKG/install.sh"
cp deploy/bullpen-rs.service "$PKG/bullpen-rs.service"

if [[ -d templates ]]; then
  cp -r templates "$PKG/templates"
fi
if [[ -d crates/server/w5 ]]; then
  cp -r crates/server/w5 "$PKG/w5"
fi

CHECKSUMS="$PKG/CHECKSUMS"
{
  sha256sum "$PKG/bullpen" | awk '{print $1 "  bullpen"}'
  (cd "$PKG/client" && find . -type f -print0 | LC_ALL=C sort -z | xargs -0 sha256sum | sed -E 's|^([0-9a-f]+) \*?\./|\1  client/|')
} > "$CHECKSUMS"

mkdir -p dist
tar -czf "$TARBALL" -C dist "bullpen-rs-${SHA}"

echo "Packaged: $PKG"
echo "Tarball:  $TARBALL"
echo "Verify:   (cd $PKG && sha256sum -c CHECKSUMS)"
