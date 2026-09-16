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
rem 2026-09-16: HTTPS, not the plain tailnet IP, and not for tidiness.
rem selkies (the VM screen inside a bot's container) calls
rem `window.isSecureContext` and refuses outright over plain http, because
rem WebCodecs requires it: "This application requires a secure connection
rem (HTTPS)." Tailscale Serve terminates TLS on meridian at :8452 and
rem proxies to 127.0.0.1:4380 - tailnet only, same exposure as before, with
rem a real cert. The per-bot screen does not render without this.
set "BULLPEN_URL=https://meridian.tail74afb5.ts.net:8452"

set "BULLPEN_RS_APP=%~dp0..\target\dx\client\release\windows\app\client.exe"
if not exist "%BULLPEN_RS_APP%" (
  echo Desktop client not built yet.
  echo Run: bash scripts/build-desktop.sh
  echo Expected at: %BULLPEN_RS_APP%
  exit /b 1
)

start "" "%BULLPEN_RS_APP%"
