#!/bin/bash
# Build the server and client for deployment.
# Builds the release server binary and the web client, then copies the client
# into dist/client/ for the server to serve.

set -euo pipefail

# Ensure we're in the project root
cd "$(dirname "$0")/.."

export PATH="/c/Users/rain/.cargo/bin:$PATH"

echo "Building server (release)..."
cargo build --release -p server

echo "Building web client (release)..."
dx build --platform web --package client --release

echo "Copying web client to dist/client/..."
mkdir -p dist/client
# The web output is under target/dx/client/release/web/public.
# 🔴 No `rm -rf` here, ever: this workspace is not backed up and Git Bash
# deletes skip the Recycle Bin. Stale hashed assets are harmless - index.html
# only references the current ones - so we copy over the top.
cp -r target/dx/client/release/web/public/* dist/client/

echo "Build complete. Server binary: target/release/bullpen"
echo "Client files: dist/client/"
