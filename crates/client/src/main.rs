//! Dioxus client. Web by default; desktop and mobile behind features.
use dioxus::prelude::*;

fn main() {
    dioxus::launch(App);
}

fn App() -> Element {
    rsx! {
        div { "Bullpen" }
    }
}
