//! Dioxus client. Web by default; desktop and mobile behind features.
mod api;
mod app;
mod approvals;
mod avatar;
mod bubble;
mod composer;
// S13a-02: only compiled with `--features desktop` (mutually exclusive with
// the default `web` feature in practice - see that module's doc).
#[cfg(feature = "desktop")]
mod desktop;
mod events;
mod goals_editor;
mod markdown;
mod memory_editor;
mod message_time;
mod model_chip;
mod permissions_editor;
mod questions;
mod rail;
mod room_picker;
mod routines_editor;
mod settings;
mod slack_card;
mod thread;
mod transport;
mod types;
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
