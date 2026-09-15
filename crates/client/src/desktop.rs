//! Desktop shell (S13a-02): launches the same [`crate::app::App`] the web
//! build renders, through `dioxus-desktop` (Dioxus 0.7's `desktop` feature,
//! already declared in `Cargo.toml`) instead of `dioxus-web`. Only in this
//! crate when the `desktop` feature is enabled (`main.rs` picks this or the
//! plain `dioxus::launch(App)` web path, never both - see that file).
//!
//! This module owns window chrome only (title, default size). The server
//! URL is NOT this module's concern: `transport::native` (S13a-01) already
//! reads `BULLPEN_URL` - default `http://100.119.100.103:4380` - for every
//! API call the desktop build makes, the same whether the window itself was
//! ever involved. There is nothing left for this file to read from that
//! variable.
//!
//! S13b-01 adds one more thing before the window opens: a silent sign-in
//! attempt, so Josh does not see the password gate on every launch the way
//! a plain native transport (no cookie persistence across processes) would
//! otherwise force - see `transport::native`'s top doc for the gap this
//! closes and why re-authenticating fresh each launch (rather than
//! persisting a session to disk) is the chosen fix.

use dioxus::desktop::{Config, LogicalSize, WindowBuilder};
use dioxus::prelude::Element;
use std::time::Duration;

/// A sensible default: wide enough for the rail + a chat pane side by side
/// without either feeling cramped, short enough to fit a 1080p display with
/// room for the taskbar. Not remembered across launches yet (S13b-02).
const DEFAULT_WIDTH: f64 = 1280.0;
const DEFAULT_HEIGHT: f64 = 860.0;

/// How long a silent sign-in attempt gets before launch proceeds without it.
/// Generous for a LAN/tailnet round trip, short enough that an unreachable
/// server never meaningfully delays the window opening - see
/// `attempt_silent_sign_in_before_launch`'s doc.
const SILENT_SIGN_IN_TIMEOUT: Duration = Duration::from_secs(5);

/// Launch `app` in a desktop window titled "Bullpen". Mirrors
/// `dioxus::launch(app)` (the web entry point `main.rs` uses when the
/// `desktop` feature is off) but through `LaunchBuilder::desktop()` so the
/// window itself can be configured first.
pub fn launch(app: fn() -> Element) {
    attempt_silent_sign_in_before_launch();
    let window = WindowBuilder::new()
        .with_title("Bullpen")
        .with_inner_size(LogicalSize::new(DEFAULT_WIDTH, DEFAULT_HEIGHT));
    dioxus::LaunchBuilder::desktop()
        .with_cfg(Config::new().with_window(window))
        .launch(app);
}

/// S13b-01: mirrors the Electron app's own `signIn()`
/// (`projects/bullpen/desktop/main.cjs`) - reads
/// `%USERPROFILE%\.bullpen\password.key` and signs in before the window
/// shows anything, so Josh never types a password into this machine's
/// desktop app. Failure here is deliberately quiet, same as `main.cjs`'s
/// own doc comment on itself: a missing key file, a rejected password, a
/// timeout, or any other error all fall through to the sign-in gate that
/// already works (`app.rs::App`'s own `auth_status` check, run
/// unconditionally once the window is up) - this function's only job is to
/// skip that gate when it safely can, never to report a problem to Josh.
///
/// Runs in a throwaway, single-purpose Tokio runtime because this is called
/// from plain synchronous `main()` before handing off to `dioxus-desktop`
/// (which brings its own long-lived runtime only once `.launch()` below is
/// called) - there is no async context yet to `.await` inside. Bounded by
/// [`SILENT_SIGN_IN_TIMEOUT`] via `tokio::time::timeout` so an unreachable
/// server can never turn into a hang: whatever the outcome, this function
/// always returns and `launch` always proceeds to open the window. See
/// `transport::native`'s top doc for why the sign-in attempt itself uses
/// its own one-off HTTP client rather than the shared one - the same
/// property that makes this throwaway runtime safe to build and drop here
/// without corrupting anything the real app uses later.
///
/// Never logs the key's contents - only the file path (via the error
/// strings `transport::native::silent_sign_in` already builds, which name
/// the path and the failure kind, never what was read from it) and the
/// outcome tag.
fn attempt_silent_sign_in_before_launch() {
    let runtime = match tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
    {
        Ok(runtime) => runtime,
        Err(e) => {
            eprintln!(
                "bullpen: could not start the startup sign-in runtime ({e}); showing the sign-in gate"
            );
            return;
        }
    };
    let outcome = runtime.block_on(async {
        tokio::time::timeout(SILENT_SIGN_IN_TIMEOUT, crate::transport::silent_sign_in()).await
    });
    match outcome {
        Ok(Ok(crate::transport::SilentSignInOutcome::SignedIn)) => {
            eprintln!("bullpen: signed in silently");
        }
        Ok(Ok(crate::transport::SilentSignInOutcome::NoKeyFile)) => {
            eprintln!("bullpen: no stored credential; the sign-in gate will ask");
        }
        Ok(Err(reason)) => {
            eprintln!("bullpen: silent sign-in refused ({reason}); the sign-in gate will ask");
        }
        Err(_) => {
            eprintln!("bullpen: silent sign-in timed out; the sign-in gate will ask");
        }
    }
}
