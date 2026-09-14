//! Port of `projects/bullpen-night/src/client/MessageRow.tsx:1-60` (why a
//! bubble and nothing else) and `:160-215` (the bubble markup itself).
//!
//! There are only ever two speakers, the bot is already named in the
//! header, and the side a bubble sits on says who is talking - so this
//! renders a bubble and nothing else: no avatar, no name, no per-message
//! timestamp (`thread.rs` draws that between groups instead), no model
//! badge, no attachment, no reactions, no hover toolbar, no voice. Those
//! are all real rows in the 700-line original; none of them are in scope
//! for S1-07a (see the ticket's Source/Target lists).

use crate::markdown::Markdown;
use crate::message_time::format_full;
use crate::types::{Message, Role};
use dioxus::prelude::*;

#[component]
pub fn Bubble(message: Message, bot_name: String, #[props(default = false)] live: bool) -> Element {
    let is_user = message.role == Role::User;
    let row_class = if is_user {
        "row row-user"
    } else {
        "row row-assistant"
    };
    let who = if is_user { "You" } else { bot_name.as_str() };
    let title = format!("{who} · {}", format_full(&message.created_at));
    let empty = message.content.is_empty();

    rsx! {
        div { class: "{row_class}",
            div { class: "bubble-col",
                div { class: "bubble", title: "{title}",
                    if !empty {
                        if is_user {
                            div { class: "text", "{message.content}" }
                        } else {
                            Markdown { text: message.content.clone() }
                        }
                    }
                    if live && empty {
                        span { class: "working", "working…" }
                    }
                }
            }
        }
    }
}
