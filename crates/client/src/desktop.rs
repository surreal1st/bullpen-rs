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

use dioxus::desktop::{Config, LogicalSize, WindowBuilder};
use dioxus::prelude::Element;

/// A sensible default: wide enough for the rail + a chat pane side by side
/// without either feeling cramped, short enough to fit a 1080p display with
/// room for the taskbar. Not remembered across launches yet (S13b).
const DEFAULT_WIDTH: f64 = 1280.0;
const DEFAULT_HEIGHT: f64 = 860.0;

/// Launch `app` in a desktop window titled "Bullpen". Mirrors
/// `dioxus::launch(app)` (the web entry point `main.rs` uses when the
/// `desktop` feature is off) but through `LaunchBuilder::desktop()` so the
/// window itself can be configured first.
pub fn launch(app: fn() -> Element) {
    let window = WindowBuilder::new()
        .with_title("Bullpen")
        .with_inner_size(LogicalSize::new(DEFAULT_WIDTH, DEFAULT_HEIGHT));
    dioxus::LaunchBuilder::desktop()
        .with_cfg(Config::new().with_window(window))
        .launch(app);
}
