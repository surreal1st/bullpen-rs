//! Port of `projects/bullpen-night/src/client/WorkingBar.tsx`: who is
//! working, drawn at the bottom of a conversation (`thread.rs` places it
//! last inside `.thread`, above the scroll anchor).
//!
//! Read from the SERVER's view of runs (`GET /api/conversations/:id/working`),
//! never assembled from this tab's own SSE stream - a room round chains
//! members two, three and four from `on_run_done` with no HTTP response for
//! this tab to read, and a routine (once one exists) fires with no tab open
//! at all. The tab's own events can only ever describe the one run it
//! started; this route reads every run row, so it sees all of them.

use crate::api;
use crate::avatar::Avatar;
use crate::events::{ChangeKind, subscribe_events};
use crate::types::WorkingBot;
use dioxus::prelude::*;

#[component]
pub fn WorkingBar(
    conversation_id: Signal<Option<String>>,
    #[props(default)] section_ids: Vec<String>,
) -> Element {
    let mut working = use_signal(Vec::<WorkingBot>::new);
    // The last payload, as JSON text, so an identical answer never
    // re-renders this row - ported from the TS `last` ref. "working" fires
    // on every tool call, and a bot that runs the same tool four times in a
    // row would otherwise re-render this row four times for a picture that
    // never changed.
    let mut last = use_signal(|| "[]".to_string());

    // `Fn`, not `FnOnce`: called once on mount (via `use_effect`, below) and
    // again every time the shared `/api/events` stream reports a "working"
    // change. All three captured signals are `Copy`, so this closure is
    // `Copy` too and can be handed to both call sites without cloning.
    //
    // 🔴 `crate::transport::spawn_task`, not `dioxus::prelude::spawn`: the
    // second call site is `subscribe_events`'s callback (below), fired from
    // `events.rs`'s bare `spawn_task(run())` loop - nothing Dioxus ever
    // considers a "current scope". `spawn()` reads that scope via
    // `Runtime::current_scope_id()`, which `.unwrap()`s an empty stack there
    // and aborts the whole wasm instance on the very first "working" event,
    // silently - no panic message reaches the console (this is what the bug
    // that failed acceptance 3 turned out to be: found by instrumenting this
    // exact call with `console::log_1` and watching the "before spawn" line
    // never get an "inside" line after it). `spawn_task` has no such
    // requirement on either platform - see its doc in `transport/mod.rs` for
    // why native needs a different escape hatch than wasm's `spawn_local`.
    // The tradeoff is that a fetch in flight is not auto-cancelled if this
    // component unmounts first, same as `events.rs`'s own `run()` loop
    // already accepts.
    let reload = move || {
        let id = conversation_id.peek().clone();
        crate::transport::spawn_task(async move {
            let Some(id) = id else {
                if *last.peek() != "[]" {
                    last.set("[]".to_string());
                    working.set(Vec::new());
                }
                return;
            };
            if let Ok(next) = api::fetch_working(&id).await {
                let serialised = serde_json::to_string(&next).unwrap_or_default();
                if serialised != *last.peek() {
                    last.set(serialised);
                    working.set(next);
                }
            }
            // Nobody needs to be told the indicator is missing - same
            // "silently drop the error" posture as the TS `.catch(() =>
            // undefined)`. It is a read of something the conversation
            // already shows in other ways.
        });
    };

    // Reads `conversation_id` through the signal (not a plain prop) so this
    // re-runs once `thread.rs`'s fetch resolves the real id, not only on
    // first mount - same reason `Thread` itself takes `Signal`s rather than
    // owned values for its own auto-scroll effect.
    use_effect(move || {
        let _ = conversation_id.read();
        reload();
    });

    // Subscribed once for the life of this component. `EventsHandle` is
    // deliberately not `Clone` (so it unsubscribes exactly once), which
    // rules out `use_hook` (requires `Clone` to hand the value back out
    // every render) - `use_signal` only needs `T: 'static` to hold it, and
    // its storage is dropped with the component same as any other signal,
    // which is what actually unsubscribes.
    let _events = use_signal(move || {
        subscribe_events(move |kind| {
            if kind == ChangeKind::Working {
                reload();
            }
        })
    });

    let rows: Vec<(WorkingBot, String, &'static str)> = working
        .read()
        .iter()
        .cloned()
        .map(|bot| {
            let title = format!("{}: {}", bot.name, bot.activity);
            let class = if bot.waiting {
                "working-face is-waiting"
            } else {
                "working-face"
            };
            (bot, title, class)
        })
        .collect();

    rsx! {
        // Nothing running, nothing drawn - an empty reserved strip under
        // every idle conversation would be furniture that says nothing.
        if !rows.is_empty() {
            div {
                class: "working-bar",
                // `polite`, not `assertive`: ambient, and a screen reader
                // interrupting a sentence to announce a bot started
                // thinking would be worse than silence. The faces
                // themselves are `aria-hidden` inside `Avatar`, so the
                // `title`/`aria-label` text below is what is actually
                // announced.
                "aria-live": "polite",
                for (bot , title , class) in rows {
                    span {
                        key: "{bot.bot_id}",
                        class: "{class}",
                        title: "{title}",
                        "aria-label": "{title}",
                        Avatar {
                            id: bot.bot_id.clone(),
                            name: bot.name.clone(),
                            section_id: bot.section_id.clone(),
                            section_ids: section_ids.clone(),
                            // A bot parked on an approval is NOT working -
                            // it is stopped, waiting for Josh. Its face
                            // holds still, which is the difference worth
                            // seeing from across the room. Bite check: bind
                            // this to `bot.waiting` (drop the `!`) and the
                            // animating/still faces swap in the shot.
                            busy: !bot.waiting,
                            avatar: bot.avatar.clone(),
                            shape: bot.shape.clone(),
                            size: 24.0,
                        }
                        span { class: "working-what", "{bot.activity}" }
                    }
                }
            }
        }
    }
}
