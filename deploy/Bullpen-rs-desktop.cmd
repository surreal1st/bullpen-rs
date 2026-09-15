@echo off
rem Opens the NATIVE bullpen-rs desktop client (S13a, 2026-09-15).
rem
rem Not the same thing as Bullpen-rs.cmd beside it. That one launches the
rem old ELECTRON Bullpen app and just points it at the Rust server, which
rem is why the Electron bridge (file drops, notifications, approvals badge)
rem is dead there - deferred defect D6. THIS one is the Dioxus client built
rem as a real Windows binary: no Electron, no webview shell from the TS
rem product, its own window.
rem
rem Build it with `bash scripts/build-desktop.sh` from the repo root. The
rem bundle lands under <target>/dx/client/release/windows/app/.
rem
rem BULLPEN_URL is read by the native transport (crates/client/src/
rem transport/native.rs) and every relative API path resolves against it.
rem The tailnet IP, not a .ts.net name and not the LAN address: meridian's
rem firewall drops LAN input to :4380.
set "BULLPEN_URL=http://100.119.100.103:4380"

set "BULLPEN_RS_APP=%~dp0..\target\dx\client\release\windows\app\client.exe"
if not exist "%BULLPEN_RS_APP%" (
  echo Desktop client not built yet.
  echo Run: bash scripts/build-desktop.sh
  echo Expected at: %BULLPEN_RS_APP%
  exit /b 1
)

start "" "%BULLPEN_RS_APP%"
