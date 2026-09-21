//! Dioxus client. Web by default; desktop and mobile behind features.
mod api;
mod app;
mod approvals;
mod attention;
mod avatar;
mod away_card;
mod bubble;
mod composer;
mod connectors_card;
// S13a-02: only compiled with `--features desktop` (mutually exclusive with
// the default `web` feature in practice - see that module's doc).
#[cfg(feature = "desktop")]
mod desktop;
mod edit_bot;
mod events;
mod goals_editor;
// S13b-03-04: the pure half of reading a file a bot asked for - see that
// module's own doc for why nothing calls it yet. Gated on both
// `feature = "desktop"` and `not(target_arch = "wasm32")` per this
// ticket's explicit instruction, one belt-and-braces wider than
// `window_state`'s desktop-only gate - `desktop`/`web` are not mutually
// exclusive at the Cargo level, only "in practice" (this file's own
// comment above), so this module's direct `std::fs`/`regex` use stays off
// a wasm32 build even if `desktop` were ever combined with it.
//
// `allow(dead_code)`, scoped to `not(test)`: `client` is a bin-only crate
// (no `[lib]` in `Cargo.toml`), so unlike a library, `pub(crate)` does not
// make rustc treat these items as "used" - every one of them is exercised
// only by `local_read_tests.rs`, which does not exist in the ordinary
// (non-test) build. That is the correct state, not a gap - nothing calls
// this module yet, by design (see its top doc) - so the honest fix is this
// explicit, narrowly-scoped allow rather than inventing a caller.
#[cfg_attr(not(test), allow(dead_code))]
#[cfg(all(not(target_arch = "wasm32"), feature = "desktop"))]
mod local_read;
mod markdown;
mod marketplace;
mod memory_editor;
mod message_time;
mod model_chip;
mod new_bot;
mod permissions_editor;
mod questions;
mod rail;
mod room_picker;
mod routines_editor;
mod settings;
mod slack_card;
mod thread;
mod threads;
mod transport;
mod types;
// S6-VM-01: the per-bot machine card - see that module's own doc.
mod vm_card;
// S13b-02: only compiled with `--features desktop` - the pure window-state
// module `desktop.rs` wires in (restore-on-launch, geometry capture, tray).
#[cfg(feature = "desktop")]
mod window_state;
mod working_bar;

use app::App;

// S13a-02: the desktop build launches through `desktop::launch`, which
// configures the window (title, default size) before handing off to
// `dioxus-desktop`; every other build (the default `web` feature) keeps the
// plain entry point unchanged.
#[cfg(feature = "desktop")]
fn main() {
    desktop::launch(App);
}

#[cfg(not(feature = "desktop"))]
fn main() {
    dioxus::launch(App);
}
