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
set "BULLPEN_URL=http://100.119.100.103:4380/"
start "" "%LOCALAPPDATA%\Programs\Bullpen\Bullpen.exe" --user-data-dir="%LOCALAPPDATA%\Bullpen-rs"
