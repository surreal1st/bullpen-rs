//! Dioxus client. Web by default; desktop and mobile behind features.
mod app;
mod avatar;
mod rail;
mod types;

use app::App;

fn main() {
    dioxus::launch(App);
}
