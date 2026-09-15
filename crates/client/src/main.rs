//! Dioxus client. Web by default; desktop and mobile behind features.
mod api;
mod app;
mod approvals;
mod avatar;
mod bubble;
mod composer;
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

fn main() {
    dioxus::launch(App);
}
