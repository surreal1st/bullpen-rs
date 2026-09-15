//! Port of `RoutinesEditor.tsx`'s list + create/edit form (774 lines; hooks,
//! tool-kind routines and conditions are S5b - skipped entirely, same as
//! `Routine`'s own doc in `types.rs`). A "Routines" button in `thread.rs`'s
//! `pane-head` opens `RoutinesModal` over the thread, the same `.modal*`
//! shell `PermissionsModal`/`MemoryModal` already use.
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

use crate::api;
use crate::types::{Routine, RoutineRun};
use dioxus::prelude::*;
use gloo_timers::future::TimeoutFuture;
use js_sys::Date;
use std::cell::Cell;
use std::rc::Rc;
use wasm_bindgen::JsValue;

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
    let create_error = use_signal(|| None::<String>);

    let mut editing_id = use_signal(|| None::<String>);
    let mut edit_name = use_signal(String::new);
    let mut edit_prompt = use_signal(String::new);
    let mut edit_schedule = use_signal(String::new);
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

    let create_disabled = new_name.read().trim().is_empty()
        || new_prompt.read().trim().is_empty()
        || new_schedule.read().trim().is_empty();

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
                                    rsx! {
                                        div { class: "routine-edit",
                                            input {
                                                value: "{edit_name}",
                                                "aria-label": "Routine name",
                                                oninput: move |e| edit_name.set(e.value()),
                                            }
                                            textarea {
                                                value: "{edit_prompt}",
                                                rows: 3,
                                                "aria-label": "Routine prompt",
                                                oninput: move |e| edit_prompt.set(e.value()),
                                            }
                                            input {
                                                value: "{edit_schedule}",
                                                "aria-label": "Schedule",
                                                oninput: move |e| edit_schedule.set(e.value()),
                                            }
                                            {schedule_hint(edit_preview)}
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
                                                            let draft = RoutineDraft {
                                                                bot_id: bot_id.clone(),
                                                                name: edit_name.read().trim().to_string(),
                                                                prompt: edit_prompt.read().trim().to_string(),
                                                                schedule: edit_schedule.read().trim().to_string(),
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
                                            edit_error.set(None);
                                            editing_id.set(Some(routine.id.clone()));
                                        }
                                    },
                                    "Edit"
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

            div { class: "routine-new",
                input {
                    value: "{new_name}",
                    placeholder: "What to call it",
                    "aria-label": "Routine name",
                    oninput: move |e| new_name.set(e.value()),
                }
                textarea {
                    value: "{new_prompt}",
                    rows: 3,
                    placeholder: "What it should do each time",
                    "aria-label": "Routine prompt",
                    oninput: move |e| new_prompt.set(e.value()),
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
                            };
                            let form = NewRoutineForm {
                                name: new_name,
                                prompt: new_prompt,
                                schedule: new_schedule,
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
/// losing any of name/prompt/schedule (`bot_id` too, for `reload` after).
struct RoutineDraft {
    bot_id: String,
    name: String,
    prompt: String,
    schedule: String,
}

/// The create form's own signals, reset together on a successful save -
/// bundled for the same reason `RoutineDraft` is.
struct NewRoutineForm {
    name: Signal<String>,
    prompt: Signal<String>,
    schedule: Signal<String>,
    error: Signal<Option<String>>,
}

async fn create_routine_action(
    draft: RoutineDraft,
    routines: Signal<Option<Vec<Routine>>>,
    mut form: NewRoutineForm,
) {
    match api::create_routine(&draft.bot_id, &draft.name, &draft.prompt, &draft.schedule).await {
        Ok(_) => {
            form.name.set(String::new());
            form.prompt.set(String::new());
            form.schedule.set(String::new());
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
    match api::update_routine(&id, &draft.name, &draft.prompt, &draft.schedule).await {
        Ok(_) => {
            editing_id.set(None);
            edit_error.set(None);
            reload(draft.bot_id, routines).await;
        }
        Err(e) => edit_error.set(Some(e)),
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
