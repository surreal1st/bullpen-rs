//! Dioxus client. Web by default; desktop and mobile behind features.
use dioxus::prelude::*;

fn main() {
    dioxus::launch(App);
}

#[allow(non_snake_case)]
fn App() -> Element {
    rsx! {
        div { "Bullpen" }
    }
}
