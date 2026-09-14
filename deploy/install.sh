#!/bin/bash
# Install bullpen-rs on meridian.
# Idempotent: creates directories, copies binary and client, installs systemd unit,
# enables and restarts the service, then verifies with a health check.
# Run this AFTER ship.sh has transferred the artifacts and verified checksums.

set -euo pipefail

BULLPEN_HOME="/home/bullpen"
BULLPEN_RS_HOME="$BULLPEN_HOME/bullpen-rs"
INCOMING="$BULLPEN_RS_HOME/incoming"

# Verify we're root
if [[ $EUID -ne 0 ]]; then
  echo "Error: This script must be run as root (or via sudo)"
  exit 1
fi

echo "Installing Bullpen-rs..."

# Create directories if they don't exist
mkdir -p "$BULLPEN_RS_HOME"
mkdir -p "$BULLPEN_RS_HOME/client"

# Verify incoming artifacts exist
if [[ ! -f "$INCOMING/bullpen" ]]; then
  echo "Error: $INCOMING/bullpen not found"
  exit 1
fi
if [[ ! -d "$INCOMING/client" ]]; then
  echo "Error: $INCOMING/client not found"
  exit 1
fi

# Copy binary (make it executable)
echo "Installing binary..."
install -o bullpen -g bullpen -m 755 "$INCOMING/bullpen" "$BULLPEN_RS_HOME/bullpen"

# Copy client files
echo "Installing client..."
cp -r "$INCOMING/client/"* "$BULLPEN_RS_HOME/client/"
chown -R bullpen:bullpen "$BULLPEN_RS_HOME/client"

# Install systemd unit
echo "Installing systemd unit..."
install -o root -g root -m 644 "deploy/bullpen-rs.service" /etc/systemd/system/bullpen-rs.service

# Reload systemd daemon
echo "Reloading systemd daemon..."
systemctl daemon-reload

# Enable and restart the service
echo "Enabling and starting bullpen-rs..."
systemctl enable bullpen-rs.service
systemctl restart bullpen-rs.service

# Wait a moment for the service to start
sleep 2

# Health check
echo "Verifying service health..."
if curl -fsS http://localhost:4380/api/auth/status >/dev/null; then
  echo "✓ Bullpen-rs is running and responding to health checks"
  exit 0
else
  echo "✗ Health check failed. Check logs with: journalctl -u bullpen-rs.service -n 50"
  exit 1
fi
