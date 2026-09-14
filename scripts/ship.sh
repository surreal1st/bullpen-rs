#!/bin/bash
# Ships the built artifact (server binary + client) to meridian.
#
# The contract: the build happens HERE, meridian only runs what it is handed.
# An uncapped build on that box once saturated it and took every service down.
#
# The transfer is verified by sha256 on both ends. A silent transcode has
# corrupted a file move between these two machines before, and the exit code
# of a copy command would not have caught it.

set -euo pipefail

cd "$(dirname "$0")/.."

export PATH="/c/Users/rain/AppData/Local/Microsoft/WinGet/Packages/zig.zig_Microsoft.Winget.Source_8wekyb3d8bbwe/zig-x86_64-windows-0.16.0:$PATH"
export PATH="/c/Users/rain/.cargo/bin:$PATH"

echo "Checking zig availability..."
zig version

echo "Checking for x86_64-unknown-linux-musl target..."
if ! rustup target list --installed | grep -q x86_64-unknown-linux-musl; then
  echo "Adding x86_64-unknown-linux-musl target..."
  rustup target add x86_64-unknown-linux-musl
fi

echo "Building Linux binary with zigbuild (release)..."
if cargo zigbuild --release --manifest-path /d/rainmade/projects/bullpen-rs/Cargo.toml -p server --target x86_64-unknown-linux-musl; then
  echo "✓ zigbuild succeeded"
else
  echo "✗ zigbuild failed"
  exit 1
fi

echo "Creating artifact tarball..."
mkdir -p artifact
tar czf artifact/bullpen-rs.tgz \
  target/x86_64-unknown-linux-musl/release/bullpen \
  dist/client/

echo "Artifact created: artifact/bullpen-rs.tgz"
echo ""
echo "# TODO S14: scp, smoke boot, restart"
