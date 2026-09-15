//! S5b-07: the Goals modal, in the style of `routines_editor.rs`'s Routines
//! modal (that module's own doc explains the shared `.modal*` shell this
//! reuses again). A "Goals" button in `thread.rs`'s `pane-head`, beside
//! "Routines", opens `GoalsModal` over the thread.
//!
//! A goal is not a routine wearing a different name - `store::goals`'s own
//! doc says why (an end state a routine has no room for, a token/deadline
//! budget instead of a wall-clock one, a plan+log that carries across
//! sessions) - so this editor is its own component rather than a variant of
//! `RoutinesEditor`, even though the CRUD/runs-panel shape rhymes.
//!
//! List: objective ("name"), status, budget, the last two log lines.
//! Create/edit: objective, done_when, budget (tokens and/or a deadline).
//! Status only changes through the edit form's own select + note field -
//! `PATCH /api/goals/:id` refuses to close a goal `"done"` without a note
//! (`store::goals::update_goal`'s own check: "Say what evidence shows
//! done_when is satisfied..."), so there is no one-click "Mark done" button
//! that could round-trip a 400 with nothing on screen to fill in. Pause/
//! Resume stay one-click (no note required by the server for those) - same
//! posture `routines_editor.rs`'s Pause/Start button already takes.
//!
//! `GoalRun` is not a separate type - `api::fetch_goal_runs` answers
//! `RoutineRun` (see that type's own doc in `types.rs`: the two routes'
//! wire shapes are byte-identical), so the runs panel below is nearly a
//! copy of `routines_editor.rs`'s own - kept side by side rather than
//! factored into one shared component, since the surrounding row markup
//! (`.routine-run*` vs a goal's own status/budget furniture) differs enough
//! that a shared component would need as many parameters as it saved lines.

use crate::api;
use crate::types::{Goal, RoutineRun};
use dioxus::prelude::*;

const STATUSES: &[(&str, &str)] = &[
    ("active", "Active"),
    ("paused", "Paused"),
    ("done", "Done"),
    ("stopped", "Stopped"),
];

/// The modal shell - see `routines_editor.rs`'s `RoutinesModal` doc for why
/// this reuses `.modal-scrim`/`.modal`/`.modal-head`/`.modal-x` as-is.
#[component]
pub fn GoalsModal(bot_id: String, bot_name: String, on_close: EventHandler<()>) -> Element {
    rsx! {
        div {
            class: "modal-scrim",
            role: "presentation",
            onclick: move |_| on_close.call(()),
            div {
                class: "modal goals-modal",
                onclick: move |evt| evt.stop_propagation(),
                role: "dialog",
                "aria-modal": "true",
                "aria-label": "{bot_name} goals",
                div { class: "modal-head",
                    h2 { "{bot_name} · Goals" }
                    button {
                        class: "modal-x",
                        "aria-label": "Close",
                        onclick: move |_| on_close.call(()),
                        "×"
                    }
                }
                div { class: "modal-body",
                    GoalsEditor { bot_id: bot_id.clone() }
                }
            }
        }
    }
}

async fn reload(bot_id: String, mut goals: Signal<Option<Vec<Goal>>>) {
    if let Ok(list) = api::fetch_goals(&bot_id).await {
        goals.set(Some(list));
    }
}

#[component]
pub fn GoalsEditor(bot_id: String) -> Element {
    let goals = use_signal(|| None::<Vec<Goal>>);

    let mut new_objective = use_signal(String::new);
    let mut new_done_when = use_signal(String::new);
    let mut new_budget_tokens = use_signal(String::new);
    let mut new_budget_until = use_signal(String::new);
    let mut create_error = use_signal(|| None::<String>);

    let mut editing_id = use_signal(|| None::<String>);
    // The status the goal actually had when the edit form opened - `Save`
    // only sends a `status` key in the PATCH when this no longer matches
    // `edit_status`, so an edit that never touches the status dropdown
    // never trips the "done needs a note" check or writes a redundant
    // "status -> same value" log line (`store::goals::update_goal`'s own
    // `if patch.status.is_some() || patch.note.is_some()` always logs
    // SOMETHING once either key is present at all).
    let mut edit_original_status = use_signal(String::new);
    let mut edit_objective = use_signal(String::new);
    let mut edit_done_when = use_signal(String::new);
    let mut edit_budget_tokens = use_signal(String::new);
    let mut edit_budget_until = use_signal(String::new);
    let mut edit_status = use_signal(|| "active".to_string());
    let mut edit_note = use_signal(String::new);
    let mut edit_error = use_signal(|| None::<String>);

    let mut runs_open = use_signal(|| None::<String>);
    let runs = use_signal(Vec::<RoutineRun>::new);
    // (goal id, message) - same per-row gating `routines_editor.rs` uses for
    // its own `run_status`, so one goal's "Run now"/error never bleeds onto
    // every other row in the list.
    let status_msg = use_signal(|| None::<(String, String)>);

    let load_bot_id = bot_id.clone();
    use_effect(move || {
        let bot_id = load_bot_id.clone();
        spawn(reload(bot_id, goals));
    });

    let Some(list) = goals.read().clone() else {
        return rsx! { p { class: "muted", "Loading goals…" } };
    };

    let create_disabled =
        new_objective.read().trim().is_empty() || new_done_when.read().trim().is_empty();

    rsx! {
        div { class: "goals",
            if list.is_empty() {
                p { class: "muted goals-empty",
                    "No goals yet. A goal works toward something across many sessions, on its own, until it is done, stuck, or out of budget."
                }
            }

            for goal in list.iter().cloned() {
                {
                    let is_editing = editing_id.read().as_deref() == Some(goal.id.as_str());
                    let is_runs_open = runs_open.read().as_deref() == Some(goal.id.as_str());
                    let recent_log: Vec<_> = goal.log.iter().rev().take(2).cloned().collect();
                    rsx! {
                        article {
                            key: "{goal.id}",
                            class: if goal.status == "active" { "goal is-on" } else { "goal" },
                            div { class: "goal-top",
                                span {
                                    class: if goal.status == "active" { "goal-dot is-on" } else { "goal-dot" },
                                    "aria-hidden": "true",
                                }
                                b { "{goal.objective}" }
                                span { class: "goal-status goal-status-{goal.status}", "{goal.status}" }
                            }
                            p { class: "goal-done-when",
                                b { "Done when: " }
                                "{goal.done_when}"
                            }
                            div { class: "goal-meta",
                                span { "{format_budget(&goal)}" }
                                if let Some(until) = goal.budget_until.clone() {
                                    span { "by {until}" }
                                }
                            }
                            if let Some(reason) = goal.reason.clone().filter(|r| !r.is_empty())
                                && goal.status != "active"
                            {
                                p { class: "goal-stopped", "{reason}" }
                            }
                            if !recent_log.is_empty() {
                                div { class: "goal-log",
                                    for entry in recent_log.iter() {
                                        p { key: "{entry.at}", class: "goal-log-line",
                                            span { class: "goal-log-kind", "{entry.kind}" }
                                            " {entry.text}"
                                        }
                                    }
                                }
                            }

                            if is_editing {
                                {
                                    let status_changing = *edit_status.read() != *edit_original_status.read();
                                    rsx! {
                                        div { class: "goal-edit",
                                            textarea {
                                                value: "{edit_objective}",
                                                rows: 2,
                                                "aria-label": "Objective",
                                                oninput: move |e| edit_objective.set(e.value()),
                                            }
                                            input {
                                                value: "{edit_done_when}",
                                                placeholder: "What done looks like",
                                                "aria-label": "Done when",
                                                oninput: move |e| edit_done_when.set(e.value()),
                                            }
                                            div { class: "goal-budget-row",
                                                input {
                                                    value: "{edit_budget_tokens}",
                                                    placeholder: "Token budget (blank = none)",
                                                    "aria-label": "Token budget",
                                                    oninput: move |e| edit_budget_tokens.set(e.value()),
                                                }
                                                input {
                                                    value: "{edit_budget_until}",
                                                    placeholder: "Deadline, ISO (blank = none)",
                                                    "aria-label": "Deadline",
                                                    oninput: move |e| edit_budget_until.set(e.value()),
                                                }
                                            }
                                            select {
                                                "aria-label": "Status",
                                                value: "{edit_status}",
                                                onchange: move |e| edit_status.set(e.value()),
                                                for (value , label) in STATUSES.iter() {
                                                    option { key: "{value}", value: "{value}", "{label}" }
                                                }
                                            }
                                            textarea {
                                                value: "{edit_note}",
                                                rows: 2,
                                                placeholder: if status_changing && *edit_status.read() == "done" { "Required: what evidence shows done_when is satisfied" } else { "Note (optional)" },
                                                "aria-label": "Note",
                                                oninput: move |e| edit_note.set(e.value()),
                                            }
                                            if let Some(err) = edit_error.read().clone() {
                                                div { class: "refusal", role: "alert", p { "{err}" } }
                                            }
                                            div { class: "routine-acts",
                                                button {
                                                    class: "primary",
                                                    onclick: {
                                                        let id = goal.id.clone();
                                                        let bot_id = bot_id.clone();
                                                        move |_| {
                                                            let budget_tokens = match edit_budget_tokens.read().trim().parse::<f64>() {
                                                                Ok(v) => BudgetTokensPatch::Set(v),
                                                                Err(_) if edit_budget_tokens.read().trim().is_empty() => BudgetTokensPatch::Clear,
                                                                Err(_) => BudgetTokensPatch::Invalid,
                                                            };
                                                            if matches!(budget_tokens, BudgetTokensPatch::Invalid) {
                                                                edit_error.set(Some("Token budget must be a number.".to_string()));
                                                                return;
                                                            }
                                                            let draft = GoalEditDraft {
                                                                bot_id: bot_id.clone(),
                                                                objective: edit_objective.read().trim().to_string(),
                                                                done_when: edit_done_when.read().trim().to_string(),
                                                                budget_tokens,
                                                                budget_until: edit_budget_until.read().trim().to_string(),
                                                                status: if status_changing { Some(edit_status.read().clone()) } else { None },
                                                                note: edit_note.read().trim().to_string(),
                                                            };
                                                            spawn(save_goal_edit(id.clone(), draft, goals, editing_id, edit_error));
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
                                if goal.status == "active" {
                                    button {
                                        onclick: {
                                            let id = goal.id.clone();
                                            let bot_id = bot_id.clone();
                                            move |_| { spawn(set_goal_status(id.clone(), bot_id.clone(), "paused".to_string(), goals, status_msg)); }
                                        },
                                        "Pause"
                                    }
                                } else if goal.status == "paused" {
                                    button {
                                        onclick: {
                                            let id = goal.id.clone();
                                            let bot_id = bot_id.clone();
                                            move |_| { spawn(set_goal_status(id.clone(), bot_id.clone(), "active".to_string(), goals, status_msg)); }
                                        },
                                        "Resume"
                                    }
                                }
                                button {
                                    onclick: {
                                        let id = goal.id.clone();
                                        move |_| {
                                            let id = id.clone();
                                            if is_runs_open {
                                                runs_open.set(None);
                                            } else {
                                                spawn(load_runs(id, runs_open, runs, status_msg));
                                            }
                                        }
                                    },
                                    if is_runs_open { "Hide runs" } else { "Recent runs" }
                                }
                                button {
                                    onclick: {
                                        let id = goal.id.clone();
                                        move |_| { spawn(run_now(id.clone(), status_msg)); }
                                    },
                                    "Run now"
                                }
                                button {
                                    onclick: {
                                        let goal = goal.clone();
                                        move |_| {
                                            edit_objective.set(goal.objective.clone());
                                            edit_done_when.set(goal.done_when.clone());
                                            edit_budget_tokens.set(goal.budget_tokens.map(|v| v.to_string()).unwrap_or_default());
                                            edit_budget_until.set(goal.budget_until.clone().unwrap_or_default());
                                            edit_status.set(goal.status.clone());
                                            edit_original_status.set(goal.status.clone());
                                            edit_note.set(String::new());
                                            edit_error.set(None);
                                            editing_id.set(Some(goal.id.clone()));
                                        }
                                    },
                                    "Edit"
                                }
                                button {
                                    class: "danger",
                                    onclick: {
                                        let id = goal.id.clone();
                                        let bot_id = bot_id.clone();
                                        move |_| { spawn(delete_goal_action(id.clone(), bot_id.clone(), goals, status_msg)); }
                                    },
                                    "Delete"
                                }
                            }

                            if let Some((gid, msg)) = status_msg.read().clone()
                                && gid == goal.id
                            {
                                p { class: "muted", "{msg}" }
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

            div { class: "goal-new",
                textarea {
                    value: "{new_objective}",
                    rows: 2,
                    placeholder: "What the goal is",
                    "aria-label": "Objective",
                    oninput: move |e| new_objective.set(e.value()),
                }
                input {
                    value: "{new_done_when}",
                    placeholder: "What done looks like",
                    "aria-label": "Done when",
                    oninput: move |e| new_done_when.set(e.value()),
                }
                div { class: "goal-budget-row",
                    input {
                        value: "{new_budget_tokens}",
                        placeholder: "Token budget (blank = none)",
                        "aria-label": "Token budget",
                        oninput: move |e| new_budget_tokens.set(e.value()),
                    }
                    input {
                        value: "{new_budget_until}",
                        placeholder: "Deadline, ISO (blank = none)",
                        "aria-label": "Deadline",
                        oninput: move |e| new_budget_until.set(e.value()),
                    }
                }
                p { class: "field",
                    small { "Up to 20 active goals per bot. It starts active - use Pause below if it should wait." }
                }
                if let Some(err) = create_error.read().clone() {
                    div { class: "refusal", role: "alert", p { "{err}" } }
                }
                button {
                    class: "stg-btn primary",
                    disabled: create_disabled,
                    onclick: {
                        let bot_id = bot_id.clone();
                        move |_| {
                            let budget_tokens = new_budget_tokens.read().trim().parse::<f64>().ok();
                            if !new_budget_tokens.read().trim().is_empty() && budget_tokens.is_none() {
                                create_error.set(Some("Token budget must be a number.".to_string()));
                                return;
                            }
                            let budget_until = new_budget_until.read().trim().to_string();
                            let draft = NewGoalDraft {
                                bot_id: bot_id.clone(),
                                objective: new_objective.read().trim().to_string(),
                                done_when: new_done_when.read().trim().to_string(),
                                budget_tokens,
                                budget_until: if budget_until.is_empty() { None } else { Some(budget_until) },
                            };
                            let form = NewGoalForm {
                                objective: new_objective,
                                done_when: new_done_when,
                                budget_tokens: new_budget_tokens,
                                budget_until: new_budget_until,
                                error: create_error,
                            };
                            spawn(create_goal_action(draft, goals, form));
                        }
                    },
                    "Add goal"
                }
            }
        }
    }
}

fn format_budget(goal: &Goal) -> String {
    match goal.budget_tokens {
        Some(cap) => format!("{}/{} tokens", goal.spent_tokens, cap),
        None => format!("{} tokens spent, no cap", goal.spent_tokens),
    }
}

struct NewGoalDraft {
    bot_id: String,
    objective: String,
    done_when: String,
    budget_tokens: Option<f64>,
    budget_until: Option<String>,
}

struct NewGoalForm {
    objective: Signal<String>,
    done_when: Signal<String>,
    budget_tokens: Signal<String>,
    budget_until: Signal<String>,
    error: Signal<Option<String>>,
}

async fn create_goal_action(
    draft: NewGoalDraft,
    goals: Signal<Option<Vec<Goal>>>,
    mut form: NewGoalForm,
) {
    match api::create_goal(
        &draft.bot_id,
        &draft.objective,
        &draft.done_when,
        draft.budget_tokens,
        draft.budget_until.as_deref(),
    )
    .await
    {
        Ok(_) => {
            form.objective.set(String::new());
            form.done_when.set(String::new());
            form.budget_tokens.set(String::new());
            form.budget_until.set(String::new());
            form.error.set(None);
            reload(draft.bot_id, goals).await;
        }
        Err(e) => form.error.set(Some(e)),
    }
}

/// Whether the edit form's token-budget text parsed to a number, was left
/// blank (clear the budget), or is garbage the server would reject anyway -
/// checked client-side so a typo shows up before the round trip rather than
/// after.
enum BudgetTokensPatch {
    Set(f64),
    Clear,
    Invalid,
}

struct GoalEditDraft {
    bot_id: String,
    objective: String,
    done_when: String,
    budget_tokens: BudgetTokensPatch,
    budget_until: String,
    /// `None` when the status dropdown was never touched - see
    /// `edit_original_status`'s own doc above for why that matters.
    status: Option<String>,
    note: String,
}

async fn save_goal_edit(
    id: String,
    draft: GoalEditDraft,
    goals: Signal<Option<Vec<Goal>>>,
    mut editing_id: Signal<Option<String>>,
    mut edit_error: Signal<Option<String>>,
) {
    let mut body = serde_json::json!({
        "objective": draft.objective,
        "doneWhen": draft.done_when,
        "budgetTokens": match draft.budget_tokens {
            BudgetTokensPatch::Set(v) => serde_json::json!(v),
            BudgetTokensPatch::Clear | BudgetTokensPatch::Invalid => serde_json::Value::Null,
        },
        "budgetUntil": if draft.budget_until.is_empty() { serde_json::Value::Null } else { serde_json::json!(draft.budget_until) },
    });
    if let Some(map) = body.as_object_mut() {
        if let Some(status) = &draft.status {
            map.insert("status".to_string(), serde_json::json!(status));
        }
        if !draft.note.is_empty() {
            map.insert("note".to_string(), serde_json::json!(draft.note));
        }
    }
    match api::patch_goal(&id, body).await {
        Ok(_) => {
            editing_id.set(None);
            edit_error.set(None);
            reload(draft.bot_id, goals).await;
        }
        Err(e) => edit_error.set(Some(e)),
    }
}

/// The Pause/Resume one-click actions - a bare status PATCH, no note (the
/// server only requires one to close a goal `"done"`).
async fn set_goal_status(
    id: String,
    bot_id: String,
    status: String,
    goals: Signal<Option<Vec<Goal>>>,
    mut status_msg: Signal<Option<(String, String)>>,
) {
    match api::patch_goal(&id, serde_json::json!({ "status": status })).await {
        Ok(_) => {
            reload(bot_id, goals).await;
            status_msg.set(None);
        }
        Err(e) => status_msg.set(Some((id, e))),
    }
}

async fn delete_goal_action(
    id: String,
    bot_id: String,
    goals: Signal<Option<Vec<Goal>>>,
    mut status_msg: Signal<Option<(String, String)>>,
) {
    match api::delete_goal(&id).await {
        Ok(_) => {
            reload(bot_id, goals).await;
            status_msg.set(None);
        }
        Err(e) => status_msg.set(Some((id, e))),
    }
}

async fn load_runs(
    id: String,
    mut runs_open: Signal<Option<String>>,
    mut runs: Signal<Vec<RoutineRun>>,
    mut status_msg: Signal<Option<(String, String)>>,
) {
    match api::fetch_goal_runs(&id).await {
        Ok(list) => {
            runs.set(list);
            runs_open.set(Some(id.clone()));
            status_msg.set(None);
        }
        Err(e) => status_msg.set(Some((id, e))),
    }
}

async fn run_now(id: String, mut status_msg: Signal<Option<(String, String)>>) {
    match api::run_goal_now(&id).await {
        Ok(run_id) => {
            let short = &run_id[..run_id.len().min(8)];
            status_msg.set(Some((id, format!("Started run {short}"))));
        }
        Err(e) => status_msg.set(Some((id, e))),
    }
}
