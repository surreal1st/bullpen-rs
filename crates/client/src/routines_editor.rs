//! Port of `RoutinesEditor.tsx`'s list + create/edit form (774 lines). A
//! "Routines" button in `thread.rs`'s `pane-head` opens `RoutinesModal` over
//! the thread, the same `.modal*` shell `PermissionsModal`/`MemoryModal`
//! already use.
//!
//! 🔴 The TS reference's own edit form only ever PATCHes `prompt` (+ the
//! tool fields this ticket skips) - `name`/`schedule` are create-only there.
//! This ticket's Design bullet lists "create/edit form (name, prompt,
//! schedule text...)" as one shape for both, and the Rust route already
//! accepts all three on `PATCH` (`UpdateRoutineBody`), so the edit form here
//! covers all three - a deliberate widening past the TS UI, not a miss.
//!
//! S5-F-02 (F5): schedule "live description" used to be a small,
//! client-only, best-effort mirror of `crates/server/src/schedule.rs`'s
//! grammar (`preview_schedule`, deleted) - it had no week cap, so "every
//! 200 hours" previewed green right up to the 400 the server actually gave
//! on submit (`reviews/S5-R.md` F5). The create/edit form's schedule field
//! now debounces (~300ms) into `api::preview_schedule`
//! (`POST /api/routines/preview`) - the SAME parser the create/patch
//! routes use, so the preview can never show something the server would
//! reject (`new_preview`/`edit_preview` below). `Routine.schedule_text`
//! (see `types.rs`'s doc) is now a real server-computed description too,
//! shown in the list in place of the old raw `schedule` text.
//!
//! S5b-07 fills in the tool-kind and hook fields this doc used to call
//! "skipped entirely": a `kind` toggle (prompt/tool) with tool name + JSON
//! args when "tool" is picked, and a hook section (kind/events/match) plus
//! "Create webhook" / "Clear webhook" acting on `POST`/`DELETE
//! /api/routines/:id/hook`. The minted secret is shown exactly once,
//! per `mint_routine_hook`'s own doc in `api.rs` - `minted_hook` below is
//! the ONLY place a secret ever touches this client's state, it is never
//! written into `Routine`/`routines` (the list refetch after minting comes
//! back with `hasHook: true` and nothing else), and closing or navigating
//! away from the modal drops it for good (a plain `Signal`, not anything
//! persisted).

use crate::api;
use crate::types::{Routine, RoutineRun};
use dioxus::prelude::*;
use gloo_timers::future::TimeoutFuture;
use js_sys::Date;
use std::cell::Cell;
use std::rc::Rc;
use wasm_bindgen::JsValue;

/// The hook kinds a routine's webhook can verify against - same four named
/// services plus "raw" (bearer-token) that `routes/hooks.rs::post_webhook`
/// switches on.
const HOOK_KINDS: &[(&str, &str)] = &[
    ("raw", "Raw (bearer token)"),
    ("github", "GitHub"),
    ("sentry", "Sentry"),
    ("linear", "Linear"),
    ("pagerduty", "PagerDuty"),
    ("slack", "Slack"),
];

/// S5c-04: the four ways a `hook_kind: "slack"` routine can fire - matches
/// `store::slack::SLACK_TRIGGER_KINDS` on the server. Unlike every other
/// hook kind, `hook_events` here is not a free-typed comma list (there is
/// nothing per-delivery to enumerate the way GitHub's event names are) - it
/// is exactly one of these four, so the edit form below swaps the free-text
/// events `input` for a `select` over this list when `slack` is picked, and
/// that select writes its single choice straight into `edit_hook_events`
/// (still a plain `String` - `save_edit`'s existing comma-split-and-filter
/// turns one value with no commas in it into a one-element `Vec` with no
/// changes needed there).
const SLACK_TRIGGER_KINDS: &[(&str, &str)] = &[
    ("mention", "Mention (@bot)"),
    ("keyword", "Keyword"),
    ("message", "Any message"),
    ("reaction", "Reaction"),
];

/// The modal shell, opened from `thread.rs`'s "Routines" button - reuses
/// `.modal-scrim`/`.modal`/`.modal-head`/`.modal-x` from `rail.css` plus the
/// `.routines-modal` width override in `settings.css`, same pattern as
/// `PermissionsModal`.
#[component]
pub fn RoutinesModal(bot_id: String, bot_name: String, on_close: EventHandler<()>) -> Element {
    rsx! {
        div {
            class: "modal-scrim",
            role: "presentation",
            onclick: move |_| on_close.call(()),
            div {
                class: "modal routines-modal",
                onclick: move |evt| evt.stop_propagation(),
                role: "dialog",
                "aria-modal": "true",
                "aria-label": "{bot_name} routines",
                div { class: "modal-head",
                    h2 { "{bot_name} · Routines" }
                    button {
                        class: "modal-x",
                        "aria-label": "Close",
                        onclick: move |_| on_close.call(()),
                        "×"
                    }
                }
                div { class: "modal-body",
                    RoutinesEditor { bot_id: bot_id.clone() }
                }
            }
        }
    }
}

async fn reload(bot_id: String, mut routines: Signal<Option<Vec<Routine>>>) {
    if let Ok(list) = api::fetch_routines(&bot_id).await {
        routines.set(Some(list));
    }
}

#[component]
pub fn RoutinesEditor(bot_id: String) -> Element {
    let routines = use_signal(|| None::<Vec<Routine>>);

    let mut new_name = use_signal(String::new);
    let mut new_prompt = use_signal(String::new);
    let mut new_schedule = use_signal(String::new);
    // S5b-07: "prompt" | "tool" - the create form's kind toggle.
    let mut new_kind = use_signal(|| "prompt".to_string());
    let mut new_tool = use_signal(String::new);
    let mut new_tool_args = use_signal(String::new);
    let create_error = use_signal(|| None::<String>);

    let mut editing_id = use_signal(|| None::<String>);
    let mut edit_name = use_signal(String::new);
    let mut edit_prompt = use_signal(String::new);
    let mut edit_schedule = use_signal(String::new);
    let mut edit_kind = use_signal(|| "prompt".to_string());
    let mut edit_tool = use_signal(String::new);
    let mut edit_tool_args = use_signal(String::new);
    // S5b-07: the hook section - kind (which signature the delivery must
    // carry), events (github only, comma-separated in the field, narrowed
    // to a `Vec<String>` on save) and match (a regex over the reduced
    // text). Blank means "leave the field alone" for events/match on save -
    // see the `Save` handler below and `UpdateRoutineReq`'s own doc on why
    // that needs an explicit `null` rather than just omitting the key.
    let mut edit_hook_kind = use_signal(|| "raw".to_string());
    let mut edit_hook_events = use_signal(String::new);
    let mut edit_hook_match = use_signal(String::new);
    let mut edit_error = use_signal(|| None::<String>);

    let mut runs_open = use_signal(|| None::<String>);
    let runs = use_signal(Vec::<RoutineRun>::new);
    // (routine id, message) - scoped to whichever row's "Run now" fired
    // last, so starting one routine never shows its status under every
    // OTHER row too (`shots/s5-05-runs.png` before this fix: a single
    // ungated `Option<String>` rendered inside the same `for` loop showed
    // "Started run ..." twice, once per routine, since the `if let` had no
    // id to check against).
    let run_status = use_signal(|| None::<(String, String)>);
    // S5b-07: same per-row gating as `run_status`, for mint/clear webhook
    // outcomes and errors.
    let hook_status = use_signal(|| None::<(String, String)>);
    // The ONE place a webhook secret ever lands in this client's state:
    // (routine_id, secret, url). Never written into `routines`, never
    // re-fetched - see this module's own doc above.
    let mut minted_hook = use_signal(|| None::<(String, String, String)>);

    // S5-F-02 (F5): live schedule previews, debounced into the server's own
    // `POST /api/routines/preview` (`api::preview_schedule`) rather than a
    // client-side grammar mirror. `Rc<Cell<u32>>` generation counters (not
    // `Signal<u32>`) so bumping "which request is still wanted" does not
    // itself re-trigger the effect that bumps it - same trap and same fix
    // as `settings.rs`'s `ModelPickerField` (see its own doc, B-F10).
    let mut new_preview = use_signal(|| None::<Result<String, String>>);
    let new_gen = use_hook(|| Rc::new(Cell::new(0u32)));
    use_effect(move || {
        let text = new_schedule.read().trim().to_string();
        if text.is_empty() {
            new_preview.set(None);
            return;
        }
        let my_gen = new_gen.get() + 1;
        new_gen.set(my_gen);
        let new_gen = new_gen.clone();
        spawn(async move {
            TimeoutFuture::new(300).await;
            if new_gen.get() != my_gen {
                return;
            }
            let result = api::preview_schedule(&text).await;
            if new_gen.get() != my_gen {
                return;
            }
            new_preview.set(Some(result));
        });
    });

    let mut edit_preview = use_signal(|| None::<Result<String, String>>);
    let edit_gen = use_hook(|| Rc::new(Cell::new(0u32)));
    use_effect(move || {
        // Only the routine currently being edited has a schedule field on
        // screen at all - reset rather than debounce a request for text
        // nobody can see.
        if editing_id.read().is_none() {
            edit_preview.set(None);
            return;
        }
        let text = edit_schedule.read().trim().to_string();
        if text.is_empty() {
            edit_preview.set(None);
            return;
        }
        let my_gen = edit_gen.get() + 1;
        edit_gen.set(my_gen);
        let edit_gen = edit_gen.clone();
        spawn(async move {
            TimeoutFuture::new(300).await;
            if edit_gen.get() != my_gen {
                return;
            }
            let result = api::preview_schedule(&text).await;
            if edit_gen.get() != my_gen {
                return;
            }
            edit_preview.set(Some(result));
        });
    });

    let load_bot_id = bot_id.clone();
    use_effect(move || {
        let bot_id = load_bot_id.clone();
        spawn(reload(bot_id, routines));
    });

    let Some(list) = routines.read().clone() else {
        return rsx! { p { class: "muted", "Loading routines…" } };
    };

    // S5b-07: a tool-kind routine needs a tool name, not a prompt - the
    // server's own validation splits the same way (`validate_tool_kind` in
    // `routes/routines.rs`), so the "Add routine" button mirrors it rather
    // than refusing a valid tool routine for want of a prompt nobody asked
    // for.
    let create_disabled = new_name.read().trim().is_empty()
        || new_schedule.read().trim().is_empty()
        || if *new_kind.read() == "tool" {
            new_tool.read().trim().is_empty()
        } else {
            new_prompt.read().trim().is_empty()
        };

    rsx! {
        div { class: "routines",
            if list.is_empty() {
                p { class: "muted routines-empty",
                    "Nothing scheduled. A routine runs this bot on its own, on the cheap model, whether or not you are here."
                }
            }

            for routine in list.iter().cloned() {
                {
                    let is_editing = editing_id.read().as_deref() == Some(routine.id.as_str());
                    let is_runs_open = runs_open.read().as_deref() == Some(routine.id.as_str());
                    rsx! {
                        article { key: "{routine.id}", class: if routine.active { "routine is-on" } else { "routine" },
                            div { class: "routine-top",
                                span { class: if routine.active { "routine-dot is-on" } else { "routine-dot" }, "aria-hidden": "true" }
                                b { "{routine.name}" }
                                if routine.kind == "tool" {
                                    span { class: "routine-badge",
                                        "tool: {routine.tool.clone().unwrap_or_default()}"
                                    }
                                }
                                if routine.has_hook {
                                    span { class: "routine-badge",
                                        "webhook · {routine.hook_kind}"
                                    }
                                }
                                span { class: "routine-when", "{routine.schedule_text}" }
                            }
                            p { class: "routine-prompt",
                                if routine.prompt.is_empty() {
                                    span { class: "muted", "(no phrasing set)" }
                                } else {
                                    "{routine.prompt}"
                                }
                            }
                            if let Some(reason) = routine.paused_reason.clone() {
                                p { class: "routine-stopped",
                                    b { "Stopped by Bullpen. " }
                                    "{reason}"
                                    if let Some(err) = routine.last_error.clone().filter(|e| !e.is_empty()) {
                                        span { class: "routine-stopped-why", "{err}" }
                                    }
                                }
                            }
                            div { class: "routine-meta",
                                if routine.active {
                                    span { "next {format_when(routine.next_run_at.as_deref())}" }
                                } else {
                                    span { if routine.paused_reason.is_none() { "paused" } else { "stopped" } }
                                }
                                if routine.failures > 0 && routine.active {
                                    span { class: "routine-warn",
                                        "{routine.failures} "
                                        if routine.failures == 1 { "failure" } else { "failures" }
                                        " in a row"
                                    }
                                }
                                if routine.last_run_at.is_some() {
                                    span { "last ran {format_when(routine.last_run_at.as_deref())}" }
                                }
                            }

                            if is_editing {
                                {
                                    let edit_preview = edit_preview.read().clone();
                                    let is_tool = *edit_kind.read() == "tool";
                                    rsx! {
                                        div { class: "routine-edit",
                                            input {
                                                value: "{edit_name}",
                                                "aria-label": "Routine name",
                                                oninput: move |e| edit_name.set(e.value()),
                                            }
                                            div { class: "routine-kind-toggle", role: "group", "aria-label": "Kind",
                                                button {
                                                    class: if !is_tool { "is-on" } else { "" },
                                                    onclick: move |_| edit_kind.set("prompt".to_string()),
                                                    "Prompt"
                                                }
                                                button {
                                                    class: if is_tool { "is-on" } else { "" },
                                                    onclick: move |_| edit_kind.set("tool".to_string()),
                                                    "Tool"
                                                }
                                            }
                                            textarea {
                                                value: "{edit_prompt}",
                                                rows: 3,
                                                placeholder: if is_tool { "What to tell the bot after the tool runs (optional)" } else { "What it should do each time" },
                                                "aria-label": "Routine prompt",
                                                oninput: move |e| edit_prompt.set(e.value()),
                                            }
                                            if is_tool {
                                                input {
                                                    value: "{edit_tool}",
                                                    placeholder: "Tool name",
                                                    "aria-label": "Tool name",
                                                    oninput: move |e| edit_tool.set(e.value()),
                                                }
                                                textarea {
                                                    value: "{edit_tool_args}",
                                                    rows: 2,
                                                    placeholder: r#"{{}} or {{"key": "value"}}"#,
                                                    "aria-label": "Tool arguments (JSON)",
                                                    oninput: move |e| edit_tool_args.set(e.value()),
                                                }
                                                p { class: "field", small { "Tool arguments must be a JSON object. Blank means " code { "{{}}" } "." } }
                                            }
                                            input {
                                                value: "{edit_schedule}",
                                                "aria-label": "Schedule",
                                                oninput: move |e| edit_schedule.set(e.value()),
                                            }
                                            {schedule_hint(edit_preview)}

                                            {
                                                let is_slack_hook = *edit_hook_kind.read() == "slack";
                                                let match_placeholder = if is_slack_hook {
                                                    "keyword, regex (blank = every message)"
                                                } else {
                                                    "Match pattern, regex (blank = every delivery)"
                                                };
                                                rsx! {
                                                    div { class: "routine-hook",
                                                        p { class: "field", small { "Webhook signature" } }
                                                        select {
                                                            "aria-label": "Webhook signature kind",
                                                            value: "{edit_hook_kind}",
                                                            onchange: move |e| {
                                                                let kind = e.value();
                                                                // S5c-04: landing on "slack" with events left over from
                                                                // another kind (free-typed GitHub event names, say)
                                                                // would otherwise save as an invalid trigger kind - a
                                                                // fresh pick always starts the trigger select on the
                                                                // server's own default ("message", same as `hook_events[0]`
                                                                // absent on `routes/slack.rs`'s read side).
                                                                if kind == "slack"
                                                                    && !SLACK_TRIGGER_KINDS
                                                                        .iter()
                                                                        .any(|(k, _)| *k == edit_hook_events.read().as_str())
                                                                {
                                                                    edit_hook_events.set("message".to_string());
                                                                }
                                                                edit_hook_kind.set(kind);
                                                            },
                                                            for (value , label) in HOOK_KINDS.iter() {
                                                                option { key: "{value}", value: "{value}", "{label}" }
                                                            }
                                                        }
                                                        if is_slack_hook {
                                                            select {
                                                                "aria-label": "Slack trigger kind",
                                                                value: "{edit_hook_events}",
                                                                onchange: move |e| edit_hook_events.set(e.value()),
                                                                for (value , label) in SLACK_TRIGGER_KINDS.iter() {
                                                                    option { key: "{value}", value: "{value}", "{label}" }
                                                                }
                                                            }
                                                        } else {
                                                            input {
                                                                value: "{edit_hook_events}",
                                                                placeholder: "push, pull_request (GitHub events, blank = all)",
                                                                "aria-label": "Webhook events",
                                                                oninput: move |e| edit_hook_events.set(e.value()),
                                                            }
                                                        }
                                                        input {
                                                            value: "{edit_hook_match}",
                                                            placeholder: "{match_placeholder}",
                                                            "aria-label": "Webhook match pattern",
                                                            oninput: move |e| edit_hook_match.set(e.value()),
                                                        }
                                                    }
                                                }
                                            }

                                            if let Some(err) = edit_error.read().clone() {
                                                div { class: "refusal", role: "alert", p { "{err}" } }
                                            }
                                            div { class: "routine-acts",
                                                button {
                                                    class: "primary",
                                                    onclick: {
                                                        let id = routine.id.clone();
                                                        let bot_id = bot_id.clone();
                                                        move |_| {
                                                            let id = id.clone();
                                                            let kind = edit_kind.read().clone();
                                                            let events: Vec<String> = edit_hook_events
                                                                .read()
                                                                .split(',')
                                                                .map(|s| s.trim().to_string())
                                                                .filter(|s| !s.is_empty())
                                                                .collect();
                                                            let hook_match = edit_hook_match.read().trim().to_string();
                                                            let draft = RoutineDraft {
                                                                bot_id: bot_id.clone(),
                                                                name: edit_name.read().trim().to_string(),
                                                                prompt: edit_prompt.read().trim().to_string(),
                                                                schedule: edit_schedule.read().trim().to_string(),
                                                                kind,
                                                                tool: edit_tool.read().trim().to_string(),
                                                                tool_args: edit_tool_args.read().clone(),
                                                                hook_kind: edit_hook_kind.read().clone(),
                                                                hook_events: if events.is_empty() { None } else { Some(events) },
                                                                hook_match: if hook_match.is_empty() { None } else { Some(hook_match) },
                                                            };
                                                            spawn(save_edit(id, draft, routines, editing_id, edit_error));
                                                        }
                                                    },
                                                    "Save"
                                                }
                                                button {
                                                    onclick: move |_| editing_id.set(None),
                                                    "Cancel"
                                                }
                                            }
                                        }
                                    }
                                }
                            }

                            div { class: "routine-acts",
                                button {
                                    onclick: {
                                        let id = routine.id.clone();
                                        let bot_id = bot_id.clone();
                                        let active = routine.active;
                                        move |_| { spawn(toggle_active(id.clone(), active, bot_id.clone(), routines, run_status)); }
                                    },
                                    if routine.active { "Pause" } else { "Start" }
                                }
                                button {
                                    onclick: {
                                        let id = routine.id.clone();
                                        move |_| {
                                            let id = id.clone();
                                            if is_runs_open {
                                                runs_open.set(None);
                                            } else {
                                                spawn(load_runs(id, runs_open, runs, run_status));
                                            }
                                        }
                                    },
                                    if is_runs_open { "Hide runs" } else { "Recent runs" }
                                }
                                button {
                                    onclick: {
                                        let id = routine.id.clone();
                                        move |_| { spawn(run_now(id.clone(), run_status)); }
                                    },
                                    "Run now"
                                }
                                button {
                                    onclick: {
                                        let routine = routine.clone();
                                        move |_| {
                                            edit_name.set(routine.name.clone());
                                            edit_prompt.set(routine.prompt.clone());
                                            edit_schedule.set(routine.schedule_text.clone());
                                            edit_kind.set(routine.kind.clone());
                                            edit_tool.set(routine.tool.clone().unwrap_or_default());
                                            edit_tool_args.set(routine.tool_args.clone().unwrap_or_default());
                                            edit_hook_kind.set(routine.hook_kind.clone());
                                            edit_hook_events.set(routine.hook_events.clone().unwrap_or_default().join(", "));
                                            edit_hook_match.set(routine.hook_match.clone().unwrap_or_default());
                                            edit_error.set(None);
                                            editing_id.set(Some(routine.id.clone()));
                                        }
                                    },
                                    "Edit"
                                }
                                if routine.has_hook {
                                    button {
                                        onclick: {
                                            let id = routine.id.clone();
                                            let bot_id = bot_id.clone();
                                            move |_| {
                                                minted_hook.set(None);
                                                spawn(clear_hook_action(id.clone(), bot_id.clone(), routines, hook_status));
                                            }
                                        },
                                        "Clear webhook"
                                    }
                                } else {
                                    button {
                                        onclick: {
                                            let id = routine.id.clone();
                                            let bot_id = bot_id.clone();
                                            move |_| { spawn(mint_hook_action(id.clone(), bot_id.clone(), routines, hook_status, minted_hook)); }
                                        },
                                        "Create webhook"
                                    }
                                }
                                button {
                                    class: "danger",
                                    onclick: {
                                        let id = routine.id.clone();
                                        let bot_id = bot_id.clone();
                                        move |_| { spawn(delete_routine_action(id.clone(), bot_id.clone(), routines, run_status)); }
                                    },
                                    "Delete"
                                }
                            }

                            if let Some((rid, status)) = run_status.read().clone()
                                && rid == routine.id
                            {
                                p { class: "muted", "{status}" }
                            }

                            if let Some((rid, status)) = hook_status.read().clone()
                                && rid == routine.id
                            {
                                p { class: "muted", "{status}" }
                            }

                            // The webhook secret, shown exactly once - see
                            // this module's own doc and `api::mint_routine_hook`'s.
                            // No "show again" path exists anywhere in this
                            // client: closing the modal or minting a
                            // different routine's hook drops this for good.
                            if let Some((rid, secret, url)) = minted_hook.read().clone()
                                && rid == routine.id
                            {
                                div { class: "hook-secret", role: "alert",
                                    p { b { "Webhook created. " } "This secret will not be shown again." }
                                    div { class: "hook-secret-row",
                                        code { "{secret}" }
                                        button {
                                            onclick: {
                                                let secret = secret.clone();
                                                move |_| copy_to_clipboard(&secret)
                                            },
                                            "Copy secret"
                                        }
                                    }
                                    div { class: "hook-secret-row",
                                        code { "{url}" }
                                        button {
                                            onclick: {
                                                let url = url.clone();
                                                move |_| copy_to_clipboard(&url)
                                            },
                                            "Copy URL"
                                        }
                                    }
                                    button {
                                        onclick: move |_| minted_hook.set(None),
                                        "Done - I saved it"
                                    }
                                }
                            }

                            if is_runs_open {
                                div { class: "routine-runs",
                                    if runs.read().is_empty() {
                                        p { class: "muted", "It has not run yet." }
                                    }
                                    for run in runs.read().iter().cloned() {
                                        div { key: "{run.id}", class: "routine-run",
                                            div { class: "routine-run-top",
                                                span { class: "routine-run-status", "{run.status}" }
                                                span { "{run.created_at}" }
                                                span { class: "mono", "${run.cost_usd:.4}" }
                                            }
                                            p {
                                                if let Some(err) = run.error.filter(|e| !e.is_empty()) {
                                                    "{err}"
                                                } else {
                                                    "{run.text}"
                                                }
                                            }
                                        }
                                    }
                                }
                            }
                        }
                    }
                }
            }

            {
                let new_is_tool = *new_kind.read() == "tool";
                rsx! {
                    div { class: "routine-new",
                        input {
                            value: "{new_name}",
                            placeholder: "What to call it",
                            "aria-label": "Routine name",
                            oninput: move |e| new_name.set(e.value()),
                        }
                        div { class: "routine-kind-toggle", role: "group", "aria-label": "Kind",
                            button {
                                class: if !new_is_tool { "is-on" } else { "" },
                                onclick: move |_| new_kind.set("prompt".to_string()),
                                "Prompt"
                            }
                            button {
                                class: if new_is_tool { "is-on" } else { "" },
                                onclick: move |_| new_kind.set("tool".to_string()),
                                "Tool"
                            }
                        }
                        textarea {
                            value: "{new_prompt}",
                            rows: 3,
                            placeholder: if new_is_tool { "What to tell the bot after the tool runs (optional)" } else { "What it should do each time" },
                            "aria-label": "Routine prompt",
                            oninput: move |e| new_prompt.set(e.value()),
                        }
                        if new_is_tool {
                            input {
                                value: "{new_tool}",
                                placeholder: "Tool name",
                                "aria-label": "Tool name",
                                oninput: move |e| new_tool.set(e.value()),
                            }
                            textarea {
                                value: "{new_tool_args}",
                                rows: 2,
                                placeholder: r#"{{}} or {{"key": "value"}}"#,
                                "aria-label": "Tool arguments (JSON)",
                                oninput: move |e| new_tool_args.set(e.value()),
                            }
                            p { class: "field", small { "Tool arguments must be a JSON object. Blank means " code { "{{}}" } "." } }
                        }
                        input {
                            value: "{new_schedule}",
                            placeholder: "daily at 07:30",
                            "aria-label": "Schedule",
                            oninput: move |e| new_schedule.set(e.value()),
                        }
                        p { class: "field",
                            small {
                                "Say it plainly: "
                                code { "every 15 minutes" }
                                ", "
                                code { "hourly" }
                                ", "
                                code { "daily at 07:30" }
                                ", "
                                code { "weekdays at 08:43" }
                                ". It starts paused."
                            }
                        }
                        {schedule_hint(new_preview.read().clone())}
                        if let Some(err) = create_error.read().clone() {
                            div { class: "refusal", role: "alert", p { "{err}" } }
                        }
                        button {
                            class: "stg-btn primary",
                            disabled: create_disabled,
                            onclick: {
                                let bot_id = bot_id.clone();
                                move |_| {
                                    let draft = RoutineDraft {
                                        bot_id: bot_id.clone(),
                                        name: new_name.read().trim().to_string(),
                                        prompt: new_prompt.read().trim().to_string(),
                                        schedule: new_schedule.read().trim().to_string(),
                                        kind: new_kind.read().clone(),
                                        tool: new_tool.read().trim().to_string(),
                                        tool_args: new_tool_args.read().clone(),
                                        hook_kind: "raw".to_string(),
                                        hook_events: None,
                                        hook_match: None,
                                    };
                                    let form = NewRoutineForm {
                                        name: new_name,
                                        prompt: new_prompt,
                                        schedule: new_schedule,
                                        kind: new_kind,
                                        tool: new_tool,
                                        tool_args: new_tool_args,
                                        error: create_error,
                                    };
                                    spawn(create_routine_action(draft, routines, form));
                                }
                            },
                            "Add routine"
                        }
                    }
                }
            }
        }
    }
}

/// Best-effort copy to the OS clipboard via `navigator.clipboard.writeText`.
/// Fire-and-forget: the returned `Promise` is dropped rather than awaited -
/// there is nothing more useful to do with a copy failure here than what
/// happens today with no copy button at all (the secret is still on screen,
/// selectable by hand), and awaiting it would need a second `spawn` for a
/// plain button click that has no other async work to do.
fn copy_to_clipboard(text: &str) {
    if let Some(window) = web_sys::window() {
        let _ = window.navigator().clipboard().write_text(text);
    }
}

/// Renders `preview_schedule`'s verdict under a schedule field: a muted
/// "→ description" line on `Ok`, a `.refusal` box on a non-empty `Err`, and
/// nothing when the local mirror has no opinion yet (`None`, or an `Err("")`
/// - see that function's doc on why it returns those two differently).
fn schedule_hint(preview: Option<Result<String, String>>) -> Element {
    match preview {
        Some(Ok(desc)) => rsx! {
            p { class: "routine-preview", "→ {desc}" }
        },
        Some(Err(msg)) if !msg.is_empty() => rsx! {
            div { class: "refusal", role: "alert", p { "{msg}" } }
        },
        _ => rsx! {},
    }
}

/// The typed-out fields behind a create or an edit save - bundled so the
/// two async actions below stay under clippy's `too_many_arguments` without
/// losing any of name/prompt/schedule/kind/tool/tool_args/hook-*
/// (`bot_id` too, for `reload` after). S5b-07 widened this past the S5-05
/// trio - `tool`/`tool_args` are sent only when `kind == "tool"` (see
/// `create_routine_action`/`save_edit` below, mirroring the server's own
/// `kind`-gated validation), and `hook_kind`/`hook_events`/`hook_match` ride
/// along on both create and edit (a routine can be born with a hook
/// pre-configured, same as TS's own `CreateRoutineBody` allows) - only the
/// SECRET itself needs the separate mint/clear round trip, not the kind
/// config around it.
struct RoutineDraft {
    bot_id: String,
    name: String,
    prompt: String,
    schedule: String,
    kind: String,
    tool: String,
    tool_args: String,
    hook_kind: String,
    hook_events: Option<Vec<String>>,
    hook_match: Option<String>,
}

/// The create form's own signals, reset together on a successful save -
/// bundled for the same reason `RoutineDraft` is. Hook fields are not reset
/// here - the create form has no hook section (see this module's own doc:
/// minting needs an id, so hook config on a brand-new routine happens after
/// creation, in the edit form).
struct NewRoutineForm {
    name: Signal<String>,
    prompt: Signal<String>,
    schedule: Signal<String>,
    kind: Signal<String>,
    tool: Signal<String>,
    tool_args: Signal<String>,
    error: Signal<Option<String>>,
}

async fn create_routine_action(
    draft: RoutineDraft,
    routines: Signal<Option<Vec<Routine>>>,
    mut form: NewRoutineForm,
) {
    let is_tool = draft.kind == "tool";
    let req = api::CreateRoutineReq {
        bot_id: &draft.bot_id,
        name: &draft.name,
        prompt: &draft.prompt,
        schedule: &draft.schedule,
        kind: Some(draft.kind.as_str()),
        tool: is_tool.then_some(draft.tool.as_str()),
        tool_args: is_tool.then_some(draft.tool_args.as_str()),
    };
    match api::create_routine(&req).await {
        Ok(_) => {
            form.name.set(String::new());
            form.prompt.set(String::new());
            form.schedule.set(String::new());
            form.kind.set("prompt".to_string());
            form.tool.set(String::new());
            form.tool_args.set(String::new());
            form.error.set(None);
            reload(draft.bot_id, routines).await;
        }
        Err(e) => form.error.set(Some(e)),
    }
}

async fn save_edit(
    id: String,
    draft: RoutineDraft,
    routines: Signal<Option<Vec<Routine>>>,
    mut editing_id: Signal<Option<String>>,
    mut edit_error: Signal<Option<String>>,
) {
    let is_tool = draft.kind == "tool";
    let req = api::UpdateRoutineReq {
        name: &draft.name,
        prompt: &draft.prompt,
        schedule: &draft.schedule,
        kind: Some(draft.kind.as_str()),
        tool: is_tool.then_some(draft.tool.as_str()),
        tool_args: is_tool.then_some(draft.tool_args.as_str()),
        hook_kind: Some(draft.hook_kind.as_str()),
        hook_events: Some(draft.hook_events.clone()),
        hook_match: Some(draft.hook_match.as_deref()),
    };
    match api::update_routine(&id, &req).await {
        Ok(_) => {
            editing_id.set(None);
            edit_error.set(None);
            reload(draft.bot_id, routines).await;
        }
        Err(e) => edit_error.set(Some(e)),
    }
}

/// `POST /api/routines/:id/hook` - mints a fresh secret and shows it via
/// `minted_hook` (see this module's own doc on why that is the only place a
/// secret is ever written into this client's state). Reloads the list
/// afterward so the row's badge flips to `hasHook: true` without a second
/// manual refresh - the reloaded `Routine` never carries the secret itself
/// (`api::mint_routine_hook`'s doc), so this is safe to do unconditionally.
async fn mint_hook_action(
    id: String,
    bot_id: String,
    routines: Signal<Option<Vec<Routine>>>,
    mut hook_status: Signal<Option<(String, String)>>,
    mut minted_hook: Signal<Option<(String, String, String)>>,
) {
    match api::mint_routine_hook(&id).await {
        Ok((secret, url)) => {
            minted_hook.set(Some((id.clone(), secret, url)));
            hook_status.set(None);
            reload(bot_id, routines).await;
        }
        Err(e) => hook_status.set(Some((id, e))),
    }
}

/// `DELETE /api/routines/:id/hook` - clears the secret; the routine stops
/// accepting deliveries until a fresh one is minted.
async fn clear_hook_action(
    id: String,
    bot_id: String,
    routines: Signal<Option<Vec<Routine>>>,
    mut hook_status: Signal<Option<(String, String)>>,
) {
    match api::clear_routine_hook(&id).await {
        Ok(()) => {
            hook_status.set(None);
            reload(bot_id, routines).await;
        }
        Err(e) => hook_status.set(Some((id, e))),
    }
}

async fn toggle_active(
    id: String,
    active: bool,
    bot_id: String,
    routines: Signal<Option<Vec<Routine>>>,
    mut run_status: Signal<Option<(String, String)>>,
) {
    match api::set_routine_active(&id, !active).await {
        Ok(_) => {
            reload(bot_id, routines).await;
            run_status.set(None);
        }
        Err(e) => run_status.set(Some((id, e))),
    }
}

async fn delete_routine_action(
    id: String,
    bot_id: String,
    routines: Signal<Option<Vec<Routine>>>,
    mut run_status: Signal<Option<(String, String)>>,
) {
    match api::delete_routine(&id).await {
        Ok(_) => {
            reload(bot_id, routines).await;
            run_status.set(None);
        }
        Err(e) => run_status.set(Some((id, e))),
    }
}

async fn load_runs(
    id: String,
    mut runs_open: Signal<Option<String>>,
    mut runs: Signal<Vec<RoutineRun>>,
    mut run_status: Signal<Option<(String, String)>>,
) {
    match api::fetch_routine_runs(&id).await {
        Ok(list) => {
            runs.set(list);
            runs_open.set(Some(id.clone()));
            run_status.set(None);
        }
        Err(e) => {
            run_status.set(Some((id, e)));
        }
    }
}

async fn run_now(id: String, mut run_status: Signal<Option<(String, String)>>) {
    match api::run_routine_now(&id).await {
        Ok(run_id) => {
            let short = &run_id[..run_id.len().min(8)];
            run_status.set(Some((id, format!("Started run {short}"))));
        }
        Err(e) => run_status.set(Some((id, e))),
    }
}

/// "in 5m" / "3h ago" / "never" - port of `RoutinesEditor.tsx`'s own
/// `formatWhen`. Built on `js_sys::Date` rather than `chrono` (the client
/// crate has no `chrono` dependency - see its `Cargo.toml`), same choice
/// `message_time.rs` already made for message timestamps; like that
/// module, this only meaningfully runs in a browser.
fn format_when(iso: Option<&str>) -> String {
    let Some(iso) = iso else {
        return "never".to_string();
    };
    let then = Date::new(&JsValue::from_str(iso));
    if then.get_time().is_nan() {
        return "never".to_string();
    }
    let diff_ms = then.get_time() - Date::now();
    let mins = (diff_ms.abs() / 60_000.0).round() as i64;
    let phrase = if mins < 1 {
        "now".to_string()
    } else if mins < 60 {
        format!("{mins}m")
    } else if mins < 1440 {
        format!("{}h", (mins as f64 / 60.0).round() as i64)
    } else {
        format!("{}d", (mins as f64 / 1440.0).round() as i64)
    };
    if diff_ms >= 0.0 {
        format!("in {phrase}")
    } else {
        format!("{phrase} ago")
    }
}
