//! S13-01: iOS/mobile shell — same [`crate::app::App`] as web/desktop.

use dioxus::prelude::*;

/// Launch the shared app under Dioxus mobile (iOS/Android target).
pub fn launch(app: fn() -> Element) {
    dioxus::launch(app);
}
