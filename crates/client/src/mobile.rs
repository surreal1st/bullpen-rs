//! S13-01: iOS/mobile shell — same [`crate::app::App`] as web/desktop.

use dioxus::prelude::*;

/// Launch the shared app under Dioxus mobile (iOS/Android target).
///
/// `scripts/gate.sh` builds with `--all-features`, which enables desktop and
/// mobile together; only one `main` is linked, so this entry is dead in that
/// configuration by design.
#[allow(dead_code)]
pub fn launch(app: fn() -> Element) {
    dioxus::launch(app);
}
