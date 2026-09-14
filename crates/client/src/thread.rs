//! Port of the `<section className="thread">` markup in
//! `projects/bullpen-night/src/client/App.tsx:1120-1276`: message rows, the
//! date/pause divider between groups, the streaming row, and the bottom
//! anchor that keeps the pane scrolled to the newest message. Also owns the
//! send path from `App.tsx:470-601` (`send()`: POST messages, the SSE
//! loop), since that state (the message list, the in-flight streaming
//! text) has to live somewhere above both the read-only list and the
//! composer that triggers it.

use crate::api;
use crate::bubble::Bubble;
use crate::composer::Composer;
use crate::message_time::{day_key, format_day, format_time, now_iso};
use crate::types::{Message, Role};
use dioxus::prelude::*;
use js_sys::Date;
use std::rc::Rc;

/// Owns one bot's conversation: fetches it, renders it, and drives sends.
/// Give this a `key: "{bot_id}"` at the call site (see `app.rs`) - a fresh
/// `bot_id` should be a fresh component instance, not a signal update, so
/// switching bots resets every signal here for free instead of needing its
/// own "is this a switch or a refresh" logic (`isConversationSwitch` in the
/// original - out of scope while there is only ever one thread open).
#[component]
pub fn ChatPane(bot_id: String, bot_name: String) -> Element {
    let mut messages = use_signal(Vec::<Message>::new);
    let mut streaming = use_signal(|| None::<String>);
    let mut sending = use_signal(|| false);
    let mut load_error = use_signal(|| None::<String>);

    let fetch_bot_id = bot_id.clone();
    use_effect(move || {
        let bot_id = fetch_bot_id.clone();
        spawn(async move {
            match api::fetch_conversation(&bot_id).await {
                // Ported behaviour from 0.4.8 (the ticket's own callout):
                // nothing re-renders when the fetched data is identical.
                // `ConversationView` derives `PartialEq`, so this is a
                // direct compare rather than the original's
                // serialize-and-compare `sameData`.
                Ok(view) => {
                    if *messages.peek() != view.messages {
                        messages.set(view.messages);
                    }
                }
                Err(err) => load_error.set(Some(err)),
            }
        });
    });

    let send_bot_id = bot_id.clone();
    let on_send = move |text: String| {
        let bot_id = send_bot_id.clone();
        messages.write().push(Message {
            id: format!("local-{}", now_iso()),
            role: Role::User,
            content: text.clone(),
            model: None,
            error: None,
            created_at: now_iso(),
        });
        streaming.set(Some(String::new()));
        sending.set(true);
        spawn(async move {
            let mut assembled = String::new();
            let result = api::send_message(&bot_id, &text, |event| match event {
                api::StreamEvent::Delta { text } => {
                    assembled.push_str(&text);
                    streaming.set(Some(assembled.clone()));
                }
                api::StreamEvent::Done { model } => {
                    messages.write().push(Message {
                        id: format!("local-{}", now_iso()),
                        role: Role::Assistant,
                        content: assembled.clone(),
                        model,
                        error: None,
                        created_at: now_iso(),
                    });
                    streaming.set(None);
                }
                api::StreamEvent::Run { .. } | api::StreamEvent::Ignored => {}
            })
            .await;
            if let Err(err) = result {
                streaming.set(None);
                load_error.set(Some(err));
            }
            sending.set(false);
        });
    };

    rsx! {
        div { class: "pane",
            if let Some(err) = load_error.read().clone() {
                p { class: "composer-error", "{err}" }
            }
            Thread { messages, streaming, bot_name: bot_name.clone() }
            Composer { bot_name: bot_name.clone(), disabled: *sending.read(), on_send }
        }
    }
}

/// The read-only pane: empty state, message rows with a divider whenever
/// the calendar day changes or the conversation paused 15+ minutes
/// (`App.tsx:1135-1161`), the streaming row, and a bottom anchor that
/// scrolls into view on growth.
///
/// Takes `Signal`s rather than owned values on purpose: the auto-scroll
/// effect below has to re-run every time the message list or the streaming
/// text grows, and Dioxus's `use_effect` only tracks dependencies it
/// actually reads through a `Signal` - a plain `Vec<Message>` prop changing
/// between renders would not re-trigger it.
#[component]
fn Thread(
    messages: Signal<Vec<Message>>,
    streaming: Signal<Option<String>>,
    bot_name: String,
) -> Element {
    let mut anchor = use_signal(|| None::<Rc<MountedData>>);

    use_effect(move || {
        // Reading both through their signals is what makes this effect
        // re-run on every append, not just on first mount.
        let _ = messages.read().len();
        let _ = streaming.read().as_ref().map(|s| s.len());
        if let Some(el) = anchor.read().clone() {
            spawn(async move {
                let _ = el.scroll_to(ScrollBehavior::Instant).await;
            });
        }
    });

    let msgs = messages.read();
    let live_text = streaming.read().clone();
    let now = Date::new_0();

    // A date/pause divider is a property of a PAIR of messages, so this is
    // computed once here rather than inside the `rsx!` loop below - same
    // reason the original computes `newDay`/`paused` as plain variables
    // before returning JSX.
    let rows: Vec<(bool, Message)> = msgs
        .iter()
        .enumerate()
        .map(|(i, m)| {
            let mark = match i.checked_sub(1).map(|p| &msgs[p]) {
                None => true,
                Some(prev) => {
                    day_key(&prev.created_at) != day_key(&m.created_at) || paused(prev, m)
                }
            };
            (mark, m.clone())
        })
        .collect();

    rsx! {
        div { class: "thread",
            if msgs.is_empty() && live_text.is_none() {
                p { class: "empty", "Ask {bot_name} something." }
            }
            for (mark , m) in rows {
                // `key` must sit on the first node of the block; some rows
                // have a divider ahead of the bubble and some do not, so
                // this wraps both in one keyed node. `display: contents`
                // takes the wrapper back out of layout, so `.thread`'s flex
                // algorithm still sees the divider and the row as its own
                // direct children (`.timemark`'s `align-self: center` and
                // `.row`'s `min-width: 0` both depend on that).
                div { key: "{m.id}", style: "display: contents;",
                    if mark {
                        div { class: "timemark", role: "separator",
                            "{format_day(&m.created_at, &now)} {format_time(&m.created_at)}"
                        }
                    }
                    Bubble { message: m, bot_name: bot_name.clone() }
                }
            }
            if let Some(text) = live_text {
                Bubble {
                    message: Message {
                        id: "local-stream".to_string(),
                        role: Role::Assistant,
                        content: text,
                        model: None,
                        error: None,
                        created_at: now_iso(),
                    },
                    bot_name: bot_name.clone(),
                    live: true,
                }
            }
            div { onmounted: move |evt| anchor.set(Some(evt.data())) }
        }
    }
}

/// 15+ minutes between two messages reads as "came back later" rather than
/// "still talking" - ported from `App.tsx:1151-1153`.
fn paused(prev: &Message, current: &Message) -> bool {
    let prev_ms = Date::new(&wasm_bindgen::JsValue::from_str(&prev.created_at)).get_time();
    let current_ms = Date::new(&wasm_bindgen::JsValue::from_str(&current.created_at)).get_time();
    if prev_ms.is_nan() || current_ms.is_nan() {
        return false;
    }
    current_ms - prev_ms >= 15.0 * 60_000.0
}
