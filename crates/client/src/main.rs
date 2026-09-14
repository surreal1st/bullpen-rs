//! Dioxus client. Web by default; desktop and mobile behind features.
mod api;
mod app;
mod avatar;
mod bubble;
mod composer;
mod events;
mod markdown;
mod message_time;
mod rail;
mod room_picker;
mod thread;
mod types;
mod working_bar;

use app::App;

fn main() {
    dioxus::launch(App);
}
