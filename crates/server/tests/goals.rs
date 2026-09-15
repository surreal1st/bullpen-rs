//! S5b-04 acceptance: the goal scheduler (`fire_due_goals`/`run_goal_now`/
//! `settle_goal_run`/`is_quiet_hours`) and the `/api/goals*` routes.
//!
//! `AppState`'s `db` field is `pub(crate)` (`lib.rs`'s `AppState::db` doc) -
//! this suite is an external integration-test crate, so most assertions go
//! through HTTP, same posture `tests/routines_fire.rs`/`tests/
//! routines_routes.rs` already take. `settle_goal_run`'s own tests need a
//! run row inserted WHILE `AppState` is alive (the run it settles is
//! supposed to already exist), which a `:memory:` db cannot support once
//! `AppState` owns the only handle to it - those tests use a temp FILE db
//! instead, opening a second `Db::open` handle on the same path purely for
//! that raw `INSERT INTO runs` a test needs and nothing production code
//! ever needs to do (a real run row is always written by `RunManager`
//! itself). WAL mode + the 5s busy_timeout `Db::open` already sets make two
//! sequential (never concurrent) connections to one file safe.

mod common;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use chrono::{DateTime, Local, TimeZone, Utc};
use common::{ScriptedPort, seed_session, text_script};
use serde_json::{Value, json};
use server::goals as scheduler;
use server::{AppState, build_app};
use std::sync::Arc;
use store::Db;
use store::goals::{CreateGoalInput, NO_TOOL_PAUSE_REASON};
use tower::ServiceExt;

fn open_db() -> Db {
    let db = Db::open(":memory:").expect("open :memory: db");
    model::routing::set_routing_settings(&db, Some(false), None)
        .expect("disable routing classifier");
    server::judge::set_judge_enabled(&db, false).expect("disable judge");
    db
}

/// Opens a file-backed db (unlike every other helper here) so a test can
/// hold a SECOND raw connection alive alongside the one `AppState` owns -
/// see this file's own module doc for why `settle_goal_run` tests need it.
fn open_file_db() -> (Db, String) {
    let path = std::env::temp_dir().join(format!("bullpen-goals-test-{}.db", uuid::Uuid::new_v4()));
    let path_str = path.to_str().expect("utf8 temp path").to_string();
    let db = Db::open(&path_str).expect("open file db");
    model::routing::set_routing_settings(&db, Some(false), None)
        .expect("disable routing classifier");
    server::judge::set_judge_enabled(&db, false).expect("disable judge");
    (db, path_str)
}

fn seed_bot(db: &Db, id: &str, model: Option<&str>) {
    db.conn()
        .execute(
            "INSERT INTO bots (id, name, purpose, instructions, model, created_at)
             VALUES (?1, ?2, '', 'do things', ?3, ?4)",
            rusqlite::params![id, id, model, chrono::Utc::now().to_rfc3339()],
        )
        .expect("seed bot");
}

/// Inserts a run row directly - the shape `settle_goal_run` reads
/// (`goal_id`, `messages`, `input_tokens`, `output_tokens`), skipping the
/// real `RunManager::start`/`drive` machinery entirely since these tests are
/// about the SETTLING half, not driving a model turn.
fn insert_run_row(
    db: &Db,
    bot_id: &str,
    goal_id: &str,
    messages_json: &str,
    input_tokens: i64,
    output_tokens: i64,
) -> String {
    let conversation_id =
        store::get_or_create_conversation(db, bot_id).expect("get_or_create_conversation");
    let run_id = uuid::Uuid::new_v4().to_string();
    let now = chrono::Utc::now().to_rfc3339();
    db.conn()
        .execute(
            "INSERT INTO runs (id, bot_id, conversation_id, trigger, status, model, messages, \
                                goal_id, input_tokens, output_tokens, created_at, updated_at)
             VALUES (?1, ?2, ?3, 'goal', 'done', 'test/model', ?4, ?5, ?6, ?7, ?8, ?8)",
            rusqlite::params![
                run_id,
                bot_id,
                conversation_id,
                messages_json,
                goal_id,
                input_tokens,
                output_tokens,
                now,
            ],
        )
        .expect("insert run row");
    run_id
}

fn no_tool_messages() -> &'static str {
    r#"[{"role":"assistant","content":"done"}]"#
}

fn tool_call_messages() -> &'static str {
    r#"[{"role":"assistant","content":"working"},{"role":"tool","content":"result"}]"#
}

async fn get_json(app: &axum::Router, path: &str, session: &str) -> (StatusCode, Value) {
    let request = Request::get(path)
        .header("cookie", session)
        .body(Body::empty())
        .unwrap();
    let response = app.clone().oneshot(request).await.unwrap();
    let status = response.status();
    let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap();
    (
        status,
        serde_json::from_slice(&bytes).unwrap_or(Value::Null),
    )
}

async fn post_json(
    app: &axum::Router,
    path: &str,
    session: &str,
    body: Value,
) -> (StatusCode, Value) {
    let request = Request::post(path)
        .header("cookie", session)
        .header("content-type", "application/json")
        .body(Body::from(body.to_string()))
        .unwrap();
    let response = app.clone().oneshot(request).await.unwrap();
    let status = response.status();
    let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap();
    (
        status,
        serde_json::from_slice(&bytes).unwrap_or(Value::Null),
    )
}

async fn patch_json(
    app: &axum::Router,
    path: &str,
    session: &str,
    body: Value,
) -> (StatusCode, Value) {
    let request = Request::patch(path)
        .header("cookie", session)
        .header("content-type", "application/json")
        .body(Body::from(body.to_string()))
        .unwrap();
    let response = app.clone().oneshot(request).await.unwrap();
    let status = response.status();
    let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap();
    (
        status,
        serde_json::from_slice(&bytes).unwrap_or(Value::Null),
    )
}

async fn delete_json(app: &axum::Router, path: &str, session: &str) -> (StatusCode, Value) {
    let request = Request::delete(path)
        .header("cookie", session)
        .body(Body::empty())
        .unwrap();
    let response = app.clone().oneshot(request).await.unwrap();
    let status = response.status();
    let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap();
    (
        status,
        serde_json::from_slice(&bytes).unwrap_or(Value::Null),
    )
}

/// Finds a goal by id out of `GET /api/goals?bot=...` - there is no
/// single-goal GET route (TS has none either, only the list).
async fn goal_row(app: &axum::Router, session: &str, bot_id: &str, goal_id: &str) -> Value {
    let (_, body) = get_json(app, &format!("/api/goals?bot={bot_id}"), session).await;
    body.get("goals")
        .and_then(|g| g.as_array())
        .and_then(|a| {
            a.iter()
                .find(|g| g.get("id").and_then(|v| v.as_str()) == Some(goal_id))
        })
        .cloned()
        .expect("goal present in list")
}

async fn wait_for_goal_run(app: &axum::Router, session: &str, goal_id: &str) -> Value {
    for _ in 0..300 {
        let (_, body) = get_json(app, &format!("/api/goals/{goal_id}/runs"), session).await;
        if let Some(run) = body
            .get("runs")
            .and_then(|r| r.as_array())
            .and_then(|a| a.first())
            && run.get("status").and_then(|s| s.as_str()) != Some("running")
        {
            return run.clone();
        }
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
    panic!("run against goal {goal_id} never left 'running'");
}

fn local_wall_clock_utc(hour: u32, minute: u32) -> DateTime<Utc> {
    let today = Local::now().date_naive();
    Local
        .from_local_datetime(&today.and_hms_opt(hour, minute, 0).unwrap())
        .single()
        .expect("unambiguous local time")
        .with_timezone(&Utc)
}

// ---------------------------------------------------------------------
// fire_due_goals
// ---------------------------------------------------------------------

#[tokio::test]
async fn fires_a_due_goal_once_and_reschedules_so_it_does_not_fire_twice() {
    let db = open_db();
    let session = seed_session(&db);
    seed_bot(&db, "arthur", None);
    let now = local_wall_clock_utc(9, 0);
    let goal = store::goals::create_goal(
        &db,
        CreateGoalInput {
            bot_id: "arthur".to_string(),
            objective: "Ship the thing".to_string(),
            done_when: "It ships".to_string(),
            budget_tokens: None,
            budget_until: None,
        },
        now,
    )
    .expect("create goal");

    let port: Arc<dyn model::ModelPort> = Arc::new(ScriptedPort::new(vec![text_script("done")]));
    let state = AppState::with_port(db, port);
    let app = build_app(state.clone());

    let started = scheduler::fire_due_goals(&state, now);
    assert_eq!(started.len(), 1, "the due goal should fire exactly once");
    assert_eq!(started[0].0, goal.id);

    let started_again = scheduler::fire_due_goals(&state, now);
    assert!(
        started_again.is_empty(),
        "must not fire twice for one due instant"
    );

    let row = goal_row(&app, &session, "arthur", &goal.id).await;
    let next_session_at = row
        .get("nextSessionAt")
        .and_then(|v| v.as_str())
        .expect("nextSessionAt set");
    let next_session_at: DateTime<Utc> = next_session_at.parse().expect("valid timestamp");
    assert!(
        next_session_at > now,
        "next_session_at must advance past the fired instant"
    );

    let run = wait_for_goal_run(&app, &session, &goal.id).await;
    assert_eq!(run.get("status").and_then(|s| s.as_str()), Some("done"));
}

#[tokio::test]
async fn a_goal_not_yet_due_does_not_fire() {
    let db = open_db();
    seed_bot(&db, "arthur", None);
    let now = local_wall_clock_utc(9, 0);
    let goal = store::goals::create_goal(
        &db,
        CreateGoalInput {
            bot_id: "arthur".to_string(),
            objective: "x".to_string(),
            done_when: "y".to_string(),
            budget_tokens: None,
            budget_until: None,
        },
        now,
    )
    .expect("create goal");
    let future = (now + chrono::Duration::hours(2)).to_rfc3339();
    db.conn()
        .execute(
            "UPDATE goals SET next_session_at = ?1 WHERE id = ?2",
            rusqlite::params![future, goal.id],
        )
        .expect("push next_session_at into the future");

    let port: Arc<dyn model::ModelPort> = Arc::new(ScriptedPort::new(vec![text_script("done")]));
    let state = AppState::with_port(db, port);

    let started = scheduler::fire_due_goals(&state, now);
    assert!(started.is_empty(), "a goal not yet due must not fire");
}

#[tokio::test]
async fn a_goal_past_its_token_budget_stops_instead_of_firing() {
    let db = open_db();
    let session = seed_session(&db);
    seed_bot(&db, "arthur", None);
    let now = local_wall_clock_utc(9, 0);
    let goal = store::goals::create_goal(
        &db,
        CreateGoalInput {
            bot_id: "arthur".to_string(),
            objective: "Ship it".to_string(),
            done_when: "y".to_string(),
            budget_tokens: Some(100.0),
            budget_until: None,
        },
        now,
    )
    .expect("create goal");
    db.conn()
        .execute(
            "UPDATE goals SET spent_tokens = 150 WHERE id = ?1",
            rusqlite::params![goal.id],
        )
        .expect("seed overspend");

    let port: Arc<dyn model::ModelPort> = Arc::new(ScriptedPort::new(vec![text_script("done")]));
    let state = AppState::with_port(db, port);
    let app = build_app(state.clone());

    let started = scheduler::fire_due_goals(&state, now);
    assert!(
        started.is_empty(),
        "a goal already over budget must not spend a session"
    );

    let row = goal_row(&app, &session, "arthur", &goal.id).await;
    assert_eq!(row.get("status").and_then(|v| v.as_str()), Some("stopped"));
    assert_eq!(
        row.get("reason").and_then(|v| v.as_str()),
        Some("Stopped \"Ship it\": used 150 of its 100-token budget.")
    );
}

/// The BITE target for S5b-04: quiet hours (23:00-07:00 local) apply to
/// goals, not routines. A goal due at 02:00 local must not fire, and its
/// `next_session_at` must land at 07:00 local (`next_workable_session`),
/// not one ordinary session cadence later.
#[tokio::test]
async fn a_goal_due_during_quiet_hours_does_not_fire_and_reschedules_to_seven_am() {
    let db = open_db();
    let session = seed_session(&db);
    seed_bot(&db, "arthur", None);
    let night_now = local_wall_clock_utc(2, 0);
    let goal = store::goals::create_goal(
        &db,
        CreateGoalInput {
            bot_id: "arthur".to_string(),
            objective: "x".to_string(),
            done_when: "y".to_string(),
            budget_tokens: None,
            budget_until: None,
        },
        night_now,
    )
    .expect("create goal");

    let port: Arc<dyn model::ModelPort> = Arc::new(ScriptedPort::new(vec![text_script("done")]));
    let state = AppState::with_port(db, port);
    let app = build_app(state.clone());

    let started = scheduler::fire_due_goals(&state, night_now);
    assert!(
        started.is_empty(),
        "a goal due at 02:00 local must not fire"
    );

    let row = goal_row(&app, &session, "arthur", &goal.id).await;
    let next_session_at = row
        .get("nextSessionAt")
        .and_then(|v| v.as_str())
        .expect("nextSessionAt set")
        .parse::<DateTime<Utc>>()
        .expect("valid timestamp");
    let expected = local_wall_clock_utc(7, 0);
    assert_eq!(
        next_session_at, expected,
        "a quiet-hours goal must reschedule to 07:00 local, not an ordinary hour later"
    );
}

/// Proves `Trigger::Goal` actually reaches `model_for_run`'s cheap-model
/// floor: even with the PLATFORM default itself set to a premium model, a
/// goal session must never call it.
#[tokio::test]
async fn a_goal_session_never_reaches_a_premium_platform_default() {
    let db = open_db();
    seed_bot(&db, "arthur", None);
    model::ladder::set_default_model(&db, "anthropic/claude-fable-5.1");
    let now = local_wall_clock_utc(9, 0);
    let goal = store::goals::create_goal(
        &db,
        CreateGoalInput {
            bot_id: "arthur".to_string(),
            objective: "x".to_string(),
            done_when: "y".to_string(),
            budget_tokens: None,
            budget_until: None,
        },
        now,
    )
    .expect("create goal");

    let scripted = Arc::new(ScriptedPort::new(vec![text_script("done")]));
    let port: Arc<dyn model::ModelPort> = scripted.clone();
    let state = AppState::with_port(db, port);

    let started = scheduler::fire_due_goals(&state, now);
    assert_eq!(started.len(), 1);
    assert_eq!(started[0].0, goal.id);

    for _ in 0..300 {
        if !scripted.requests().is_empty() {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }

    let requests = scripted.requests();
    assert_eq!(requests.len(), 1, "exactly one model call for one firing");
    assert_eq!(
        requests[0].model,
        model::CHEAP_DEFAULT_MODEL,
        "Trigger::Goal must take the same cheap-model floor a routine does"
    );
}

// ---------------------------------------------------------------------
// run_goal_now
// ---------------------------------------------------------------------

#[tokio::test]
async fn run_goal_now_fires_regardless_of_schedule() {
    let db = open_db();
    let session = seed_session(&db);
    seed_bot(&db, "arthur", None);
    let now = local_wall_clock_utc(9, 0);
    let goal = store::goals::create_goal(
        &db,
        CreateGoalInput {
            bot_id: "arthur".to_string(),
            objective: "x".to_string(),
            done_when: "y".to_string(),
            budget_tokens: None,
            budget_until: None,
        },
        now,
    )
    .expect("create goal");
    // Push next_session_at far into the future - "run now" must ignore it.
    let future = (now + chrono::Duration::days(1)).to_rfc3339();
    db.conn()
        .execute(
            "UPDATE goals SET next_session_at = ?1 WHERE id = ?2",
            rusqlite::params![future, goal.id],
        )
        .expect("push next_session_at");

    let port: Arc<dyn model::ModelPort> = Arc::new(ScriptedPort::new(vec![text_script("done")]));
    let state = AppState::with_port(db, port);
    let app = build_app(state.clone());

    let result = scheduler::run_goal_now(&state, &goal.id, now);
    assert!(
        result.is_ok(),
        "run_goal_now must fire regardless of schedule"
    );

    let run = wait_for_goal_run(&app, &session, &goal.id).await;
    assert_eq!(run.get("status").and_then(|s| s.as_str()), Some("done"));
}

#[tokio::test]
async fn run_goal_now_errors_for_an_unknown_goal() {
    let db = open_db();
    let port: Arc<dyn model::ModelPort> = Arc::new(ScriptedPort::new(vec![]));
    let state = AppState::with_port(db, port);

    let result = scheduler::run_goal_now(&state, "does-not-exist", Utc::now());
    assert_eq!(result, Err("no such goal".to_string()));
}

// ---------------------------------------------------------------------
// settle_goal_run
// ---------------------------------------------------------------------

#[tokio::test]
async fn settle_goal_run_pauses_after_three_no_tool_sessions_in_a_row() {
    let (db, path) = open_file_db();
    seed_bot(&db, "arthur", None);
    let now = Utc::now();
    let goal = store::goals::create_goal(
        &db,
        CreateGoalInput {
            bot_id: "arthur".to_string(),
            objective: "x".to_string(),
            done_when: "y".to_string(),
            budget_tokens: None,
            budget_until: None,
        },
        now,
    )
    .expect("create goal");

    let port: Arc<dyn model::ModelPort> = Arc::new(ScriptedPort::new(vec![]));
    let state = AppState::with_port(db, port);
    let app = build_app(state.clone());
    let session = {
        // `seed_session` needs a `&Db` - the raw handle below, opened after
        // `AppState` already took the first one (see this file's module doc).
        let raw = Db::open(&path).expect("reopen file db for session seed");
        seed_session(&raw)
    };

    for _ in 0..3 {
        let raw = Db::open(&path).expect("reopen file db");
        let run_id = insert_run_row(&raw, "arthur", &goal.id, no_tool_messages(), 5, 5);
        scheduler::settle_goal_run(&state, &run_id, now);
    }

    let row = goal_row(&app, &session, "arthur", &goal.id).await;
    assert_eq!(row.get("status").and_then(|v| v.as_str()), Some("paused"));
    assert_eq!(
        row.get("reason").and_then(|v| v.as_str()),
        Some(NO_TOOL_PAUSE_REASON)
    );
}

#[tokio::test]
async fn settle_goal_run_resets_the_no_tool_streak_when_a_tool_ran() {
    let (db, path) = open_file_db();
    seed_bot(&db, "arthur", None);
    let now = Utc::now();
    let goal = store::goals::create_goal(
        &db,
        CreateGoalInput {
            bot_id: "arthur".to_string(),
            objective: "x".to_string(),
            done_when: "y".to_string(),
            budget_tokens: None,
            budget_until: None,
        },
        now,
    )
    .expect("create goal");

    let port: Arc<dyn model::ModelPort> = Arc::new(ScriptedPort::new(vec![]));
    let state = AppState::with_port(db, port);
    let app = build_app(state.clone());
    let session = {
        let raw = Db::open(&path).expect("reopen file db for session seed");
        seed_session(&raw)
    };

    // no-tool, tool (resets), no-tool, no-tool - streak ends at 2, never
    // reaching NO_TOOL_LIMIT (3), which only holds if the tool call in the
    // middle actually reset it back to 0.
    let sequence = [
        no_tool_messages(),
        tool_call_messages(),
        no_tool_messages(),
        no_tool_messages(),
    ];
    for messages in sequence {
        let raw = Db::open(&path).expect("reopen file db");
        let run_id = insert_run_row(&raw, "arthur", &goal.id, messages, 5, 5);
        scheduler::settle_goal_run(&state, &run_id, now);
    }

    let row = goal_row(&app, &session, "arthur", &goal.id).await;
    assert_eq!(
        row.get("status").and_then(|v| v.as_str()),
        Some("active"),
        "a tool call mid-sequence must reset the no-tool streak, not just slow it"
    );
}

#[tokio::test]
async fn settle_goal_run_stops_the_goal_when_spend_crosses_its_budget() {
    let (db, path) = open_file_db();
    seed_bot(&db, "arthur", None);
    let now = Utc::now();
    let goal = store::goals::create_goal(
        &db,
        CreateGoalInput {
            bot_id: "arthur".to_string(),
            objective: "Ship it".to_string(),
            done_when: "y".to_string(),
            budget_tokens: Some(100.0),
            budget_until: None,
        },
        now,
    )
    .expect("create goal");

    let port: Arc<dyn model::ModelPort> = Arc::new(ScriptedPort::new(vec![]));
    let state = AppState::with_port(db, port);
    let app = build_app(state.clone());
    let session = {
        let raw = Db::open(&path).expect("reopen file db for session seed");
        seed_session(&raw)
    };

    let raw = Db::open(&path).expect("reopen file db");
    // A tool call, so this stays on the budget path rather than the
    // no-tool-streak path.
    let run_id = insert_run_row(&raw, "arthur", &goal.id, tool_call_messages(), 60, 50);
    scheduler::settle_goal_run(&state, &run_id, now);

    let row = goal_row(&app, &session, "arthur", &goal.id).await;
    assert_eq!(row.get("status").and_then(|v| v.as_str()), Some("stopped"));
    assert_eq!(row.get("spentTokens").and_then(|v| v.as_i64()), Some(110));
    assert_eq!(
        row.get("reason").and_then(|v| v.as_str()),
        Some("Stopped \"Ship it\": used 110 of its 100-token budget.")
    );
}

#[tokio::test]
async fn settle_goal_run_is_a_noop_when_the_goal_is_already_closed() {
    let (db, path) = open_file_db();
    seed_bot(&db, "arthur", None);
    let now = Utc::now();
    let goal = store::goals::create_goal(
        &db,
        CreateGoalInput {
            bot_id: "arthur".to_string(),
            objective: "x".to_string(),
            done_when: "y".to_string(),
            budget_tokens: None,
            budget_until: None,
        },
        now,
    )
    .expect("create goal");
    let mut patch = store::goals::UpdateGoalPatch::default();
    patch.status = Some("done".to_string());
    patch.note = Some("finished already".to_string());
    store::goals::update_goal(&db, &goal.id, &patch, None, now).expect("close goal");

    let port: Arc<dyn model::ModelPort> = Arc::new(ScriptedPort::new(vec![]));
    let state = AppState::with_port(db, port);
    let app = build_app(state.clone());
    let session = {
        let raw = Db::open(&path).expect("reopen file db for session seed");
        seed_session(&raw)
    };

    let raw = Db::open(&path).expect("reopen file db");
    let run_id = insert_run_row(&raw, "arthur", &goal.id, tool_call_messages(), 999, 999);
    scheduler::settle_goal_run(&state, &run_id, now);

    let row = goal_row(&app, &session, "arthur", &goal.id).await;
    assert_eq!(
        row.get("status").and_then(|v| v.as_str()),
        Some("done"),
        "a closed goal must not be reopened or re-bookkept by a late settle"
    );
    assert_eq!(
        row.get("spentTokens").and_then(|v| v.as_i64()),
        Some(0),
        "spend must not fold in once the goal is no longer active"
    );
}

#[tokio::test]
async fn settle_goal_run_is_a_noop_for_a_run_with_no_goal_id() {
    let db = open_db();
    seed_bot(&db, "arthur", None);
    let conversation_id =
        store::get_or_create_conversation(&db, "arthur").expect("get_or_create_conversation");
    let run_id = uuid::Uuid::new_v4().to_string();
    let now_str = chrono::Utc::now().to_rfc3339();
    db.conn()
        .execute(
            "INSERT INTO runs (id, bot_id, conversation_id, trigger, status, model, messages, created_at, updated_at)
             VALUES (?1, 'arthur', ?2, 'chat', 'done', 'test/model', '[]', ?3, ?3)",
            rusqlite::params![run_id, conversation_id, now_str],
        )
        .expect("insert ordinary chat run with no goal_id");

    let port: Arc<dyn model::ModelPort> = Arc::new(ScriptedPort::new(vec![]));
    let state = AppState::with_port(db, port);

    // Must not panic - the only observable behaviour for a run with no
    // `goal_id` is that nothing happens.
    scheduler::settle_goal_run(&state, &run_id, Utc::now());
}

// ---------------------------------------------------------------------
// routes
// ---------------------------------------------------------------------

#[tokio::test]
async fn post_goals_creates_and_get_goals_lists_it() {
    let db = open_db();
    let session = seed_session(&db);
    seed_bot(&db, "arthur", None);
    let port: Arc<dyn model::ModelPort> = Arc::new(ScriptedPort::new(vec![]));
    let state = AppState::with_port(db, port);
    let app = build_app(state);

    let (status, body) = post_json(
        &app,
        "/api/goals",
        &session,
        json!({ "botId": "arthur", "objective": "Ship it", "doneWhen": "It ships" }),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED);
    let id = body["goal"]["id"].as_str().expect("goal id").to_string();
    assert_eq!(body["goal"]["objective"], "Ship it");
    assert_eq!(body["goal"]["status"], "active");

    let (status, body) = get_json(&app, "/api/goals?bot=arthur", &session).await;
    assert_eq!(status, StatusCode::OK);
    let goals = body["goals"].as_array().expect("goals array");
    assert!(goals.iter().any(|g| g["id"] == id));
}

#[tokio::test]
async fn post_goals_rejects_an_unknown_bot() {
    let db = open_db();
    let session = seed_session(&db);
    let port: Arc<dyn model::ModelPort> = Arc::new(ScriptedPort::new(vec![]));
    let state = AppState::with_port(db, port);
    let app = build_app(state);

    let (status, body) = post_json(
        &app,
        "/api/goals",
        &session,
        json!({ "botId": "nobody", "objective": "x", "doneWhen": "y" }),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(body["error"], "no such bot");
}

#[tokio::test]
async fn patch_goal_requires_a_note_to_close_it_done() {
    let db = open_db();
    let session = seed_session(&db);
    seed_bot(&db, "arthur", None);
    let now = Utc::now();
    let goal = store::goals::create_goal(
        &db,
        CreateGoalInput {
            bot_id: "arthur".to_string(),
            objective: "x".to_string(),
            done_when: "y".to_string(),
            budget_tokens: Some(500.0),
            budget_until: None,
        },
        now,
    )
    .expect("create goal");
    let port: Arc<dyn model::ModelPort> = Arc::new(ScriptedPort::new(vec![]));
    let state = AppState::with_port(db, port);
    let app = build_app(state);

    let (status, body) = patch_json(
        &app,
        &format!("/api/goals/{}", goal.id),
        &session,
        json!({ "status": "done" }),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert!(
        body["error"]
            .as_str()
            .unwrap_or_default()
            .contains("Say what evidence")
    );

    // An explicit `null` clears the budget - proves the route tells a
    // missing key apart from a `null` one (this module's own doc).
    let (status, body) = patch_json(
        &app,
        &format!("/api/goals/{}", goal.id),
        &session,
        json!({ "status": "done", "note": "Shipped, see PR #1", "budgetTokens": null }),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["goal"]["status"], "done");
    assert!(body["goal"]["budgetTokens"].is_null());
}

#[tokio::test]
async fn patch_goal_404s_for_an_unknown_id() {
    let db = open_db();
    let session = seed_session(&db);
    let port: Arc<dyn model::ModelPort> = Arc::new(ScriptedPort::new(vec![]));
    let state = AppState::with_port(db, port);
    let app = build_app(state);

    let (status, _) = patch_json(
        &app,
        "/api/goals/does-not-exist",
        &session,
        json!({ "plan": "new plan" }),
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn delete_goal_removes_it_and_404s_the_second_time() {
    let db = open_db();
    let session = seed_session(&db);
    seed_bot(&db, "arthur", None);
    let now = Utc::now();
    let goal = store::goals::create_goal(
        &db,
        CreateGoalInput {
            bot_id: "arthur".to_string(),
            objective: "x".to_string(),
            done_when: "y".to_string(),
            budget_tokens: None,
            budget_until: None,
        },
        now,
    )
    .expect("create goal");
    let port: Arc<dyn model::ModelPort> = Arc::new(ScriptedPort::new(vec![]));
    let state = AppState::with_port(db, port);
    let app = build_app(state);

    let (status, body) = delete_json(&app, &format!("/api/goals/{}", goal.id), &session).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["ok"], true);

    let (status, _) = delete_json(&app, &format!("/api/goals/{}", goal.id), &session).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

/// The route calls `chrono::Utc::now()` internally (the REAL clock), same
/// as `POST /api/routines/tick` does for `routines::fire_due` - so the goal
/// seeded here is due relative to whatever "now" actually is when the
/// request lands, not a fixed wall-clock hour (which would make this test
/// flaky depending on what time of day it happens to run). Quiet hours are
/// real too: this test asks `is_quiet_hours` what the ACTUAL current hour
/// says, rather than assuming daytime, so it is correct at 03:00 local on a
/// CI box exactly as it is at noon.
#[tokio::test]
async fn goals_tick_route_fires_the_same_thing_the_scheduler_would() {
    let db = open_db();
    let session = seed_session(&db);
    seed_bot(&db, "arthur", None);
    let due_at = Utc::now() - chrono::Duration::hours(1);
    let goal = store::goals::create_goal(
        &db,
        CreateGoalInput {
            bot_id: "arthur".to_string(),
            objective: "x".to_string(),
            done_when: "y".to_string(),
            budget_tokens: None,
            budget_until: None,
        },
        due_at,
    )
    .expect("create goal");

    let port: Arc<dyn model::ModelPort> = Arc::new(ScriptedPort::new(vec![text_script("done")]));
    let state = AppState::with_port(db, port);
    let app = build_app(state);

    let real_now = Utc::now();
    let (status, body) = post_json(&app, "/api/goals/tick", &session, json!({})).await;
    assert_eq!(status, StatusCode::OK);
    let started = body["started"].as_array().expect("started array");

    if scheduler::is_quiet_hours(real_now) {
        assert!(
            started.is_empty(),
            "quiet hours: the tick route must defer rather than fire, same as the scheduler"
        );
    } else {
        assert_eq!(started.len(), 1);
        assert_eq!(started[0]["goalId"], goal.id);
    }
}

#[tokio::test]
async fn goal_run_route_404s_for_an_unknown_goal() {
    let db = open_db();
    let session = seed_session(&db);
    let port: Arc<dyn model::ModelPort> = Arc::new(ScriptedPort::new(vec![]));
    let state = AppState::with_port(db, port);
    let app = build_app(state);

    let (status, body) =
        post_json(&app, "/api/goals/does-not-exist/run", &session, json!({})).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert_eq!(body["error"], "no such goal");
}
