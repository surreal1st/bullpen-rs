//! S5b-05 acceptance: tool-kind routines. Tool validation, scheduled firing,
//! and run-now. Ports the TS ranges `routines.ts:280-320` (validation),
//! `:355-380` (create), `:470-500` (update), `:820-870` (scheduled tool branch),
//! `:990-1030` (run-now tool branch).
//!
//! Tests focus on the happy path: tool validation and tool firing when the
//! tool runs successfully. Permission checks, error handling, and the
//! nothing-found marker are integration tests only (relied on by other tests).
//!
//! S5b-05b: the five tests above only ever exercised validation - nothing
//! called `fire_routine_tool` and checked it actually ran the tool and
//! started a run (the same gap a sibling ticket shipped a TODO behind).
//! The three tests below drive `fire_due`/`POST /api/routines/:id/run`
//! against a real `AppState`, the same posture `tests/routines_fire.rs`
//! already takes for prompt-kind routines:
//! - `fire_due_runs_a_due_tool_routine_and_starts_a_run_with_the_tool_result`:
//!   the "say" tool (a real, deterministic tool - no model call inside it)
//!   actually runs, and its result rides the follow-up model prompt under
//!   "## What the tool found" (`routines.rs`'s `fire_routine_tool`).
//! - `post_routine_run_fires_a_tool_routine_regardless_of_schedule_or_active`:
//!   the same firing, reached through the HTTP run-now route, on a routine
//!   that is neither due nor active.
//! - `nothing_new_marker_starts_no_run`: `project_remember` given a project
//!   name that matches none of the bot's real projects echoes that name
//!   back verbatim in its error text (`tools/project_remember.rs`) - so a
//!   tool_args project name set to the literal marker
//!   `[[bullpen:nothing-new]]` makes a REAL tool call return a result that
//!   genuinely contains the marker, without needing to touch src to invent
//!   one. `tool_found_nothing` must then swallow it before any run starts.

mod common;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use common::{ScriptedPort, text_script};
use serde_json::{Value, json};
use server::{AppState, build_app};
use std::sync::Arc;
use store::Db;
use tower::ServiceExt;

fn open_db() -> Db {
    let db = Db::open(":memory:").expect("open :memory: db");
    model::routing::set_routing_settings(&db, Some(false), None)
        .expect("disable routing classifier");
    server::judge::set_judge_enabled(&db, false).expect("disable judge");
    db
}

fn seed_bot(db: &Db, id: &str, name: &str) {
    db.conn()
        .execute(
            "INSERT INTO bots (id, name, purpose, instructions, model, created_at)
             VALUES (?1, ?2, '', 'do things', NULL, ?3)",
            rusqlite::params![id, name, chrono::Utc::now().to_rfc3339()],
        )
        .expect("seed bot");
}

#[tokio::test]
async fn validate_tool_args_must_be_json_object() {
    let db = open_db();
    seed_bot(&db, "bot1", "Bot 1");
    let session_cookie = common::seed_session(&db);

    let port = ScriptedPort::new(vec![text_script("Result.")]);
    let app_state = AppState::with_port(db, Arc::new(port));
    let app = build_app(app_state);

    // Create a routine with kind "tool" and valid JSON object args - should succeed
    let body = json!({
        "botId": "bot1",
        "name": "Get info",
        "prompt": "Summarize what you find",
        "schedule": "every 1 hour",
        "kind": "tool",
        "tool": "ask",
        "toolArgs": "{\"question\": \"What's today's weather?\"}"
    });

    let request = Request::builder()
        .method("POST")
        .uri("/api/routines")
        .header("cookie", &session_cookie)
        .header("content-type", "application/json")
        .body(Body::from(serde_json::to_string(&body).unwrap()))
        .unwrap();

    let response = app.clone().oneshot(request).await.unwrap();
    assert_eq!(response.status(), StatusCode::CREATED);
}

#[tokio::test]
async fn validate_tool_args_rejects_json_array() {
    let db = open_db();
    seed_bot(&db, "bot1", "Bot 1");
    let session_cookie = common::seed_session(&db);

    let port = ScriptedPort::new(vec![text_script("Result.")]);
    let app_state = AppState::with_port(db, Arc::new(port));
    let app = build_app(app_state);

    // Create a routine with kind "tool" but array args - should fail
    let body = json!({
        "botId": "bot1",
        "name": "Get info",
        "prompt": "Summarize what you find",
        "schedule": "every 1 hour",
        "kind": "tool",
        "tool": "ask",
        "toolArgs": "[1, 2, 3]"
    });

    let request = Request::builder()
        .method("POST")
        .uri("/api/routines")
        .header("cookie", &session_cookie)
        .header("content-type", "application/json")
        .body(Body::from(serde_json::to_string(&body).unwrap()))
        .unwrap();

    let response = app.clone().oneshot(request).await.unwrap();
    assert_eq!(response.status(), StatusCode::BAD_REQUEST);

    let body_bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap();
    let body_str = String::from_utf8(body_bytes.to_vec()).unwrap();
    assert!(body_str.contains("tool arguments must be a JSON object"));
}

#[tokio::test]
async fn validate_tool_args_rejects_string_primitive() {
    let db = open_db();
    seed_bot(&db, "bot1", "Bot 1");
    let session_cookie = common::seed_session(&db);

    let port = ScriptedPort::new(vec![text_script("Result.")]);
    let app_state = AppState::with_port(db, Arc::new(port));
    let app = build_app(app_state);

    // Create a routine with kind "tool" but string primitive args - should fail
    let body = json!({
        "botId": "bot1",
        "name": "Get info",
        "prompt": "Summarize what you find",
        "schedule": "every 1 hour",
        "kind": "tool",
        "tool": "ask",
        "toolArgs": "\"just a string\""
    });

    let request = Request::builder()
        .method("POST")
        .uri("/api/routines")
        .header("cookie", &session_cookie)
        .header("content-type", "application/json")
        .body(Body::from(serde_json::to_string(&body).unwrap()))
        .unwrap();

    let response = app.clone().oneshot(request).await.unwrap();
    assert_eq!(response.status(), StatusCode::BAD_REQUEST);

    let body_bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap();
    let body_str = String::from_utf8(body_bytes.to_vec()).unwrap();
    assert!(body_str.contains("tool arguments must be a JSON object"));
}

#[tokio::test]
async fn validate_tool_name_required() {
    let db = open_db();
    seed_bot(&db, "bot1", "Bot 1");
    let session_cookie = common::seed_session(&db);

    let port = ScriptedPort::new(vec![text_script("Result.")]);
    let app_state = AppState::with_port(db, Arc::new(port));
    let app = build_app(app_state);

    // Create a routine with kind "tool" but no tool name - should fail
    let body = json!({
        "botId": "bot1",
        "name": "Get info",
        "prompt": "Summarize what you find",
        "schedule": "every 1 hour",
        "kind": "tool",
        "tool": "",
        "toolArgs": "{}"
    });

    let request = Request::builder()
        .method("POST")
        .uri("/api/routines")
        .header("cookie", &session_cookie)
        .header("content-type", "application/json")
        .body(Body::from(serde_json::to_string(&body).unwrap()))
        .unwrap();

    let response = app.clone().oneshot(request).await.unwrap();
    assert_eq!(response.status(), StatusCode::BAD_REQUEST);

    let body_bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap();
    let body_str = String::from_utf8(body_bytes.to_vec()).unwrap();
    assert!(body_str.contains("Give the routine a tool to run"));
}

#[tokio::test]
async fn validate_empty_tool_args_defaults_to_empty_object() {
    let db = open_db();
    seed_bot(&db, "bot1", "Bot 1");
    let session_cookie = common::seed_session(&db);

    let port = ScriptedPort::new(vec![text_script("Result.")]);
    let app_state = AppState::with_port(db, Arc::new(port));
    let app = build_app(app_state);

    // Create a routine with kind "tool" and empty args - should succeed and default to {}
    let body = json!({
        "botId": "bot1",
        "name": "Get info",
        "prompt": "Summarize what you find",
        "schedule": "every 1 hour",
        "kind": "tool",
        "tool": "ask",
        "toolArgs": ""
    });

    let request = Request::builder()
        .method("POST")
        .uri("/api/routines")
        .header("cookie", &session_cookie)
        .header("content-type", "application/json")
        .body(Body::from(serde_json::to_string(&body).unwrap()))
        .unwrap();

    let response = app.clone().oneshot(request).await.unwrap();
    assert_eq!(response.status(), StatusCode::CREATED);

    let body_bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap();
    let body_str = String::from_utf8(body_bytes.to_vec()).unwrap();
    let response_json: Value = serde_json::from_str(&body_str).unwrap();

    // Check that toolArgs was stored as {}
    assert_eq!(
        response_json["routine"]["toolArgs"],
        json!("{}"),
        "toolArgs should be normalized to '{{}}'"
    );
}

// ---------------------------------------------------------------------
// S5b-05b: actual firing, not just validation.
// ---------------------------------------------------------------------

/// Pulls the text out of a `ModelMessage`'s content - same helper
/// `tests/routines_fire.rs` keeps for the same reason (only `Text` ever
/// appears on a routine's own turn, `Parts` is the multimodal shape other
/// callers use).
fn message_text(msg: &model::ModelMessage) -> &str {
    match &msg.content {
        model::MessageContent::Text(text) => text.as_str(),
        model::MessageContent::Parts(_) => "",
    }
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

/// Polls `GET /api/routines/:id/runs` until its newest run row exists and
/// has left "running" - the run itself finishes on a task `start_routine`
/// spawns, same pattern `tests/routines_fire.rs::wait_for_run` uses.
async fn wait_for_run(app: &axum::Router, session: &str, routine_id: &str) -> Value {
    for _ in 0..300 {
        let (_, body) = get_json(app, &format!("/api/routines/{routine_id}/runs"), session).await;
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
    panic!("run against routine {routine_id} never left 'running'");
}

/// Creates a tool-kind routine directly via `store::create_routine` (kept
/// independent of `POST /api/routines`'s body, same reasoning
/// `tests/routines_fire.rs` gives for prompt-kind routines) and activates
/// it with `next_run_at` stamped to `due_at` - a PAST or CURRENT instant,
/// so `due_routines` finds it without waiting on `schedule::next_run`.
#[allow(clippy::too_many_arguments)]
fn seed_tool_routine(
    db: &Db,
    bot_id: &str,
    name: &str,
    prompt: &str,
    tool: &str,
    tool_args: &str,
    due_at: chrono::DateTime<chrono::Utc>,
) -> String {
    let id = store::create_routine(
        db,
        bot_id,
        name,
        prompt,
        "every 1 hour".to_string(),
        Some(due_at.to_rfc3339()),
        None,
        Some("tool"),
        Some(tool),
        Some(tool_args),
        None,
        None,
        None,
        None,
    )
    .expect("create tool routine");
    store::set_routine_active(db, &id, true, Some(due_at.to_rfc3339())).expect("activate routine");
    id
}

#[tokio::test]
async fn fire_due_runs_a_due_tool_routine_and_starts_a_run_with_the_tool_result() {
    let db = open_db();
    let session = common::seed_session(&db);
    seed_bot(&db, "bot1", "Bot 1");
    let now = chrono::Utc::now();

    // "say" is a real registered tool (`tools/say.rs`) that needs no model
    // call of its own and returns a fixed, checkable result - it actually
    // runs (appends an assistant message) rather than merely validating.
    let id = seed_tool_routine(
        &db,
        "bot1",
        "Check status",
        "Summarize what the tool found.",
        "say",
        r#"{"text":"3 servers need attention"}"#,
        now,
    );

    let scripted = Arc::new(ScriptedPort::new(vec![text_script("Handled.")]));
    let port: Arc<dyn model::ModelPort> = scripted.clone();
    let app_state = AppState::with_port(db, port);
    let app = build_app(app_state.clone());

    let started = server::routines::fire_due(&app_state, now).await;
    assert_eq!(
        started.len(),
        1,
        "a due tool routine whose tool found something must fire exactly once"
    );
    assert_eq!(started[0].0, id);

    let run = wait_for_run(&app, &session, &id).await;
    assert_eq!(run.get("status").and_then(|s| s.as_str()), Some("done"));

    let requests = scripted.requests();
    assert_eq!(
        requests.len(),
        1,
        "exactly one model call - the phrasing turn after the tool ran"
    );
    let last_user = requests[0]
        .messages
        .iter()
        .rev()
        .find(|m| m.role == "user")
        .expect("a user turn carrying the tool result");
    let text = message_text(last_user);
    assert!(
        text.contains("## What the tool found"),
        "the tool result must ride under this heading: {text}"
    );
    assert!(
        text.contains("Said. Josh can see that now."),
        "the ACTUAL result the \"say\" tool returned must ride along, not a \
         stand-in for it: {text}"
    );
}

#[tokio::test]
async fn post_routine_run_fires_a_tool_routine_regardless_of_schedule_or_active() {
    let db = open_db();
    let session = common::seed_session(&db);
    seed_bot(&db, "bot1", "Bot 1");

    // Created but never activated - `store::create_routine` always starts
    // paused (`routines_fire.rs`'s own comment on the same rule), and this
    // never calls `set_routine_active`, so `active = 0` and `next_run_at`
    // is NULL: `due_routines` would never pick this row up.
    let id = store::create_routine(
        &db,
        "bot1",
        "Check status",
        "Summarize what the tool found.",
        "every 1 hour".to_string(),
        None,
        None,
        Some("tool"),
        Some("say"),
        Some(r#"{"text":"disk is at 91%"}"#),
        None,
        None,
        None,
        None,
    )
    .expect("create tool routine");

    let scripted = Arc::new(ScriptedPort::new(vec![text_script("Handled.")]));
    let port: Arc<dyn model::ModelPort> = scripted.clone();
    let app_state = AppState::with_port(db, port);
    let app = build_app(app_state.clone());

    let request = Request::builder()
        .method("POST")
        .uri(format!("/api/routines/{id}/run"))
        .header("cookie", &session)
        .body(Body::empty())
        .unwrap();
    let response = app.clone().oneshot(request).await.unwrap();
    assert_eq!(
        response.status(),
        StatusCode::CREATED,
        "run-now must fire an inactive, never-due tool routine anyway"
    );

    let run = wait_for_run(&app, &session, &id).await;
    assert_eq!(run.get("status").and_then(|s| s.as_str()), Some("done"));

    let requests = scripted.requests();
    assert_eq!(requests.len(), 1);
    let last_user = requests[0]
        .messages
        .iter()
        .rev()
        .find(|m| m.role == "user")
        .expect("a user turn carrying the tool result");
    let text = message_text(last_user);
    assert!(
        text.contains("## What the tool found"),
        "run-now's tool branch must feed the tool result the same way \
         fire_due's does: {text}"
    );
    assert!(text.contains("Said. Josh can see that now."));

    // Confirm run-now really did ignore schedule/active state, not just
    // that it happened to succeed: the routine is still exactly as
    // inactive and undue as it was created.
    let (_, body) = get_json(&app, "/api/routines?bot=bot1", &session).await;
    let row = body
        .get("routines")
        .and_then(|r| r.as_array())
        .and_then(|a| {
            a.iter()
                .find(|r| r.get("id").and_then(|v| v.as_str()) == Some(id.as_str()))
        })
        .expect("routine present in list");
    assert_eq!(
        row.get("active").and_then(|v| v.as_bool()),
        Some(false),
        "run-now must not itself flip the routine active"
    );
}

#[tokio::test]
async fn nothing_new_marker_starts_no_run() {
    let db = open_db();
    let session = common::seed_session(&db);
    seed_bot(&db, "bot1", "Bot 1");
    let now = chrono::Utc::now();

    // `project_remember` is a real registered tool (`tools/
    // project_remember.rs`): given a `project` name that matches none of
    // the bot's actual projects, it echoes that name back verbatim in its
    // error text - `format!("No project named \"{wanted}\". Your
    // projects: {}.", ...)`. Seeding one real (different) project first is
    // what routes execution into that echoing branch rather than the
    // empty-membership one ("You are not a member of any project.", which
    // does not echo anything). Setting the wanted name to the literal
    // marker means the tool's OWN result genuinely contains
    // `[[bullpen:nothing-new]]` - a real tool call, not a stand-in for one.
    let project = store::create_project(&db, "Ops").expect("create project");
    store::add_project_member(&db, &project.id, "bot1").expect("add project member");

    let id = seed_tool_routine(
        &db,
        "bot1",
        "Log to project",
        "File this under the project.",
        "project_remember",
        r#"{"project":"[[bullpen:nothing-new]]","fact":"irrelevant"}"#,
        now,
    );

    let scripted = Arc::new(ScriptedPort::new(vec![text_script(
        "Should never be called.",
    )]));
    let port: Arc<dyn model::ModelPort> = scripted.clone();
    let app_state = AppState::with_port(db, port);
    let app = build_app(app_state.clone());

    let started = server::routines::fire_due(&app_state, now).await;
    assert!(
        started.is_empty(),
        "a tool result declaring NOTHING_NEW must start no run"
    );

    // Give any wrongly-spawned run task a moment to land, then confirm
    // there genuinely is none - not merely that `fire_due` said so.
    tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    let (_, body) = get_json(&app, &format!("/api/routines/{id}/runs"), &session).await;
    let runs = body
        .get("runs")
        .and_then(|r| r.as_array())
        .cloned()
        .unwrap_or_default();
    assert!(
        runs.is_empty(),
        "no run row should exist for a routine whose tool found nothing: {runs:?}"
    );
    assert!(
        scripted.requests().is_empty(),
        "the model must never be called when the tool found nothing"
    );
}
