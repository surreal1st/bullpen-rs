//! S5b-F-04 (F4): `set_goal`, `update_goal`, `reflect` - the three tools a
//! goal session needs to ever actually finish. Port of `src/server/app.ts`'s
//! three specs (`:5575-5620`) and their dispatch (`:7047-7074`), over the
//! already-landed `store::goals` data layer (S5b-03).
//!
//! Before this file, `permissions.rs:154-156` already carried all three
//! names as `Decision::Allow` and the goal-session prompt already told the
//! model to call them (`crate::goals::GOAL_SESSION_INSTRUCTIONS`) - but
//! `tools::all_specs()` never offered them and no dispatch arm existed, so
//! every call came back `"Unknown tool: update_goal"` and a goal could
//! never reach `done` short of a hand-PATCH. See `reviews/S5b-R.md`'s F4.
//!
//! Each `run` here follows this crate's established strict-`Args` shape
//! (`note.rs`, `remember.rs`): a required field missing or the wrong JSON
//! type fails the WHOLE parse with a "Could not read ..." result, rather
//! than TS's per-field `typeof x === "string" ? x : ""` coercion. This is a
//! deviation from a literal port, but it is the convention every other tool
//! in this module already uses, and `store::goals::create_goal`/
//! `update_goal` still refuse an empty `objective`/`done_when`/`id` with
//! their own TS-mirrored error text either way - so the model-facing
//! behaviour (a clear refusal, never a silent no-op) is the same.

use std::sync::Arc;

use chrono::Utc;
use model::ToolSpec;
use serde::Deserialize;
use serde_json::json;
use store::Db;
use store::goals::{CreateGoalInput, UpdateGoalPatch, is_goal_status};

use super::lock_db;

pub fn set_goal_spec() -> ToolSpec {
    ToolSpec {
        name: "set_goal".to_string(),
        description: "Start a new goal: something you work toward across many sessions until \
it is done, stuck, or out of budget, rather than a one-off instruction. You get a work session \
roughly every hour under a quiet-hours window, forced to the cheap model. Use this only when \
Josh actually asked for ongoing work, not for a single task - use add_task for that."
            .to_string(),
        parameters: json!({
            "type": "object",
            "properties": {
                "objective": { "type": "string", "description": "What you are working toward, plainly." },
                "done_when": { "type": "string", "description": "What has to be true for this to be finished." },
                "budget_tokens": {
                    "type": "number",
                    "description": "Optional. Stop and report once this many tokens have been spent on it, across every session."
                },
                "budget_until": {
                    "type": "string",
                    "description": "Optional. An ISO date/time. Stop and report once this passes, whatever the token spend."
                }
            },
            "required": ["objective", "done_when"]
        }),
    }
}

pub fn update_goal_spec() -> ToolSpec {
    ToolSpec {
        name: "update_goal".to_string(),
        description: "Change one of your own goals: its plan, or its status. Set status to \
\"done\" the moment done_when is genuinely satisfied - note is REQUIRED then, and has to say \
what evidence shows it, the same as closing a task without inventing the answer. \"paused\" or \
\"stopped\" end the goal's sessions; note there should say why."
            .to_string(),
        parameters: json!({
            "type": "object",
            "properties": {
                "id": { "type": "string", "description": "The goal id." },
                "status": { "type": "string", "enum": ["active", "paused", "done", "stopped"] },
                "plan": { "type": "string", "description": "Replaces the plan in full - phases, todos, whatever helps the next session pick up." },
                "note": {
                    "type": "string",
                    "description": "Required when status is \"done\": the evidence. Otherwise optional: why the status changed."
                }
            },
            "required": ["id"]
        }),
    }
}

pub fn reflect_spec() -> ToolSpec {
    ToolSpec {
        name: "reflect".to_string(),
        description: "Log what this work session found before it ends: anything unexpected, \
and what the next session should do. Written to the goal's own log, which the next session \
reads before it starts."
            .to_string(),
        parameters: json!({
            "type": "object",
            "properties": {
                "unexpected": { "type": "string", "description": "Anything that surprised you this session. Optional if nothing did." },
                "next_steps": { "type": "string", "description": "What the next session should pick up. Optional if the plan already says." }
            }
        }),
    }
}

#[derive(Deserialize)]
struct SetGoalArgs {
    objective: String,
    done_when: String,
    budget_tokens: Option<f64>,
    budget_until: Option<String>,
}

/// `createGoal(db, { botId, objective, doneWhen, ... })`, TS `app.ts:5588`.
pub fn run_set_goal(db: &Arc<std::sync::Mutex<Db>>, bot_id: &str, args: &str) -> String {
    let Ok(parsed) = serde_json::from_str::<SetGoalArgs>(args) else {
        return "Could not read `objective`/`done_when`.".to_string();
    };

    let db = lock_db(db);
    let input = CreateGoalInput {
        bot_id: bot_id.to_string(),
        objective: parsed.objective,
        done_when: parsed.done_when,
        budget_tokens: parsed.budget_tokens,
        budget_until: parsed.budget_until,
    };
    match store::goals::create_goal(&db, input, Utc::now()) {
        Ok(goal) => format!(
            "Goal set: \"{}\" (id {}). Work sessions start on the next hourly tick.",
            goal.objective, goal.id
        ),
        Err(err) => err,
    }
}

#[derive(Deserialize)]
struct UpdateGoalArgs {
    id: String,
    status: Option<String>,
    plan: Option<String>,
    note: Option<String>,
}

/// `updateGoal(db, id, patch, { botId })`, TS `app.ts:7060`. Bot-scoped -
/// another bot's goal id is refused with `store::goals::update_goal`'s own
/// "No goal of yours has that id." text, the same as `update_task`.
pub fn run_update_goal(db: &Arc<std::sync::Mutex<Db>>, bot_id: &str, args: &str) -> String {
    let Ok(parsed) = serde_json::from_str::<UpdateGoalArgs>(args) else {
        return "Could not read `id`.".to_string();
    };

    // TS's `isGoalStatus(input.status)` guard: an invalid status string is
    // silently dropped from the patch (leaves the goal's status untouched)
    // rather than erroring - same as an absent `status` field.
    let status = parsed.status.filter(|s| is_goal_status(s));
    let patch = UpdateGoalPatch {
        status,
        plan: parsed.plan,
        note: parsed.note,
        ..Default::default()
    };

    let db = lock_db(db);
    match store::goals::update_goal(&db, &parsed.id, &patch, Some(bot_id), Utc::now()) {
        Ok(goal) => format!("\"{}\" is now {}.", goal.objective, goal.status),
        Err(err) => err,
    }
}

#[derive(Deserialize, Default)]
struct ReflectArgs {
    unexpected: Option<String>,
    next_steps: Option<String>,
}

/// `reflectOnGoal(db, botId, unexpected, next_steps)`, TS `app.ts:7069`.
/// Neither field is required (no `required` array in the spec above), so
/// unlike `set_goal`/`update_goal` a malformed or empty `args` string falls
/// back to both fields blank instead of refusing the call - `store::goals::
/// reflect_on_goal` already turns "both blank" into its own message rather
/// than a hard error, same as TS.
pub fn run_reflect(db: &Arc<std::sync::Mutex<Db>>, bot_id: &str, args: &str) -> String {
    let parsed: ReflectArgs = serde_json::from_str(args).unwrap_or_default();
    let db = lock_db(db);
    store::goals::reflect_on_goal(
        &db,
        bot_id,
        parsed.unexpected.as_deref().unwrap_or(""),
        parsed.next_steps.as_deref().unwrap_or(""),
        Utc::now(),
    )
    .expect("reflect_on_goal")
}
