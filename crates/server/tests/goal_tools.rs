//! S5b-F-04 acceptance: the three goal tools (`set_goal`, `update_goal`,
//! `reflect`, `crates/server/src/tools/goal_tools.rs`) and F16 (`goal_id`
//! stamped in the SAME INSERT `RunManager::start_goal` does, `crates/
//! server/src/runs.rs`, rather than a follow-up `UPDATE` after `start`
//! returns).
//!
//! Drives `RunManager` directly with a `ScriptedPort`, the same posture
//! `tests/memory_tools.rs` already takes for its own four tools - a tool
//! call turn, then a final-answer turn, with the run's own `RunEvent::
//! ToolResult` (drained via `common::drain`) read back for the exact
//! result string, alongside whatever the tool actually wrote to
//! `store::goals`. No HTTP/`AppState` needed for the tool tests: these are
//! tool-dispatch and `RunManager` behaviours, not route behaviours. F16's
//! own test drives `RunManager::start_goal` the same way, since that is
//! exactly the entry point F16 adds.

mod common;

use std::sync::{Arc, Mutex};

use chrono::Utc;
use common::{ScriptedPort, drain, own_conversation, seed_bot, seed_user_message};
use model::ladder::Trigger;
use model::{ModelEvent, ModelMessage, ToolCall};
use server::runs::{RunEvent, RunManager, StartOptions};
use store::Db;
use store::goals::{CreateGoalInput, Goal, GoalLogEntry, GoalRow};

fn open_db() -> Arc<Mutex<Db>> {
    let db = Db::open(":memory:").expect("open :memory: db");
    model::routing::set_routing_settings(&db, Some(false), None)
        .expect("disable routing classifier for scripted-model tests");
    server::judge::set_judge_enabled(&db, false).expect("disable judge for scripted-model tests");
    Arc::new(Mutex::new(db))
}

/// Turn 1 calls `tool_name` with `args_json`; turn 2 answers plainly.
/// Mirrors `tests/memory_tools.rs`'s identical helper (`tool_call_then_answer`).
fn tool_call_then_answer(tool_name: &str, args_json: &str) -> ScriptedPort {
    ScriptedPort::new(vec![
        vec![ModelEvent::ToolCalls {
            calls: vec![ToolCall {
                id: "c1".to_string(),
                name: tool_name.to_string(),
                arguments: args_json.to_string(),
            }],
            usage: None,
        }],
        vec![
            ModelEvent::Delta {
                text: "Done.".to_string(),
            },
            ModelEvent::Done {
                model: "test/model".to_string(),
                usage: None,
                finish_reason: None,
            },
        ],
    ])
}

/// Runs one tool call against a bot's own conversation on an ordinary chat
/// trigger (the goal TOOLS are called by any run, not only a goal session -
/// `set_goal` in particular is always a chat-turn call, per the ticket),
/// returns every event the run emitted.
async fn run_tool_events(
    db: &Arc<Mutex<Db>>,
    bot_id: &str,
    tool_name: &str,
    args_json: &str,
) -> Vec<RunEvent> {
    let conversation_id = own_conversation(db, bot_id);
    seed_user_message(db, &conversation_id, "go");

    let port: Arc<dyn model::ModelPort> = Arc::new(tool_call_then_answer(tool_name, args_json));
    let manager = Arc::new(RunManager::new(Arc::clone(db), port));
    let run_id = manager.start(StartOptions {
        bot_id: bot_id.to_string(),
        conversation_id,
        model: "test/model".to_string(),
        messages: vec![ModelMessage::user("go")],
        trigger: Trigger::Chat,
        room: false,
    });
    drain(manager.subscribe(&run_id)).await
}

/// The `ToolResult` event's own `result` string for `tool_name`, if the run
/// actually called it - `None` means the model never called it (a
/// mis-wired dispatch, or a spec/permission gap), which every test below
/// treats as a hard failure via `.expect`.
fn tool_result(events: &[RunEvent], tool_name: &str) -> Option<String> {
    events.iter().find_map(|e| match e {
        RunEvent::ToolResult { name, result } if name == tool_name => Some(result.clone()),
        _ => None,
    })
}

fn create_goal(db: &Arc<Mutex<Db>>, bot_id: &str, objective: &str, done_when: &str) -> Goal {
    let db = db.lock().expect("db mutex poisoned");
    store::goals::create_goal(
        &db,
        CreateGoalInput {
            bot_id: bot_id.to_string(),
            objective: objective.to_string(),
            done_when: done_when.to_string(),
            budget_tokens: None,
            budget_until: None,
        },
        Utc::now(),
    )
    .expect("create goal")
}

fn goal_row(db: &Arc<Mutex<Db>>, id: &str) -> GoalRow {
    let db = db.lock().expect("db mutex poisoned");
    store::goals::goal_by_id(&db, id)
        .expect("query goal")
        .expect("goal row must exist")
}

fn goal_log(row: &GoalRow) -> Vec<GoalLogEntry> {
    serde_json::from_str(&row.log).unwrap_or_default()
}

// ---------------------------------------------------------------------
// update_goal
// ---------------------------------------------------------------------

#[tokio::test]
async fn update_goal_with_status_done_and_note_closes_the_goal_and_logs_the_note() {
    let db = open_db();
    seed_bot(&db, "arthur", "Arthur");
    let goal = create_goal(&db, "arthur", "Ship S5b-F-04", "F4 and F16 both land clean");

    let args = format!(
        r#"{{"id":"{}","status":"done","note":"evidence: cargo test -p server --test goal_tools all green"}}"#,
        goal.id
    );
    let events = run_tool_events(&db, "arthur", "update_goal", &args).await;

    let result = tool_result(&events, "update_goal").expect("update_goal must have run");
    assert_eq!(result, format!("\"{}\" is now done.", goal.objective));

    let row = goal_row(&db, &goal.id);
    assert_eq!(row.status, "done");
    let log = goal_log(&row);
    assert!(
        log.iter().any(|entry| entry.kind == "note"
            && entry.text == "evidence: cargo test -p server --test goal_tools all green"),
        "goal log must carry the closing note: {log:?}"
    );
}

#[tokio::test]
async fn update_goal_with_another_bots_goal_id_is_refused() {
    let db = open_db();
    seed_bot(&db, "arthur", "Arthur");
    seed_bot(&db, "boris", "Boris");
    let boris_goal = create_goal(&db, "boris", "Boris's own goal", "whatever Boris decides");

    let args = format!(
        r#"{{"id":"{}","status":"done","note":"evidence"}}"#,
        boris_goal.id
    );
    let events = run_tool_events(&db, "arthur", "update_goal", &args).await;

    let result = tool_result(&events, "update_goal").expect("update_goal must have run");
    assert_eq!(result, "No goal of yours has that id.");

    let row = goal_row(&db, &boris_goal.id);
    assert_eq!(
        row.status, "active",
        "a refused update must leave Boris's goal untouched"
    );
}

// ---------------------------------------------------------------------
// reflect
// ---------------------------------------------------------------------

#[tokio::test]
async fn reflect_appends_to_the_goals_own_log() {
    let db = open_db();
    seed_bot(&db, "arthur", "Arthur");
    let goal = create_goal(&db, "arthur", "Ship S5b-F-04", "F4 and F16 both land clean");

    let args = r#"{"unexpected":"the review's line numbers had drifted","next_steps":"read runs.rs fresh next time"}"#;
    let events = run_tool_events(&db, "arthur", "reflect", args).await;

    let result = tool_result(&events, "reflect").expect("reflect must have run");
    assert_eq!(result, format!("Logged on \"{}\".", goal.objective));

    let row = goal_row(&db, &goal.id);
    let log = goal_log(&row);
    assert!(
        log.iter().any(|entry| entry.kind == "reflect"
            && entry.text
                == "Unexpected: the review's line numbers had drifted Next: read runs.rs fresh next time"),
        "goal log must carry the reflection: {log:?}"
    );
}

// ---------------------------------------------------------------------
// set_goal
// ---------------------------------------------------------------------

#[tokio::test]
async fn set_goal_from_a_chat_turn_creates_an_active_goal_for_the_calling_bot() {
    let db = open_db();
    seed_bot(&db, "arthur", "Arthur");

    let args = r#"{"objective":"Ship S5b-F-04","done_when":"F4 and F16 both land clean"}"#;
    let events = run_tool_events(&db, "arthur", "set_goal", args).await;

    let goals = {
        let raw = db.lock().expect("db mutex poisoned");
        store::goals::list_goals(&raw, Some("arthur")).expect("list_goals")
    };
    assert_eq!(goals.len(), 1, "set_goal must create exactly one goal row");
    let goal = &goals[0];
    assert_eq!(goal.status, "active");
    assert_eq!(goal.objective, "Ship S5b-F-04");
    assert_eq!(goal.done_when, "F4 and F16 both land clean");

    let result = tool_result(&events, "set_goal").expect("set_goal must have run");
    assert_eq!(
        result,
        format!(
            "Goal set: \"{}\" (id {}). Work sessions start on the next hourly tick.",
            goal.objective, goal.id
        )
    );
}

// ---------------------------------------------------------------------
// F16 - goal_id in the same INSERT `RunManager::start_goal` does
// ---------------------------------------------------------------------

/// Before F16, `server::goals::fire_goal_session` started the run with the
/// plain `RunManager::start` and stamped `goal_id` with a follow-up
/// `UPDATE runs SET goal_id = ?1 WHERE id = ?2` AFTER `start` returned. A
/// model that errors on its very first frame - exactly what `ScriptedPort`
/// with one `ModelEvent::Error` script produces - can settle (via
/// `on_run_done` -> `settle_goal_run`) before that `UPDATE` ever runs, so
/// `settle_goal_run` reads `goal_id` back as NULL and no-ops: the run's
/// spend never folds into the goal, and it never shows under `goal_runs`.
/// This test drives `RunManager::start_goal` directly (the same entry
/// point `fire_goal_session` now calls) rather than going through the
/// scheduler, so it exercises exactly the INSERT-time stamp F16 adds.
#[tokio::test]
async fn a_fast_failing_goal_session_still_stamps_goal_id_on_its_run_row() {
    let db = open_db();
    seed_bot(&db, "arthur", "Arthur");
    let goal = create_goal(&db, "arthur", "Ship S5b-F-04", "F4 and F16 both land clean");

    let conversation_id = own_conversation(&db, "arthur");
    let port: Arc<dyn model::ModelPort> =
        Arc::new(ScriptedPort::new(vec![vec![ModelEvent::Error {
            message: "missing API key".to_string(),
            status: None,
        }]]));
    let manager = Arc::new(RunManager::new(Arc::clone(&db), port));

    let run_id = manager.start_goal(
        StartOptions {
            bot_id: "arthur".to_string(),
            conversation_id,
            model: "test/model".to_string(),
            messages: vec![ModelMessage::user("[goal: Ship S5b-F-04] work session")],
            trigger: Trigger::Goal,
            room: false,
        },
        goal.id.clone(),
    );
    drain(manager.subscribe(&run_id)).await;

    let raw = db.lock().expect("db mutex poisoned");
    let (status, goal_id_col): (String, Option<String>) = raw
        .conn()
        .query_row(
            "SELECT status, goal_id FROM runs WHERE id = ?1",
            rusqlite::params![run_id],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .expect("read run row");
    assert_eq!(
        status, "failed",
        "the scripted model error must fail the run fast"
    );
    assert_eq!(
        goal_id_col.as_deref(),
        Some(goal.id.as_str()),
        "goal_id must already be set on the run row even though the run \
         errored before any follow-up UPDATE could have run"
    );

    let goal_runs = store::goals::goal_runs(&raw, &goal.id, 20).expect("goal_runs");
    assert_eq!(
        goal_runs.len(),
        1,
        "the fast-failing run must still show up under the goal's own runs"
    );
    assert_eq!(goal_runs[0].id, run_id);
}
