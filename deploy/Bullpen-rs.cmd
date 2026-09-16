@echo off
rem Opens the installed Bullpen desktop app against bullpen-rs on meridian.
rem
rem Same app, two things different: BULLPEN_URL points it at the Rust server
rem (tailnet address; meridian's firewall drops LAN traffic to :4380, and
rem Chrome force-upgrades http on a .ts.net NAME, so it is the IP), and
rem --user-data-dir gives it its own profile, so it runs BESIDE the live
rem Bullpen window instead of just focusing it (the app holds a
rem single-instance lock keyed on that directory).
rem
rem Sign-in is silent from %USERPROFILE%\.bullpen\password.key, the same
rem password as live because the server runs on a copy of live's database.
rem 2026-09-16: HTTPS, not the plain tailnet IP, and not for tidiness.
rem selkies (the VM screen inside a bot's container) calls
rem `window.isSecureContext` and refuses outright over plain http, because
rem WebCodecs requires it: "This application requires a secure connection
rem (HTTPS)." Tailscale Serve terminates TLS on meridian at :8452 and
rem proxies to 127.0.0.1:4380 - tailnet only, same exposure as before, with
rem a real cert. The per-bot screen does not render without this.
set "BULLPEN_URL=https://meridian.tail74afb5.ts.net:8452/"
start "" "%LOCALAPPDATA%\Programs\Bullpen\Bullpen.exe" --user-data-dir="%LOCALAPPDATA%\Bullpen-rs"
