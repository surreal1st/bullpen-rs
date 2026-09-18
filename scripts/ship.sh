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

MERIDIAN_HOST="meridian"
BULLPEN_HOME="/home/bullpen"
BULLPEN_RS_HOME="$BULLPEN_HOME/bullpen-rs"
# Staged in the ssh user's own home: /home/bullpen is 750 bullpen:bullpen, so the
# transfer user cannot create anything under it. install.sh (root) reads from
# wherever it sits, then installs into $BULLPEN_RS_HOME.
# A fresh directory per ship: `scp -r` into an existing dir nests a second
# `client/` inside it and stale files would poison the tree hash.
INCOMING="/home/rainmade/bullpen-rs-incoming/$(date +%Y%m%d-%H%M%S)"

echo "Checking zig availability..."
zig version

echo "Checking for x86_64-unknown-linux-musl target..."
if ! rustup target list --installed | grep -q x86_64-unknown-linux-musl; then
  echo "Adding x86_64-unknown-linux-musl target..."
  rustup target add x86_64-unknown-linux-musl
fi

echo "Building Linux binary with zigbuild (release)..."
if cargo zigbuild --release -p server --target x86_64-unknown-linux-musl; then
  echo "✓ zigbuild succeeded"
else
  echo "✗ zigbuild failed"
  exit 1
fi

echo "Building web client (release)..."
if dx build --platform web --package client --release; then
  echo "✓ client build succeeded"
else
  echo "✗ client build failed"
  exit 1
fi

echo "Refreshing dist/client..."
mkdir -p dist/client
cp -r target/dx/client/release/web/public/* dist/client/

BINARY="target/x86_64-unknown-linux-musl/release/bullpen"
CLIENT_DIR="dist/client"

# Calculate checksums for all artifacts (F8, F14)
echo ""
echo "Calculating checksums..."
BINARY_SHA=$(sha256sum "$BINARY" | awk '{print $1}')
echo "Binary SHA256: $BINARY_SHA"

# Verify client files as a tree (F14). Hash from INSIDE the directory so the
# paths beside each digest are relative: sha256sum prints the path it was given,
# so hashing from two different roots can never agree. The first real ship
# (2026-09-14) failed on exactly that with every byte intact.
# Windows sha256sum marks binary mode as `<hash> *./path`; Linux prints
# `<hash>  ./path`. Normalise the marker or the listings never hash the same.
CLIENT_SHA=$(cd "$CLIENT_DIR" && find . -type f -print0 | LC_ALL=C sort -z | xargs -0 sha256sum | sed -E 's|^([0-9a-f]+) \*?\./|\1  ./|' | sha256sum | awk '{print $1}')
echo "Client tree SHA256: $CLIENT_SHA"

# Create a checksums file that install.sh verifies with `sha256sum -c` from
# INCOMING (F15): the binary, then every client file as `client/<relative>`.
CHECKSUMS_FILE="$(mktemp)"
echo "$BINARY_SHA  bullpen" > "$CHECKSUMS_FILE"
(cd "$CLIENT_DIR" && find . -type f -print0 | LC_ALL=C sort -z | xargs -0 sha256sum | sed -E 's|^([0-9a-f]+) \*?\./|\1  client/|') >> "$CHECKSUMS_FILE"

# Create incoming directory on meridian and transfer (F8)
echo ""
echo "Preparing transfer to $MERIDIAN_HOST:$INCOMING..."
ssh "$MERIDIAN_HOST" "mkdir -p $INCOMING"

echo "Transferring binary..."
scp -q "$BINARY" "$MERIDIAN_HOST:$INCOMING/bullpen"

echo "Transferring client files..."
scp -rq "$CLIENT_DIR" "$MERIDIAN_HOST:$INCOMING/client"

echo "Transferring deploy files (F8)..."
scp -q "deploy/install.sh" "$MERIDIAN_HOST:$INCOMING/install.sh"
scp -q "deploy/bullpen-rs.service" "$MERIDIAN_HOST:$INCOMING/bullpen-rs.service"

echo "Transferring checksums (F14, F15)..."
scp -q "$CHECKSUMS_FILE" "$MERIDIAN_HOST:$INCOMING/CHECKSUMS"

printf 'Checksum manifest preserved at: %s\n' "$CHECKSUMS_FILE"

# Verify checksums on meridian
echo ""
echo "Verifying checksums on $MERIDIAN_HOST..."
REMOTE_BINARY_SHA=$(ssh "$MERIDIAN_HOST" "sha256sum $INCOMING/bullpen" | awk '{print $1}')
if [[ "$BINARY_SHA" == "$REMOTE_BINARY_SHA" ]]; then
  echo "✓ Binary checksum verified: $BINARY_SHA"
else
  echo "✗ Binary checksum mismatch!"
  echo "  Local:  $BINARY_SHA"
  echo "  Remote: $REMOTE_BINARY_SHA"
  exit 1
fi

# Verify client tree checksum on meridian (F14)
echo "Verifying client tree checksum..."
REMOTE_CLIENT_SHA=$(ssh "$MERIDIAN_HOST" "cd $INCOMING/client && find . -type f -print0 | LC_ALL=C sort -z | xargs -0 sha256sum | sha256sum" | awk '{print $1}')
if [[ "$CLIENT_SHA" == "$REMOTE_CLIENT_SHA" ]]; then
  echo "✓ Client checksum verified: $CLIENT_SHA"
else
  echo "✗ Client checksum mismatch!"
  echo "  Local:  $CLIENT_SHA"
  echo "  Remote: $REMOTE_CLIENT_SHA"
  exit 1
fi

echo ""
echo "✓ All artifacts transferred and verified"
echo ""
echo "To complete the installation, run on $MERIDIAN_HOST as root:"
echo ""
echo "  sudo bash $INCOMING/install.sh"
echo ""
echo "This will:"
echo "  - Install the binary to $BULLPEN_RS_HOME/bullpen"
echo "  - Install client files to $BULLPEN_RS_HOME/client/"
echo "  - Install the systemd unit to /etc/systemd/system/bullpen-rs.service"
echo "  - Enable and start the service on port 4380"
echo ""
echo "After installation, verify with:"
echo "  curl -s http://localhost:4380/api/auth/status"
echo ""
