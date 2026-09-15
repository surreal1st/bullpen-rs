//! Port of the `<section className="thread">` markup in
//! `projects/bullpen-night/src/client/App.tsx:1120-1276`: message rows, the
//! date/pause divider between groups, the streaming row, and the bottom
//! anchor that keeps the pane scrolled to the newest message. Also owns the
//! send path from `App.tsx:470-601` (`send()`: POST messages, the SSE
//! loop), since that state (the message list, the in-flight streaming
//! text) has to live somewhere above both the read-only list and the
//! composer that triggers it.

use crate::api;
use crate::approvals::Approvals;
use crate::bubble::Bubble;
use crate::composer::Composer;
use crate::goals_editor::GoalsModal;
use crate::memory_editor::MemoryModal;
use crate::message_time::{day_key, format_day, format_time, now_iso, parse_epoch_ms};
use crate::model_chip::ModelChip;
use crate::permissions_editor::PermissionsModal;
use crate::questions::Questions;
use crate::routines_editor::RoutinesModal;
use crate::types::{Bot, Message, Role};
use crate::working_bar::WorkingBar;
use dioxus::prelude::*;
use std::rc::Rc;

/// Owns one bot's (or room's) conversation: fetches it, renders it, and
/// drives sends. Give this a `key` unique to the open conversation at the
/// call site (see `app.rs`) - a fresh key should be a fresh component
/// instance, not a signal update, so switching what is open resets every
/// signal here for free instead of needing its own "is this a switch or a
/// refresh" logic (`isConversationSwitch` in the original - out of scope
/// while there is only ever one thread open).
///
/// `thread_id` is `Some(room.id)` when this pane is a group chat opened
/// through its owner bot (`rail.rs`'s `GroupRow`) - S1-07b's addition over
/// S1-07a, which only ever talked to a bot's default conversation.
///
/// `on_seen` (F14, S1-F-11) fires once the `/seen` call below resolves, so
/// `app.rs` can refetch the roster/room list the dot is drawn from - ported
/// from `App.tsx:758,769`'s `fetch(.../seen).then(() => loadBots())`. This
/// component has no access to those lists itself (they live above it, in
/// `app.rs`), so it can only ask for a refresh, not perform one.
#[component]
pub fn ChatPane(
    bot_id: String,
    bot_name: String,
    #[props(default)] thread_id: Option<String>,
    #[props(default)] section_ids: Vec<String>,
    // S2-09b: the full roster row for the open bot, when this pane is a
    // bot's own conversation (not a room - `app.rs` only ever has one bot
    // object to hand over there). Carries the model pin/effort the header's
    // chip and the permissions grid below it both need. `None` for a room:
    // neither the chip nor the grid is a single bot's own setting in that
    // case, so this ticket narrows both to the non-room path (see this
    // component's own `## Result` note on the gap).
    #[props(default)] bot: Option<Bot>,
    on_seen: EventHandler<()>,
) -> Element {
    let mut messages = use_signal(Vec::<Message>::new);
    let mut streaming = use_signal(|| None::<String>);
    let mut sending = use_signal(|| false);
    let mut load_error = use_signal(|| None::<String>);
    // The working bar needs the REAL conversation id (a bot's default
    // thread is created lazily server-side, so `thread_id` alone is not
    // always it) - a `Signal` rather than a plain field so `working_bar.rs`'s
    // `use_effect` re-runs once the fetch below resolves it, the same
    // reason `messages`/`streaming` below are `Signal`s the read-only
    // `Thread` takes rather than owned values.
    let mut conversation_id = use_signal(|| None::<String>);

    // F14: opening this pane is what makes it read. `thread_id` distinguishes
    // a room (owns its own `seen_at` on the `conversations` row) from a bot's
    // own default conversation (owns `last_seen_at` on the `bots` row) - see
    // `crates/server/src/routes/mod.rs`'s `mark_bot_seen` and
    // `crates/server/src/routes/rooms.rs`'s `mark_seen` for the two different
    // columns this clears. Runs once per mount (this component is remounted
    // by a fresh `key` on every switch, never patched in place - see the doc
    // above), which is exactly "on open".
    let seen_bot_id = bot_id.clone();
    let seen_thread_id = thread_id.clone();
    use_effect(move || {
        let bot_id = seen_bot_id.clone();
        let thread_id = seen_thread_id.clone();
        spawn(async move {
            let result = match &thread_id {
                Some(room_id) => api::mark_room_seen(room_id).await,
                None => api::mark_bot_seen(&bot_id).await,
            };
            if result.is_ok() {
                on_seen.call(());
            }
        });
    });

    let fetch_bot_id = bot_id.clone();
    let fetch_thread_id = thread_id.clone();
    use_effect(move || {
        let bot_id = fetch_bot_id.clone();
        let thread_id = fetch_thread_id.clone();
        spawn(async move {
            match api::fetch_conversation(&bot_id, thread_id.as_deref()).await {
                // Ported behaviour from 0.4.8 (the ticket's own callout):
                // nothing re-renders when the fetched data is identical.
                // `ConversationView` derives `PartialEq`, so this is a
                // direct compare rather than the original's
                // serialize-and-compare `sameData`.
                Ok(view) => {
                    conversation_id.set(Some(view.conversation_id.clone()));
                    if *messages.peek() != view.messages {
                        messages.set(view.messages);
                    }
                }
                Err(err) => load_error.set(Some(err)),
            }
        });
    });

    let send_bot_id = bot_id.clone();
    let send_thread_id = thread_id.clone();
    let on_send = move |text: String| {
        let bot_id = send_bot_id.clone();
        let thread_id = send_thread_id.clone();
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
            let result =
                api::send_message(&bot_id, &text, thread_id.as_deref(), |event| match event {
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
                    // F7 (S1-F-11): a run that failed used to be an `Ignored`
                    // frame - the streaming bubble went blank, the composer
                    // re-enabled, and nothing on screen said why. Whatever
                    // text arrived before the failure is kept (a partial
                    // answer is still evidence), with `error` set so
                    // `bubble.rs` renders the `.upstream-error` box under it.
                    api::StreamEvent::Error { message } => {
                        messages.write().push(Message {
                            id: format!("local-{}", now_iso()),
                            role: Role::Assistant,
                            content: assembled.clone(),
                            model: None,
                            error: Some(message),
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

    // S2-09b: a local echo of the open bot's pin/effort, seeded from the
    // `bot` prop and updated by `ModelChip`'s own `on_saved` - this pane has
    // no roster of its own to write a PATCH's result back into (that lives
    // above it, in `app.rs`), so the chip and the permissions grid read
    // their own copy rather than going stale until the next full roster
    // fetch. A known, scoped gap: the RAIL's copy (e.g. a "default" tag on
    // the bot row, if one is ever added there) still only refreshes on the
    // next roster load.
    let mut local_bot = use_signal(|| bot.clone());
    use_effect(move || {
        local_bot.set(bot.clone());
    });

    // S2-F-08 (D1): the permissions grid used to sit inline under the
    // header (`section.pane-perms`), eating the top half of the pane on
    // every bot. It now opens over the thread instead, behind this button -
    // see `permissions_editor.rs`'s `PermissionsModal`.
    let mut perms_open = use_signal(|| false);
    // S3-05: the memory pane, same placement as "Permissions" beside it in
    // `pane-head-meta` - see `memory_editor.rs`'s `MemoryModal`.
    let mut mem_open = use_signal(|| false);
    // S5-05: the routines pane, same placement again - see
    // `routines_editor.rs`'s `RoutinesModal`.
    let mut routines_open = use_signal(|| false);
    // S5b-07: the goals pane, same placement again - see
    // `goals_editor.rs`'s `GoalsModal`.
    let mut goals_open = use_signal(|| false);

    rsx! {
        div { class: "pane",
            if let Some(current) = local_bot.read().clone() {
                div { class: "pane-head",
                    div { class: "pane-head-who",
                        b { "{bot_name}" }
                    }
                    div { class: "pane-head-meta",
                        button {
                            class: "pane-perms-btn",
                            onclick: move |_| perms_open.set(true),
                            "Permissions"
                        }
                        button {
                            class: "pane-perms-btn",
                            onclick: move |_| mem_open.set(true),
                            "Memory"
                        }
                        button {
                            class: "pane-perms-btn",
                            onclick: move |_| routines_open.set(true),
                            "Routines"
                        }
                        button {
                            class: "pane-perms-btn",
                            onclick: move |_| goals_open.set(true),
                            "Goals"
                        }
                        ModelChip {
                            bot: current,
                            on_saved: move |updated: Bot| local_bot.set(Some(updated)),
                        }
                    }
                }
            }
            if *perms_open.read() {
                PermissionsModal {
                    bot_id: bot_id.clone(),
                    bot_name: bot_name.clone(),
                    on_close: move |_| perms_open.set(false),
                }
            }
            if *mem_open.read() {
                MemoryModal {
                    bot_id: bot_id.clone(),
                    bot_name: bot_name.clone(),
                    on_close: move |_| mem_open.set(false),
                }
            }
            if *routines_open.read() {
                RoutinesModal {
                    bot_id: bot_id.clone(),
                    bot_name: bot_name.clone(),
                    on_close: move |_| routines_open.set(false),
                }
            }
            if *goals_open.read() {
                GoalsModal {
                    bot_id: bot_id.clone(),
                    bot_name: bot_name.clone(),
                    on_close: move |_| goals_open.set(false),
                }
            }
            if let Some(err) = load_error.read().clone() {
                p { class: "composer-error", "{err}" }
            }
            Thread {
                messages,
                streaming,
                bot_name: bot_name.clone(),
                conversation_id,
                section_ids: section_ids.clone(),
            }
            // A paused run is the most urgent thing on screen - never folded
            // into a popover, and just above the composer, same placement
            // `App.tsx:1278-1300` gives it ("that is where every editor puts
            // the thing asking for a decision"). Approvals before questions:
            // "the blocking thing goes first" - an approval is a run frozen
            // waiting for Josh, a question is not.
            Approvals {}
            Questions { bot_id: bot_id.clone(), bot_name: bot_name.clone() }
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
    conversation_id: Signal<Option<String>>,
    #[props(default)] section_ids: Vec<String>,
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
    let now = now_iso();

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
            // Last thing inside `.thread`, above the scroll anchor, so it
            // rides the bottom the way a typing indicator does - ported
            // placement from `App.tsx:1276-1286`.
            WorkingBar { conversation_id, section_ids: section_ids.clone() }
            div { onmounted: move |evt| anchor.set(Some(evt.data())) }
        }
    }
}

/// 15+ minutes between two messages reads as "came back later" rather than
/// "still talking" - ported from `App.tsx:1151-1153`. S13a-01b: was
/// `js_sys::Date` directly; now `message_time::parse_epoch_ms`, the same
/// portable (wasm/native) parse this file's other date handling already
/// goes through, since a ms diff has no locale dependency to split on.
fn paused(prev: &Message, current: &Message) -> bool {
    let (Some(prev_ms), Some(current_ms)) = (
        parse_epoch_ms(&prev.created_at),
        parse_epoch_ms(&current.created_at),
    ) else {
        return false;
    };
    current_ms - prev_ms >= 15.0 * 60_000.0
}
