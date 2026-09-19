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
//! **THREADS-02: rename, reversing THREADS-01's own scope call, for a
//! reason rather than a contradiction.** THREADS-01 left `PATCH /api/
//! threads/:id` unported because `Threads.tsx` itself never calls it - that
//! was correct at the time. It stops being correct once chips are titled
//! automatically from a whole first message (THREADS-01's own doc example:
//! *"Does the chip above pick up this titl…"*): once a conversation is a
//! place Josh returns to, being able to call it something short is worth
//! having, and the route was already there, tested, waiting. Rename
//! happens IN PLACE on the chip - double-click the title, Enter commits,
//! Escape cancels, blur commits - never a modal, which would be more
//! chrome than the one short string it edits. `ondoubleclick` was checked
//! against this crate's pinned Dioxus (`dioxus-html 0.7.10`, `Cargo.lock`)
//! before assuming it wires cleanly - it does (an alias for `dblclick`,
//! `dioxus-html-0.7.10/src/events/generated.rs`), so no pencil-icon
//! fallback was needed.
//!
//! **Two traps, both checked in the source, not assumed:**
//! - `store::rename_thread` does NOT refuse an empty title - it only
//!   returns `false` (404) when the thread id itself does not exist; an
//!   empty title against a REAL id succeeds and silently blanks it. So "an
//!   empty title must not be sent at all" is enforced here, client-side,
//!   in `commit_rename` - never relying on a server refusal that does not
//!   exist for this case.
//! - `store::rename_thread` does NOT trim - it caps at 120 `chars()` and
//!   stores exactly what it is given otherwise (its own local variable is
//!   named `trimmed`, which is misleading: nothing there calls `.trim()`).
//!   `commit_rename` trims before sending, so `"  triage  "` cannot land in
//!   the database with its padding intact.
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

/// THREADS-02: commits (or silently cancels) an in-place rename. An empty
/// or whitespace-only `title` is treated as CANCEL and never reaches the
/// network at all - `store::rename_thread` does not refuse one (see this
/// module's own top doc), so a stray blank Enter would otherwise silently
/// blank a title rather than doing nothing. The title is trimmed here too,
/// since the server does not (same doc) - `"  triage  "` becomes `"triage"`
/// before it is ever sent.
///
/// Reuses `refresh_key` rather than reloading `threads` directly - the
/// SAME counter `thread.rs`'s own send-completion bump
/// (THREADS-01a/F1) and `on_pick`'s own bump already drive the one
/// reload effect in `Threads` above; a rename is a third reason that
/// effect needs to re-run, not a reason for a second reload path.
async fn commit_rename(
    thread_id: String,
    title: String,
    mut editing: Signal<Option<String>>,
    mut refresh_key: Signal<u32>,
) {
    let trimmed = title.trim();
    if !trimmed.is_empty() {
        let _ = api::rename_thread(&thread_id, trimmed).await;
        refresh_key.with_mut(|k| *k += 1);
    }
    editing.set(None);
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
    // THREADS-02: which thread (if any) is being renamed in place, and the
    // input's own live text. One slot for both, not per-chip state - only
    // one chip is ever being renamed at a time, and this is what makes
    // switching straight from editing one chip to double-clicking another
    // simply discard the first edit rather than needing its own recovery
    // path (see `commit_rename`'s callers below: a blur only ever commits
    // when `editing` STILL names the chip it fired from).
    let mut editing = use_signal(|| None::<String>);
    let mut edit_value = use_signal(String::new);

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
                            let is_editing = editing.read().as_deref() == Some(thread.id.as_str());
                            rsx! {
                                span {
                                    key: "{thread.id}",
                                    class: if is_current { "thread-chip is-on" } else { "thread-chip" },
                                    if is_editing {
                                        input {
                                            class: "thread-pick thread-pick-editing",
                                            value: "{edit_value}",
                                            autofocus: true,
                                            oninput: move |evt| edit_value.set(evt.value()),
                                            onkeydown: {
                                                let id = thread.id.clone();
                                                move |evt: KeyboardEvent| match evt.key() {
                                                    Key::Enter => {
                                                        evt.prevent_default();
                                                        spawn(commit_rename(
                                                            id.clone(),
                                                            edit_value.peek().clone(),
                                                            editing,
                                                            refresh_key,
                                                        ));
                                                    }
                                                    Key::Escape => {
                                                        evt.prevent_default();
                                                        editing.set(None);
                                                    }
                                                    _ => {}
                                                }
                                            },
                                            onblur: {
                                                let id = thread.id.clone();
                                                move |_| {
                                                    // Only this chip's OWN blur commits - Enter/
                                                    // Escape above (or switching straight to
                                                    // editing a DIFFERENT chip) already cleared
                                                    // `editing` by the time an unmount-triggered
                                                    // blur would otherwise fire here, so this
                                                    // guard is what stops a double-commit or a
                                                    // stray commit of the WRONG chip's text.
                                                    if editing.peek().as_deref() == Some(id.as_str()) {
                                                        spawn(commit_rename(
                                                            id.clone(),
                                                            edit_value.peek().clone(),
                                                            editing,
                                                            refresh_key,
                                                        ));
                                                    }
                                                }
                                            },
                                        }
                                    } else {
                                        button {
                                            class: "thread-pick",
                                            onclick: {
                                                let id = thread.id.clone();
                                                move |_| on_pick.call(id.clone())
                                            },
                                            ondoubleclick: {
                                                let id = thread.id.clone();
                                                let raw_title = thread.title.clone();
                                                move |_| {
                                                    edit_value.set(raw_title.clone());
                                                    editing.set(Some(id.clone()));
                                                }
                                            },
                                            span { class: "thread-pick-title", "{label}" }
                                            i { "{thread.message_count}" }
                                        }
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
