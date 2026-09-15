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

    // F17: this used to name "ask" here - not a toolbox name (`ask_josh`
    // is; plain "ask" never was) - so it was an ANTI-GUARD: it asserted
    // CREATED for a row F1's fix now refuses, and green here meant nothing
    // downstream ever noticed. "say" is a real toolbox name.
    let body = json!({
        "botId": "bot1",
        "name": "Get info",
        "prompt": "Summarize what you find",
        "schedule": "every 1 hour",
        "kind": "tool",
        "tool": "say",
        "toolArgs": "{\"text\": \"What's today's weather?\"}"
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

    // F17: also assert the actual claim this test's name makes - a
    // non-object `toolArgs` body (here, a bare number) is refused with the
    // TS text, on a VALID tool name, so the 400 is unambiguously about the
    // args shape and not (as it would be, checked first) the tool name.
    let bad_body = json!({
        "botId": "bot1",
        "name": "Get info 2",
        "prompt": "Summarize what you find",
        "schedule": "every 1 hour",
        "kind": "tool",
        "tool": "say",
        "toolArgs": "42"
    });
    let bad_request = Request::builder()
        .method("POST")
        .uri("/api/routines")
        .header("cookie", &session_cookie)
        .header("content-type", "application/json")
        .body(Body::from(serde_json::to_string(&bad_body).unwrap()))
        .unwrap();
    let bad_response = app.clone().oneshot(bad_request).await.unwrap();
    assert_eq!(bad_response.status(), StatusCode::BAD_REQUEST);
    let bad_bytes = axum::body::to_bytes(bad_response.into_body(), usize::MAX)
        .await
        .unwrap();
    let bad_str = String::from_utf8(bad_bytes.to_vec()).unwrap();
    assert!(
        bad_str.contains("tool arguments must be a JSON object"),
        "got: {bad_str}"
    );
}

#[tokio::test]
async fn validate_tool_args_rejects_json_array() {
    let db = open_db();
    seed_bot(&db, "bot1", "Bot 1");
    let session_cookie = common::seed_session(&db);

    let port = ScriptedPort::new(vec![text_script("Result.")]);
    let app_state = AppState::with_port(db, Arc::new(port));
    let app = build_app(app_state);

    // Create a routine with kind "tool" but array args - should fail. F17:
    // "say" (not "ask", never a toolbox name) so a 400 here is
    // unambiguously about the args shape, checked after the tool name.
    let body = json!({
        "botId": "bot1",
        "name": "Get info",
        "prompt": "Summarize what you find",
        "schedule": "every 1 hour",
        "kind": "tool",
        "tool": "say",
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

    // Create a routine with kind "tool" but string primitive args - should
    // fail. F17: "say" (not "ask", never a toolbox name), same reasoning
    // as the array case above.
    let body = json!({
        "botId": "bot1",
        "name": "Get info",
        "prompt": "Summarize what you find",
        "schedule": "every 1 hour",
        "kind": "tool",
        "tool": "say",
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
async fn unknown_tool_name_refused_at_save_time_on_post_and_patch() {
    let db = open_db();
    seed_bot(&db, "bot1", "Bot 1");
    let session_cookie = common::seed_session(&db);

    let port = ScriptedPort::new(vec![text_script("Result.")]);
    let app_state = AppState::with_port(db, Arc::new(port));
    let app = build_app(app_state);

    // F1: `fetch_url` is on the TS tool catalog (so it carries a default
    // `Allow` permission decision) but was never ported to this toolbox's
    // 13 names - exactly the name the review names as the bug (`reviews/
    // S5b-R.md`, F1).
    let body = json!({
        "botId": "bot1",
        "name": "Get info",
        "prompt": "Summarize what you find",
        "schedule": "every 1 hour",
        "kind": "tool",
        "tool": "fetch_url",
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
    assert_eq!(
        body_str, r#"{"error":"Bullpen has no tool called fetch_url."}"#,
        "POST must refuse an unknown tool name with the exact TS-style message"
    );

    // Same gate on PATCH: start from a VALID tool routine (created through
    // the same route, tool "say"), then try to patch its tool to the same
    // unknown name.
    let valid_body = json!({
        "botId": "bot1",
        "name": "Get info 2",
        "prompt": "Summarize what you find",
        "schedule": "every 1 hour",
        "kind": "tool",
        "tool": "say",
        "toolArgs": "{}"
    });
    let valid_request = Request::builder()
        .method("POST")
        .uri("/api/routines")
        .header("cookie", &session_cookie)
        .header("content-type", "application/json")
        .body(Body::from(serde_json::to_string(&valid_body).unwrap()))
        .unwrap();
    let valid_response = app.clone().oneshot(valid_request).await.unwrap();
    assert_eq!(valid_response.status(), StatusCode::CREATED);
    let valid_bytes = axum::body::to_bytes(valid_response.into_body(), usize::MAX)
        .await
        .unwrap();
    let valid_json: Value = serde_json::from_slice(&valid_bytes).unwrap();
    let id = valid_json["routine"]["id"]
        .as_str()
        .expect("created routine has an id")
        .to_string();

    let patch_body = json!({ "kind": "tool", "tool": "fetch_url" });
    let patch_request = Request::builder()
        .method("PATCH")
        .uri(format!("/api/routines/{id}"))
        .header("cookie", &session_cookie)
        .header("content-type", "application/json")
        .body(Body::from(serde_json::to_string(&patch_body).unwrap()))
        .unwrap();
    let patch_response = app.clone().oneshot(patch_request).await.unwrap();
    assert_eq!(patch_response.status(), StatusCode::BAD_REQUEST);
    let patch_bytes = axum::body::to_bytes(patch_response.into_body(), usize::MAX)
        .await
        .unwrap();
    let patch_str = String::from_utf8(patch_bytes.to_vec()).unwrap();
    assert_eq!(
        patch_str, r#"{"error":"Bullpen has no tool called fetch_url."}"#,
        "PATCH must refuse the same way POST does"
    );
}

#[tokio::test]
async fn validate_empty_tool_args_defaults_to_empty_object() {
    let db = open_db();
    seed_bot(&db, "bot1", "Bot 1");
    let session_cookie = common::seed_session(&db);

    let port = ScriptedPort::new(vec![text_script("Result.")]);
    let app_state = AppState::with_port(db, Arc::new(port));
    let app = build_app(app_state);

    // Create a routine with kind "tool" and empty args - should succeed and
    // default to {}. F17: "say" (not "ask", never a toolbox name) - a 400
    // here would otherwise be the unknown-tool gate, not this test's claim.
    let body = json!({
        "botId": "bot1",
        "name": "Get info",
        "prompt": "Summarize what you find",
        "schedule": "every 1 hour",
        "kind": "tool",
        "tool": "say",
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

/// Polls `GET /api/routines/:id/runs` until every run row for this routine
/// has left "running", then returns the newest - the run itself finishes on
/// a task `start_routine`/`start_routine_narrowed` spawns, same pattern
/// `tests/routines_fire.rs::wait_for_run` uses.
///
/// F2: waits for EVERY row, not just the newest, because a "found
/// something" tool firing now writes TWO rows synchronously one after the
/// other - `record_tool_run`'s own (`done`/`failed` from the instant it is
/// inserted) and the phrasing run (`running` until its spawned task
/// completes). Both can land in the SAME millisecond (`now_iso`'s
/// resolution), so `ORDER BY created_at DESC` does not reliably put the
/// still-running phrasing row first - checking only the front row risked
/// returning the already-`done` tool-run row while the phrasing run (and
/// the model call a caller may still be about to assert on) had not
/// actually finished yet.
async fn wait_for_run(app: &axum::Router, session: &str, routine_id: &str) -> Value {
    for _ in 0..300 {
        let (_, body) = get_json(app, &format!("/api/routines/{routine_id}/runs"), session).await;
        if let Some(runs) = body.get("runs").and_then(|r| r.as_array())
            && !runs.is_empty()
            && runs
                .iter()
                .all(|r| r.get("status").and_then(|s| s.as_str()) != Some("running"))
        {
            return runs[0].clone();
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

    // F3: the phrasing run is narrowed to ALWAYS_ON only (`tools: []` in
    // TS, `routines.ts:908-910`) - it may say something, not run anything.
    // `say` (always-on) must still be offered; `shell`, `message_bot` and
    // `create_room` (none always-on) must not.
    let tool_names: Vec<&str> = requests[0]
        .tools
        .as_ref()
        .expect("the phrasing run must still offer a toolbox")
        .iter()
        .map(|spec| spec.name.as_str())
        .collect();
    assert!(
        tool_names.contains(&"say"),
        "an always-on tool must ride through the narrowing: {tool_names:?}"
    );
    for narrowed_out in ["shell", "message_bot", "create_room"] {
        assert!(
            !tool_names.contains(&narrowed_out),
            "{narrowed_out} must NOT be offered to a narrowed phrasing run: {tool_names:?}"
        );
    }
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

    // F17: this test used to also assert `active == Some(false)` here -
    // dead from the start (`reviews/S5b-R.md`, F17): `create_routine`
    // hardcodes `active = 0` and nothing on the run-now path writes that
    // column, so no mutation of `run_routine_now`/`fire_routine_tool`
    // could ever turn it red. Deleted rather than kept as a check that
    // cannot fail.
}

#[tokio::test]
async fn run_now_on_a_quiet_tool_routine_returns_201_not_could_not_start() {
    let db = open_db();
    let session = common::seed_session(&db);
    seed_bot(&db, "bot1", "Bot 1");

    // F9 (`reviews/S5b-R.md`): before this fix, `fire_routine_tool`
    // returning `None` on the quiet branch made `run_routine_now` map it
    // to the generic 400 "could not start" - `routes/routines.rs:465`
    // paints that red under the row in `routines_editor.rs:897-905`, so a
    // check that correctly found NOTHING wrong looked like a failure.
    let project = store::create_project(&db, "Ops").expect("create project");
    store::add_project_member(&db, &project.id, "bot1").expect("add project member");
    let id = store::create_routine(
        &db,
        "bot1",
        "Log to project",
        "File this under the project.",
        "every 1 hour".to_string(),
        None,
        None,
        Some("tool"),
        Some("project_remember"),
        Some(r#"{"project":"[[bullpen:nothing-new]]","fact":"irrelevant"}"#),
        None,
        None,
        None,
        None,
    )
    .expect("create tool routine");

    let scripted = Arc::new(ScriptedPort::new(vec![text_script(
        "Should never be called.",
    )]));
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
        "a correctly-quiet run-now must be 201, never the generic 400"
    );
    let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap();
    let json: Value = serde_json::from_slice(&bytes).unwrap();
    assert!(
        json.get("runId").and_then(|v| v.as_str()).is_some(),
        "response must carry a real runId: {json:?}"
    );

    assert!(
        scripted.requests().is_empty(),
        "no model call on a quiet run-now"
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

    // F17 (`reviews/S5b-R.md`): all three of this test's old assertions
    // were negative (`is_empty`) - replacing `fire_routine_tool`'s body
    // with `None`, hardening the permission gate to refuse everything, or
    // deleting the `[name] ran` append all stayed green, because "the
    // marker was honoured" and "tool routines do nothing" looked identical
    // from here. F2 also means a quiet firing is no longer invisible: it
    // records a REAL run (just no MODEL run), so `started` now gains one
    // entry and `GET .../runs` one row - asserted below as the POSITIVE
    // observable instead.
    let started = server::routines::fire_due(&app_state, now).await;
    assert_eq!(
        started.len(),
        1,
        "a quiet tool firing is still a real run that fired - only the \
         MODEL turn is skipped"
    );
    assert_eq!(started[0].0, id);

    let (_, body) = get_json(&app, &format!("/api/routines/{id}/runs"), &session).await;
    let runs = body
        .get("runs")
        .and_then(|r| r.as_array())
        .cloned()
        .unwrap_or_default();
    assert_eq!(
        runs.len(),
        1,
        "exactly one recorded run for the quiet tool firing: {runs:?}"
    );
    assert_eq!(
        runs[0].get("status").and_then(|s| s.as_str()),
        Some("done"),
        "a quiet find is OK, not a failure: {runs:?}"
    );
    let text = runs[0]
        .get("text")
        .and_then(|t| t.as_str())
        .expect("run has text");
    assert!(
        text.contains("[[bullpen:nothing-new]]"),
        "the recorded run's text must carry the tool's ACTUAL result: {text}"
    );

    let (_, list_body) = get_json(&app, "/api/routines?bot=bot1", &session).await;
    let row = list_body
        .get("routines")
        .and_then(|r| r.as_array())
        .and_then(|a| {
            a.iter()
                .find(|r| r.get("id").and_then(|v| v.as_str()) == Some(id.as_str()))
        })
        .expect("routine present in list");
    assert_eq!(
        row.get("failures").and_then(|v| v.as_i64()),
        Some(0),
        "a quiet tool run is a HEALTHY firing, not a failure: {row:?}"
    );

    assert!(
        scripted.requests().is_empty(),
        "the model must never be called when the tool found nothing"
    );
}

#[tokio::test]
async fn permission_gate_records_a_failed_run_with_no_toolbox_call() {
    let db = open_db();
    let session = common::seed_session(&db);
    seed_bot(&db, "bot1", "Bot 1");
    let now = chrono::Utc::now();

    // F17 (`reviews/S5b-R.md`): "mutating away the permission gate leaves
    // all 8 `tool_routines.rs` tests green - no test uses a tool whose
    // routine decision is `ask` or `deny`." `shell`'s DEFAULT decision
    // (`permissions.rs`'s `default_decisions`, S2-03) is `Ask`, not
    // `Allow` - exactly the case `fire_routine_tool`'s permission check
    // exists for: at 06:00 nobody is awake to answer an approval, so this
    // must fail closed rather than ever reach the sandbox.
    let id = seed_tool_routine(
        &db,
        "bot1",
        "Run a script",
        "Report what the script found.",
        "shell",
        r#"{"command":"echo hi"}"#,
        now,
    );

    let scripted = Arc::new(ScriptedPort::new(vec![text_script(
        "Should never be called.",
    )]));
    let port: Arc<dyn model::ModelPort> = scripted.clone();
    let app_state = AppState::with_port(db, port);
    let app = build_app(app_state.clone());

    let started = server::routines::fire_due(&app_state, now).await;
    assert_eq!(
        started.len(),
        1,
        "a refused tool firing is still a real run that fired"
    );

    let (_, body) = get_json(&app, &format!("/api/routines/{id}/runs"), &session).await;
    let runs = body
        .get("runs")
        .and_then(|r| r.as_array())
        .cloned()
        .unwrap_or_default();
    assert_eq!(runs.len(), 1, "exactly one recorded run: {runs:?}");
    assert_eq!(
        runs[0].get("status").and_then(|s| s.as_str()),
        Some("failed"),
        "a routine tool needing approval must FAIL, never silently skip: {runs:?}"
    );
    assert_eq!(
        runs[0].get("error").and_then(|e| e.as_str()),
        Some("This routine's tool needs approval, so it cannot run unattended."),
        "the TS approval-refusal text, verbatim: {runs:?}"
    );

    let (_, list_body) = get_json(&app, "/api/routines?bot=bot1", &session).await;
    let row = list_body
        .get("routines")
        .and_then(|r| r.as_array())
        .and_then(|a| {
            a.iter()
                .find(|r| r.get("id").and_then(|v| v.as_str()) == Some(id.as_str()))
        })
        .expect("routine present in list");
    assert_eq!(
        row.get("failures").and_then(|v| v.as_i64()),
        Some(1),
        "the streak must move on a permission refusal, or 3-strikes can never reach it: {row:?}"
    );

    assert!(
        scripted.requests().is_empty(),
        "no model call - the phrasing run never starts on a refused firing"
    );
}

#[tokio::test]
async fn unknown_tool_name_fails_at_fire_time_and_pauses_after_three_ticks() {
    let db = open_db();
    let session = common::seed_session(&db);
    seed_bot(&db, "bot1", "Bot 1");
    let start = chrono::Utc::now();

    // F1: a row with an unknown tool name predating (or bypassing, same as
    // here) the save-time gate - `seed_tool_routine` calls
    // `store::create_routine` directly, same as `POST /api/routines` did
    // before F1, so this is exactly the row shape the review's bite names.
    let id = seed_tool_routine(
        &db,
        "bot1",
        "Deploy watch",
        "Summarize what changed.",
        "fetch_url",
        "{}",
        start,
    );

    let scripted = Arc::new(ScriptedPort::new(vec![
        text_script("Should never be called."),
        text_script("Should never be called."),
        text_script("Should never be called."),
    ]));
    let port: Arc<dyn model::ModelPort> = scripted.clone();
    let app_state = AppState::with_port(db, port);
    let app = build_app(app_state.clone());

    // "every 1 hour" advances `next_run_at` by exactly one hour on every
    // reschedule, so firing at +0h, +1h, +2h finds the routine due each
    // time with no manual re-activation - same pattern
    // `tests/routines_fire.rs::three_failures_in_a_row_pause_the_routine_
    // with_the_reason` uses for a prompt-kind routine. `record_routine_run`
    // only PAUSES on the THIRD failure, not before.
    for n in 0..3i64 {
        let at = start + chrono::Duration::hours(n);
        let started = server::routines::fire_due(&app_state, at).await;
        assert_eq!(
            started.len(),
            1,
            "firing #{n}: an unknown-tool routine still records a real \
             (failed) run every tick, same as any other failure"
        );
    }

    let (_, body) = get_json(&app, &format!("/api/routines/{id}/runs"), &session).await;
    let runs = body
        .get("runs")
        .and_then(|r| r.as_array())
        .cloned()
        .unwrap_or_default();
    assert_eq!(runs.len(), 3, "one failed run per tick: {runs:?}");
    for run in &runs {
        assert_eq!(
            run.get("status").and_then(|s| s.as_str()),
            Some("failed"),
            "every tick's run must be FAILED, never a silent skip: {run:?}"
        );
    }

    let (_, list_body) = get_json(&app, "/api/routines?bot=bot1", &session).await;
    let row = list_body
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
        "three failures in a row must auto-pause: {row:?}"
    );
    assert_eq!(
        row.get("pausedReason").and_then(|v| v.as_str()),
        Some("Stopped after 3 failures in a row."),
        "exact TS pause text: {row:?}"
    );

    assert!(
        scripted.requests().is_empty(),
        "the model must never be called for a tool name Bullpen does not have"
    );
}
