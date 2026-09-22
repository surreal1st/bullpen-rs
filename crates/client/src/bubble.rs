//! Port of `projects/bullpen-night/src/client/MessageRow.tsx:1-60` (why a
//! bubble and nothing else) and `:160-215` (the bubble markup itself).
//!
//! There are only ever two speakers, the bot is already named in the
//! header, and the side a bubble sits on says who is talking - so this
//! renders a bubble and little else: no avatar, no name, no per-message
//! timestamp (`thread.rs` draws that between groups instead), no model
//! badge, no attachment, no reactions, no hover toolbar. S12-04b adds a
//! minimal read-aloud control on assistant bubbles when the browser can
//! speak; full hover toolbar stays out of scope.
//! are all real rows in the 700-line original; none of them are in scope
//! for S1-07a (see the ticket's Source/Target lists).
//!
//! F7 (S1-F-11) is the one exception: `message.error` now renders as
//! `.upstream-error` (ported from `MessageRow.tsx:207-220`), since a failed
//! run used to vanish - no bubble, no error, the composer just re-enabled
//! itself. The TS original also has a `.withheld` branch for a hold BY
//! Bullpen (not the same thing as an upstream failure - see that file's own
//! comment) and paraphrases the raw error through `friendlyUpstreamError`;
//! neither is ported here (out of this ticket's scope) - every error is
//! shown as an upstream failure, verbatim.

use crate::markdown::Markdown;
use crate::message_time::format_full;
use crate::types::{Message, Role};
use crate::voice::{can_speak, speak, stop_speaking};
use dioxus::prelude::*;

#[component]
pub fn Bubble(
    message: Message,
    bot_name: String,
    #[props(default = false)] live: bool,
    #[props(default)] bot_voice: Option<String>,
) -> Element {
    let is_user = message.role == Role::User;
    let row_class = if is_user {
        "row row-user"
    } else {
        "row row-assistant"
    };
    let who = if is_user { "You" } else { bot_name.as_str() };
    let title = format!("{who} · {}", format_full(&message.created_at));
    let empty = message.content.is_empty();
    let mut speaking = use_signal(|| false);
    let can_read = !is_user && !empty && !live && can_speak();
    let content_for_speak = message.content.clone();
    let voice_for_speak = bot_voice.clone();

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
                    if let Some(error) = message.error.clone() {
                        div { class: "upstream-error",
                            span { class: "upstream-error-label", "The model call failed. Upstream said:" }
                            pre { class: "mono", "{error}" }
                        }
                    }
                }
                if can_read {
                    button {
                        class: "bubble-speak pane-perms-btn",
                        r#type: "button",
                        "aria-label": if *speaking.read() { "Stop reading aloud" } else { "Read aloud" },
                        onclick: move |_| {
                            if *speaking.read() {
                                stop_speaking();
                                speaking.set(false);
                                return;
                            }
                            speaking.set(true);
                            let text = content_for_speak.clone();
                            let voice = voice_for_speak.clone();
                            speak(
                                &text,
                                Some(Box::new(move || speaking.set(false))),
                                voice.as_deref(),
                            );
                        },
                        if *speaking.read() { "Stop" } else { "Read aloud" }
                    }
                }
            }
        }
    }
}
