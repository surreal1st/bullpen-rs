//! Port of `projects/bullpen-night/src/client/Threads.tsx` (79 lines) - the
//! conversations one bot has, shown as a STRIP below its header rather
//! than a third column: a bot usually has one or two conversations, and a
//! column would cost more room than it earns (the TS's own doc, carried
//! forward unchanged). The chips only render when there is MORE THAN ONE
//! thread - a bot with a single conversation shows just "New conversation",
//! no chrome at all.
//!
//! **Never mounted for a group chat.** The TS's own `App.tsx` does not
//! mount this component while a group chat is the open conversation - a
//! room is its own roster entry, not one of a bot's conversations. This
//! component follows the same rule using the SAME signal `thread.rs`
//! already had for exactly that distinction: it is only ever built inside
//! the `if let Some(current) = local_bot.read().clone() { ... }` block that
//! file already gates every other bot-only control behind (Export,
//! Archive, the model chip, the permissions grid...), never a flag
//! invented for this ticket.
//!
//! **Rename is out of scope, on purpose.** `PATCH /api/threads/:id` exists
//! and works (`crates/server/src/routes/conversations.rs::rename_thread`),
//! but `Threads.tsx` never calls it - this is a port of ONE component, and
//! that component does not expose renaming a thread. A future ticket that
//! wants it is a new affordance on `.thread-chip`, not something this one
//! owes silently.
//!
//! **Picking a thread must actually change where a message goes** - the
//! ticket's own top risk, and the most likely way to ship this looking
//! right while being wrong: `crate::api::send_message` takes its own
//! `thread_id`, so a strip that only updates what is DISPLAYED (via
//! `fetch_conversation`) without also updating what `send_message` reads
//! would let Josh watch the right conversation while every reply lands in
//! the default one. `thread.rs` closes this by making BOTH read from the
//! SAME `active_thread_id: Signal<Option<String>>` - see that file's own
//! doc on it, and on how this was verified rather than just intended.
//!
//! This component only ever tells its caller WHICH thread id was picked
//! (via `on_pick`) or created; it does not touch `send_message`/
//! `fetch_conversation` itself - that wiring lives one level up, in
//! `thread.rs`.
//!
//! `.thread-new` below is deliberately the SAME class `new_bot.rs`/
//! `thread.rs` already use for Export/Archive/Cancel/Create - checked
//! before reusing it: in this codebase (unlike the TS, where `.thread-new`
//! names specifically its "New conversation" button) that class is already
//! a generic action-button style with no meaning more specific than that,
//! so reusing it here is exactly the button look "New conversation" wants,
//! not a collision. See `crates/client/assets/settings.css`'s own top doc
//! for where `.threads-wrap`/`.threads`/`.thread-chip`/`.thread-pick`/
//! `.thread-close` (all new names, checked against every existing
//! stylesheet first) live.
//!
//! **THREADS-01a/F2: a title is capped in CSS, never truncated here.** The
//! auto-title is a conversation's first message, so a chip can carry 60+
//! characters - `Threads.tsx` itself renders `{thread.title || "Untitled"}`
//! with no truncation at all, so its own CSS (not shown to this port) must
//! be doing that work. `.thread-pick-title` (`settings.css`) caps the
//! button's width and ellipsizes there, so the label below is always the
//! REAL, untruncated string - what a copy/paste or a future tooltip would
//! see is never a Rust-side cut string standing in for the truth.

use crate::api;
use crate::types::ThreadSummary;
use dioxus::prelude::*;

/// Which thread to switch to after archiving `archived_id` - the first
/// entry in `threads` that is not the one being archived, in `threads`'
/// own order (`list_threads`'s `ORDER BY COALESCE(last_at, created_at)
/// DESC`, most-recently-active first). `None` only when `archived_id` was
/// the sole thread. Matches the TS `threads.find((t) => t.id !== id)`
/// exactly, including reading a PRE-archive snapshot rather than waiting on
/// a fresh refetch - `archive_and_advance` below captures that snapshot
/// before the archive call, the same way the TS closure closes over its own
/// `threads` state before `load()`'s refetch has landed.
fn next_after_archiving(threads: &[ThreadSummary], archived_id: &str) -> Option<String> {
    threads
        .iter()
        .find(|t| t.id != archived_id)
        .map(|t| t.id.clone())
}

async fn reload(bot_id: String, mut threads: Signal<Vec<ThreadSummary>>) {
    threads.set(api::fetch_threads(&bot_id).await.unwrap_or_default());
}

/// Port of the TS `create`: always reloads the list, regardless of whether
/// the create itself succeeded (matching the TS's own `load()` called
/// unconditionally), and only calls `on_pick` when a thread actually came
/// back.
async fn create_and_pick(
    bot_id: String,
    threads: Signal<Vec<ThreadSummary>>,
    on_pick: EventHandler<String>,
) {
    let created = api::create_thread(&bot_id).await.ok();
    reload(bot_id, threads).await;
    if let Some(thread) = created {
        on_pick.call(thread.id);
    }
}

/// Port of the TS `archive`: fires the DELETE, reloads the list, and picks
/// `next_after_archiving`'s answer from the PRE-archive snapshot.
async fn archive_and_advance(
    thread_id: String,
    bot_id: String,
    threads: Signal<Vec<ThreadSummary>>,
    on_pick: EventHandler<String>,
) {
    let previous = threads.peek().clone();
    let _ = api::archive_thread(&thread_id).await;
    reload(bot_id, threads).await;
    if let Some(next_id) = next_after_archiving(&previous, &thread_id) {
        on_pick.call(next_id);
    }
}

#[component]
pub fn Threads(
    bot_id: String,
    // The live conversation id `thread.rs`'s own fetch resolved (the TS's
    // `view?.conversationId ?? null`) - a `Signal`, not a plain `Option
    // <String>` snapshot, the same "pass the live signal, not a copy of
    // it" pattern `working_bar.rs`'s own `conversation_id` prop already
    // uses, so a chip's highlight follows without this component needing
    // to be remounted.
    current_id: Signal<Option<String>>,
    on_pick: EventHandler<String>,
    // Bumped by `thread.rs` every time `on_pick` fires (picking an
    // EXISTING chip does not otherwise change anything this component
    // would notice) - read inside the effect below so THIS reload actually
    // re-runs on that bump. A plain cloned value would not: it would only
    // ever reflect whatever it was at mount, the same reason `thread.rs`
    // itself needed `active_thread_id` to be a `Signal` and not a cloned
    // prop (see that file's own doc).
    refresh_key: Signal<u32>,
) -> Element {
    let threads = use_signal(Vec::<ThreadSummary>::new);

    let effect_bot_id = bot_id.clone();
    use_effect(move || {
        let _ = refresh_key.read();
        let bot_id = effect_bot_id.clone();
        spawn(reload(bot_id, threads));
    });

    rsx! {
        div { class: "threads-wrap",
            div { class: "threads",
                if threads.read().len() > 1 {
                    for thread in threads.read().iter().cloned() {
                        {
                            let label = if thread.title.trim().is_empty() {
                                "Untitled".to_string()
                            } else {
                                thread.title.clone()
                            };
                            let is_current = current_id.read().as_deref() == Some(thread.id.as_str());
                            rsx! {
                                span {
                                    key: "{thread.id}",
                                    class: if is_current { "thread-chip is-on" } else { "thread-chip" },
                                    button {
                                        class: "thread-pick",
                                        onclick: {
                                            let id = thread.id.clone();
                                            move |_| on_pick.call(id.clone())
                                        },
                                        span { class: "thread-pick-title", "{label}" }
                                        i { "{thread.message_count}" }
                                    }
                                    button {
                                        class: "thread-close",
                                        "aria-label": "Archive this conversation",
                                        title: "Archive this conversation",
                                        onclick: {
                                            let id = thread.id.clone();
                                            let bot_id = bot_id.clone();
                                            move |_| {
                                                spawn(archive_and_advance(id.clone(), bot_id.clone(), threads, on_pick));
                                            }
                                        },
                                        "×"
                                    }
                                }
                            }
                        }
                    }
                }
                button {
                    class: "thread-new",
                    onclick: {
                        let bot_id = bot_id.clone();
                        move |_| {
                            spawn(create_and_pick(bot_id.clone(), threads, on_pick));
                        }
                    },
                    "New conversation"
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn thread(id: &str) -> ThreadSummary {
        ThreadSummary {
            id: id.to_string(),
            bot_id: "test-bot".to_string(),
            bot_name: "Test Bot".to_string(),
            title: String::new(),
            message_count: 0,
            last_at: None,
            created_at: "2026-01-01T00:00:00Z".to_string(),
        }
    }

    #[test]
    fn next_after_archiving_picks_the_first_remaining_thread_in_order() {
        let threads = vec![thread("a"), thread("b"), thread("c")];
        assert_eq!(next_after_archiving(&threads, "a"), Some("b".to_string()));
    }

    #[test]
    fn next_after_archiving_skips_only_the_archived_one() {
        let threads = vec![thread("a"), thread("b"), thread("c")];
        assert_eq!(next_after_archiving(&threads, "b"), Some("a".to_string()));
    }

    #[test]
    fn next_after_archiving_returns_none_when_it_was_the_only_thread() {
        let threads = vec![thread("a")];
        assert_eq!(next_after_archiving(&threads, "a"), None);
    }

    #[test]
    fn next_after_archiving_returns_none_for_an_empty_list() {
        assert_eq!(next_after_archiving(&[], "a"), None);
    }
}
