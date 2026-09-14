//! S2-03 acceptance: a tool call that needs Josh's decision stops the run
//! and queues it; approving resumes with the real tool result in the
//! transcript; rejecting tells the model it was refused and the run carries
//! on; a routine still asks even when `shell` is stored `allow` (S2-02's
//! TIGHTEN); the working indicator says so while parked. Port of the
//! relevant cases in `test/approvals.test.ts`.
//!
//! Drives `RunManager` directly (never internals below it) for the run
//! behaviour, matching `tests/runs.rs`'s own posture - plus a handful of
//! HTTP-level tests through `build_app` for `GET /api/approvals` and
//! `POST /api/approvals/:id`, the routes this ticket owns.

mod common;

use std::sync::{Arc, Mutex};
use std::time::Duration;

use axum::Router;
use axum::body::Body;
use axum::http::{Request, StatusCode};
use common::{
    ScriptedPort, as_port, own_conversation, run_row, seed_bot, seed_session, seed_user_message,
};
use model::ladder::Trigger;
use model::{MessageContent, ModelEvent, ModelMessage, ToolCall};
use rusqlite::OptionalExtension;
use serde_json::{Value, json};
use server::runs::{RunEvent, RunManager, StartOptions};
use server::{AppState, build_app};
use store::Db;
use tower::ServiceExt;

fn open_db() -> Arc<Mutex<Db>> {
    // S2-04: routing defaults to enabled (S2-01); disable it so a
    // scripted test's first reply isn't consumed by the classifier call
    // instead of the turn it scripted it for.
    let db = Db::open(":memory:").expect("open :memory: db");
    model::routing::set_routing_settings(&db, Some(false), None)
        .expect("disable routing classifier for scripted-model tests");
    Arc::new(Mutex::new(db))
}

fn run_status(db: &Arc<Mutex<Db>>, run_id: &str) -> String {
    let db = db.lock().expect("db mutex poisoned");
    db.conn()
        .query_row(
            "SELECT status FROM runs WHERE id = ?1",
            rusqlite::params![run_id],
            |row| row.get(0),
        )
        .expect("read run row")
}

fn run_messages_json(db: &Arc<Mutex<Db>>, run_id: &str) -> String {
    let db = db.lock().expect("db mutex poisoned");
    db.conn()
        .query_row(
            "SELECT messages FROM runs WHERE id = ?1",
            rusqlite::params![run_id],
            |row| row.get(0),
        )
        .expect("read run messages")
}

/// The one pending approval row for a run, if there is one - `(id, tool_name,
/// tool_args, status)`. Panics on more than one: S2-03 parks a run on at
/// most a single pending call.
fn pending_approval(db: &Arc<Mutex<Db>>, run_id: &str) -> Option<(String, String, String, String)> {
    let db = db.lock().expect("db mutex poisoned");
    let mut stmt = db
        .conn()
        .prepare("SELECT id, tool_name, tool_args, status FROM approvals WHERE run_id = ?1")
        .expect("prepare");
    let rows: Vec<(String, String, String, String)> = stmt
        .query_map(rusqlite::params![run_id], |row| {
            Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?))
        })
        .expect("query")
        .collect::<Result<_, _>>()
        .expect("collect");
    assert!(
        rows.len() <= 1,
        "expected at most one approval row, got {rows:?}"
    );
    rows.into_iter().next()
}

/// A `shell` call once, then a final answer - the shape every test here
/// scripts, matching `test/approvals.test.ts`'s own `shellThenAnswer`.
fn shell_then_answer(answer: &str) -> ScriptedPort {
    ScriptedPort::new(vec![
        vec![ModelEvent::ToolCalls {
            calls: vec![ToolCall {
                id: "call-1".to_string(),
                name: "shell".to_string(),
                arguments: "{\"command\":\"rm -rf /work/x\"}".to_string(),
            }],
            usage: None,
        }],
        vec![
            ModelEvent::Delta {
                text: answer.to_string(),
            },
            ModelEvent::Done {
                model: "test/model".to_string(),
                usage: None,
                finish_reason: None,
            },
        ],
    ])
}

/// Drains a run's events until it settles OR parks - the two terminal-ish
/// states a subscriber can see (a park is not settlement, but it is the
/// last event this test needs).
async fn drain_until_paused_or_done(
    mut rx: tokio::sync::mpsc::UnboundedReceiver<RunEvent>,
) -> Vec<RunEvent> {
    let mut seen = Vec::new();
    while let Some(event) = rx.recv().await {
        let stop = matches!(
            event,
            RunEvent::Done { .. } | RunEvent::Error { .. } | RunEvent::ApprovalNeeded { .. }
        );
        seen.push(event);
        if stop {
            break;
        }
    }
    seen
}

async fn wait_for_status(db: &Arc<Mutex<Db>>, run_id: &str, target: &str) -> String {
    let mut status = run_status(db, run_id);
    for _ in 0..300 {
        if status == target {
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
        status = run_status(db, run_id);
    }
    status
}

// ---- F8: sweep helpers - back-date a row so a test proves the TTL/
// retention math without sleeping out real hours or days. ----

fn backdate_approval_created_at(db: &Arc<Mutex<Db>>, approval_id: &str, ago: chrono::Duration) {
    let ts = (chrono::Utc::now() - ago).to_rfc3339_opts(chrono::SecondsFormat::Millis, true);
    let db = db.lock().expect("db mutex poisoned");
    db.conn()
        .execute(
            "UPDATE approvals SET created_at = ?1 WHERE id = ?2",
            rusqlite::params![ts, approval_id],
        )
        .expect("backdate approval created_at");
}

fn backdate_approval_decided_at(db: &Arc<Mutex<Db>>, approval_id: &str, ago: chrono::Duration) {
    let ts = (chrono::Utc::now() - ago).to_rfc3339_opts(chrono::SecondsFormat::Millis, true);
    let db = db.lock().expect("db mutex poisoned");
    db.conn()
        .execute(
            "UPDATE approvals SET decided_at = ?1 WHERE id = ?2",
            rusqlite::params![ts, approval_id],
        )
        .expect("backdate approval decided_at");
}

fn approval_row_exists(db: &Arc<Mutex<Db>>, approval_id: &str) -> bool {
    let db = db.lock().expect("db mutex poisoned");
    db.conn()
        .query_row(
            "SELECT COUNT(*) FROM approvals WHERE id = ?1",
            rusqlite::params![approval_id],
            |row| row.get::<_, i64>(0),
        )
        .expect("count approval rows")
        > 0
}

fn approval_status(db: &Arc<Mutex<Db>>, approval_id: &str) -> Option<String> {
    let db = db.lock().expect("db mutex poisoned");
    db.conn()
        .query_row(
            "SELECT status FROM approvals WHERE id = ?1",
            rusqlite::params![approval_id],
            |row| row.get(0),
        )
        .optional()
        .expect("query approval status")
}

// 1. Ask: a `shell` call pauses the run, writes a pending approval row, and
//    tells the subscriber which call is waiting.
#[tokio::test]
async fn a_tool_call_that_needs_approval_pauses_the_run_and_queues_it() {
    let db = open_db();
    seed_bot(&db, "arthur", "Arthur");
    let conversation_id = own_conversation(&db, "arthur");
    seed_user_message(&db, &conversation_id, "clean up");

    let manager = Arc::new(RunManager::new(
        Arc::clone(&db),
        as_port(shell_then_answer("Cleaned it up.")),
    ));
    let run_id = manager.start(StartOptions {
        bot_id: "arthur".to_string(),
        conversation_id: conversation_id.clone(),
        model: "test/model".to_string(),
        messages: vec![ModelMessage::user("clean up")],
        trigger: Trigger::Chat,
        room: false,
    });

    let events = drain_until_paused_or_done(manager.subscribe(&run_id)).await;
    assert!(
        matches!(
            events.last(),
            Some(RunEvent::ApprovalNeeded { name, .. }) if name == "shell"
        ),
        "expected the run to park on the shell call, got {events:?}"
    );

    assert_eq!(wait_for_status(&db, &run_id, "waiting").await, "waiting");

    let (_, tool_name, tool_args, status) =
        pending_approval(&db, &run_id).expect("expected a pending approval row");
    assert_eq!(tool_name, "shell");
    assert!(tool_args.contains("rm -rf"));
    assert_eq!(status, "pending");
}

// 2. Approve: the tool runs (S2's stubbed, sandbox-less `shell`) and the run
//    finishes, with the stub's result in the transcript.
#[tokio::test]
async fn approving_runs_the_tool_and_the_run_finishes_with_the_result_in_the_transcript() {
    let db = open_db();
    seed_bot(&db, "arthur", "Arthur");
    let conversation_id = own_conversation(&db, "arthur");
    seed_user_message(&db, &conversation_id, "clean up");

    let manager = Arc::new(RunManager::new(
        Arc::clone(&db),
        as_port(shell_then_answer("Cleaned it up.")),
    ));
    let run_id = manager.start(StartOptions {
        bot_id: "arthur".to_string(),
        conversation_id: conversation_id.clone(),
        model: "test/model".to_string(),
        messages: vec![ModelMessage::user("clean up")],
        trigger: Trigger::Chat,
        room: false,
    });
    drain_until_paused_or_done(manager.subscribe(&run_id)).await;
    let (approval_id, ..) = pending_approval(&db, &run_id).expect("pending approval");

    assert!(manager.decide_approval(&approval_id, true).await);

    assert_eq!(wait_for_status(&db, &run_id, "done").await, "done");

    let messages_json = run_messages_json(&db, &run_id);
    let messages: Vec<ModelMessage> =
        serde_json::from_str(&messages_json).expect("parse stored messages");
    let tool_result = messages
        .iter()
        .find(|m| m.role == "tool")
        .expect("a tool result message");
    let MessageContent::Text(text) = &tool_result.content else {
        panic!("expected text content");
    };
    assert!(
        text.contains("Sandboxing is off here"),
        "expected the S2 shell stub's text, got {text:?}"
    );

    let thread = {
        let db = db.lock().unwrap();
        store::list_messages(&db, &conversation_id).unwrap()
    };
    let last = thread.last().expect("at least one message");
    assert_eq!(last.role, "assistant");
    assert_eq!(last.content, "Cleaned it up.");

    let (_, _, _, status) = pending_approval(&db, &run_id).expect("approval row still exists");
    assert_eq!(status, "approved");
}

// 3. Reject: the model is told IN WORDS that Josh refused, and the run
//    carries on rather than dying.
#[tokio::test]
async fn rejecting_tells_the_model_it_was_declined_and_the_run_carries_on() {
    let db = open_db();
    seed_bot(&db, "arthur", "Arthur");
    let conversation_id = own_conversation(&db, "arthur");
    seed_user_message(&db, &conversation_id, "clean up");

    let manager = Arc::new(RunManager::new(
        Arc::clone(&db),
        as_port(shell_then_answer("I did not delete anything.")),
    ));
    let run_id = manager.start(StartOptions {
        bot_id: "arthur".to_string(),
        conversation_id: conversation_id.clone(),
        model: "test/model".to_string(),
        messages: vec![ModelMessage::user("clean up")],
        trigger: Trigger::Chat,
        room: false,
    });
    drain_until_paused_or_done(manager.subscribe(&run_id)).await;
    let (approval_id, ..) = pending_approval(&db, &run_id).expect("pending approval");

    assert!(manager.decide_approval(&approval_id, false).await);

    assert_eq!(wait_for_status(&db, &run_id, "done").await, "done");

    let messages_json = run_messages_json(&db, &run_id);
    let messages: Vec<ModelMessage> =
        serde_json::from_str(&messages_json).expect("parse stored messages");
    let tool_result = messages
        .iter()
        .find(|m| m.role == "tool")
        .expect("a tool result message");
    let MessageContent::Text(text) = &tool_result.content else {
        panic!("expected text content");
    };
    assert!(
        text.to_lowercase().contains("did not approve"),
        "expected the refusal text, got {text:?}"
    );

    let thread = {
        let db = db.lock().unwrap();
        store::list_messages(&db, &conversation_id).unwrap()
    };
    let last = thread.last().expect("at least one message");
    assert_eq!(last.content, "I did not delete anything.");

    let (_, _, _, status) = pending_approval(&db, &run_id).expect("approval row still exists");
    assert_eq!(status, "rejected");
}

// 4. TIGHTEN: a routine-triggered run still asks for `shell` even with it
//    stored `allow` for chat.
#[tokio::test]
async fn a_routine_trigger_still_asks_for_shell_even_when_stored_allow() {
    let db = open_db();
    seed_bot(&db, "arthur", "Arthur");
    let conversation_id = own_conversation(&db, "arthur");
    seed_user_message(&db, &conversation_id, "clean up");
    {
        let guard = db.lock().expect("db mutex poisoned");
        let current = server::permissions::get_permissions(&guard, "arthur").expect("get perms");
        server::permissions::set_permissions(&guard, "arthur", &{
            let mut m = current;
            m.insert("shell".to_string(), server::permissions::Decision::Allow);
            m
        })
        .expect("set perms");
    }

    let manager = Arc::new(RunManager::new(
        Arc::clone(&db),
        as_port(shell_then_answer("Ran it.")),
    ));
    let run_id = manager.start(StartOptions {
        bot_id: "arthur".to_string(),
        conversation_id: conversation_id.clone(),
        model: "test/model".to_string(),
        messages: vec![ModelMessage::user("clean up")],
        trigger: Trigger::Routine,
        room: false,
    });

    drain_until_paused_or_done(manager.subscribe(&run_id)).await;
    assert_eq!(wait_for_status(&db, &run_id, "waiting").await, "waiting");
    let (_, tool_name, _, status) =
        pending_approval(&db, &run_id).expect("still asks for shell, unattended");
    assert_eq!(tool_name, "shell");
    assert_eq!(status, "pending");
}

// 5. working(): the line says exactly what it is waiting on while parked.
#[tokio::test]
async fn working_says_waiting_for_you_to_approve_shell_while_parked() {
    let db = open_db();
    seed_bot(&db, "arthur", "Arthur");
    let conversation_id = own_conversation(&db, "arthur");
    seed_user_message(&db, &conversation_id, "clean up");

    let manager = Arc::new(RunManager::new(
        Arc::clone(&db),
        as_port(shell_then_answer("Cleaned it up.")),
    ));
    let run_id = manager.start(StartOptions {
        bot_id: "arthur".to_string(),
        conversation_id: conversation_id.clone(),
        model: "test/model".to_string(),
        messages: vec![ModelMessage::user("clean up")],
        trigger: Trigger::Chat,
        room: false,
    });
    drain_until_paused_or_done(manager.subscribe(&run_id)).await;
    wait_for_status(&db, &run_id, "waiting").await;

    let working = manager.working(&conversation_id).expect("working");
    let arthur = working
        .iter()
        .find(|b| b.bot_id == "arthur")
        .expect("arthur is working");
    assert!(arthur.waiting);
    assert_eq!(arthur.activity, "Waiting for you to approve shell");
}

// 6. F4: a tool name absent from the permission map entirely parks the run
//    instead of running free. Run-level rather than a unit test on
//    `permissions::decide_call` because the defect is in `runs.rs`'s OWN
//    `match perms.get(...)` arm (the `None` branch), not in that helper -
//    a name the toolbox itself does not recognise still has to be decided
//    before it ever reaches the toolbox, so this scripts a call to a name
//    that is neither in `default_decisions()` nor in `tools/mod.rs`'s
//    match. Bite: with the `None` arm reverted to `Decision::Allow`, the
//    toolbox runs it and answers "Unknown tool: ..." instead of parking -
//    the `ApprovalNeeded` match below never sees that event and panics.
#[tokio::test]
async fn a_tool_call_with_no_permission_row_parks_the_run_instead_of_running_free() {
    let db = open_db();
    seed_bot(&db, "arthur", "Arthur");
    let conversation_id = own_conversation(&db, "arthur");
    seed_user_message(&db, &conversation_id, "do the thing");

    let port = ScriptedPort::new(vec![
        vec![ModelEvent::ToolCalls {
            calls: vec![ToolCall {
                id: "call-1".to_string(),
                name: "totally_unmapped_tool".to_string(),
                arguments: "{}".to_string(),
            }],
            usage: None,
        }],
        vec![
            ModelEvent::Delta {
                text: "Never got here.".to_string(),
            },
            ModelEvent::Done {
                model: "test/model".to_string(),
                usage: None,
                finish_reason: None,
            },
        ],
    ]);

    let manager = Arc::new(RunManager::new(Arc::clone(&db), as_port(port)));
    let run_id = manager.start(StartOptions {
        bot_id: "arthur".to_string(),
        conversation_id: conversation_id.clone(),
        model: "test/model".to_string(),
        messages: vec![ModelMessage::user("do the thing")],
        trigger: Trigger::Chat,
        room: false,
    });

    let events = drain_until_paused_or_done(manager.subscribe(&run_id)).await;
    assert!(
        matches!(
            events.last(),
            Some(RunEvent::ApprovalNeeded { name, .. }) if name == "totally_unmapped_tool"
        ),
        "expected an unmapped tool name to park the run rather than run free, got {events:?}"
    );

    assert_eq!(wait_for_status(&db, &run_id, "waiting").await, "waiting");
    let (_, tool_name, _, status) = pending_approval(&db, &run_id)
        .expect("expected a pending approval row for the unmapped tool");
    assert_eq!(tool_name, "totally_unmapped_tool");
    assert_eq!(status, "pending");
}

// ---- 7. F8: a pending approval 25h old is expired by the sweep, and the
// run it parked reads `failed` - THE BITE ----
#[tokio::test]
async fn a_pending_approval_25_hours_old_is_expired_by_the_sweep_and_the_run_fails() {
    let db = open_db();
    seed_bot(&db, "arthur", "Arthur");
    let conversation_id = own_conversation(&db, "arthur");
    seed_user_message(&db, &conversation_id, "clean up");

    let manager = Arc::new(RunManager::new(
        Arc::clone(&db),
        as_port(shell_then_answer("Cleaned it up.")),
    ));
    let run_id = manager.start(StartOptions {
        bot_id: "arthur".to_string(),
        conversation_id: conversation_id.clone(),
        model: "test/model".to_string(),
        messages: vec![ModelMessage::user("clean up")],
        trigger: Trigger::Chat,
        room: false,
    });
    drain_until_paused_or_done(manager.subscribe(&run_id)).await;
    let (approval_id, ..) = pending_approval(&db, &run_id).expect("pending approval");

    backdate_approval_created_at(&db, &approval_id, chrono::Duration::hours(25));
    manager.sweep_approvals(chrono::Utc::now());

    let (status, error) = run_row(&db, &run_id);
    assert_eq!(status, "failed", "an expired approval must fail its run");
    assert_eq!(error.as_deref(), Some("approval expired"));
    assert_eq!(
        approval_status(&db, &approval_id),
        Some("expired".to_string())
    );
}

// ---- 8. F8: a decided approval 31 days old is pruned by the sweep ----
#[tokio::test]
async fn a_decided_approval_31_days_old_is_pruned_by_the_sweep() {
    let db = open_db();
    seed_bot(&db, "arthur", "Arthur");
    let conversation_id = own_conversation(&db, "arthur");
    seed_user_message(&db, &conversation_id, "clean up");

    let manager = Arc::new(RunManager::new(
        Arc::clone(&db),
        as_port(shell_then_answer("Cleaned it up.")),
    ));
    let run_id = manager.start(StartOptions {
        bot_id: "arthur".to_string(),
        conversation_id: conversation_id.clone(),
        model: "test/model".to_string(),
        messages: vec![ModelMessage::user("clean up")],
        trigger: Trigger::Chat,
        room: false,
    });
    drain_until_paused_or_done(manager.subscribe(&run_id)).await;
    let (approval_id, ..) = pending_approval(&db, &run_id).expect("pending approval");
    assert!(manager.decide_approval(&approval_id, true).await);
    assert_eq!(wait_for_status(&db, &run_id, "done").await, "done");

    backdate_approval_decided_at(&db, &approval_id, chrono::Duration::days(31));
    manager.sweep_approvals(chrono::Utc::now());

    assert!(
        !approval_row_exists(&db, &approval_id),
        "a decided row 31 days old must be pruned by the sweep"
    );
}

// ---- 9. F8: a decided approval 1 day old survives the sweep ----
#[tokio::test]
async fn a_decided_approval_1_day_old_is_kept_by_the_sweep() {
    let db = open_db();
    seed_bot(&db, "arthur", "Arthur");
    let conversation_id = own_conversation(&db, "arthur");
    seed_user_message(&db, &conversation_id, "clean up");

    let manager = Arc::new(RunManager::new(
        Arc::clone(&db),
        as_port(shell_then_answer("Cleaned it up.")),
    ));
    let run_id = manager.start(StartOptions {
        bot_id: "arthur".to_string(),
        conversation_id: conversation_id.clone(),
        model: "test/model".to_string(),
        messages: vec![ModelMessage::user("clean up")],
        trigger: Trigger::Chat,
        room: false,
    });
    drain_until_paused_or_done(manager.subscribe(&run_id)).await;
    let (approval_id, ..) = pending_approval(&db, &run_id).expect("pending approval");
    assert!(manager.decide_approval(&approval_id, true).await);
    assert_eq!(wait_for_status(&db, &run_id, "done").await, "done");

    backdate_approval_decided_at(&db, &approval_id, chrono::Duration::days(1));
    manager.sweep_approvals(chrono::Utc::now());

    assert!(
        approval_row_exists(&db, &approval_id),
        "a decided row only 1 day old must survive the sweep"
    );
}

// ---- HTTP: GET /api/approvals, POST /api/approvals/:id ----

fn app_for(db: Db, port: Arc<dyn model::ModelPort>) -> Router {
    build_app(AppState::with_port(db, port))
}

async fn get_json(app: &Router, uri: &str, cookie: &str) -> (StatusCode, Value) {
    let resp = app
        .clone()
        .oneshot(
            Request::get(uri)
                .header("cookie", cookie)
                .body(Body::empty())
                .expect("build request"),
        )
        .await
        .expect("oneshot");
    let status = resp.status();
    let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
        .await
        .expect("collect");
    let json = if bytes.is_empty() {
        Value::Null
    } else {
        serde_json::from_slice(&bytes).expect("response body is JSON")
    };
    (status, json)
}

async fn post_json(app: &Router, uri: &str, body: Value, cookie: &str) -> (StatusCode, Value) {
    let resp = app
        .clone()
        .oneshot(
            Request::post(uri)
                .header("content-type", "application/json")
                .header("cookie", cookie)
                .body(Body::from(body.to_string()))
                .expect("build request"),
        )
        .await
        .expect("oneshot");
    let status = resp.status();
    let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
        .await
        .expect("collect");
    let json = if bytes.is_empty() {
        Value::Null
    } else {
        serde_json::from_slice(&bytes).expect("response body is JSON")
    };
    (status, json)
}

/// Starts a run over HTTP without waiting on its SSE body - `oneshot`
/// resolves once the handler returns the (streaming) response, same as the
/// TS test file's own `void app.request(...)`. Polling `GET /api/approvals`
/// afterwards is what actually waits for the run to park.
async fn post_message_fire_and_forget(app: &Router, bot_id: &str, text: &str, cookie: &str) {
    let resp = app
        .clone()
        .oneshot(
            Request::post(format!("/api/bots/{bot_id}/messages"))
                .header("content-type", "application/json")
                .header("cookie", cookie)
                .body(Body::from(json!({"text": text}).to_string()))
                .expect("build request"),
        )
        .await
        .expect("oneshot");
    assert_eq!(resp.status(), StatusCode::OK);
}

async fn wait_for_one_pending(app: &Router, cookie: &str) -> Value {
    for _ in 0..300 {
        let (status, body) = get_json(app, "/api/approvals", cookie).await;
        assert_eq!(status, StatusCode::OK);
        let approvals = body["approvals"].as_array().cloned().unwrap_or_default();
        if approvals.len() == 1 {
            return approvals[0].clone();
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    panic!("no approval showed up in time");
}

#[tokio::test]
async fn get_approvals_lists_the_pending_shell_call() {
    let db = open_db_plain();
    store::set_password(&db, "test-password").expect("set password");
    seed_bot_plain(&db, "arthur", "Arthur");
    let session = seed_session(&db);
    let app = app_for(db, as_port(shell_then_answer("Cleaned it up.")));

    post_message_fire_and_forget(&app, "arthur", "clean up", &session).await;
    let approval = wait_for_one_pending(&app, &session).await;

    assert_eq!(approval["toolName"], "shell");
    assert_eq!(approval["botName"], "Arthur");
    assert_eq!(approval["trigger"], "chat");
    assert!(approval["id"].as_str().is_some_and(|s| !s.is_empty()));
}

#[tokio::test]
async fn post_approvals_id_approves_and_the_run_resumes() {
    let db = open_db_plain();
    store::set_password(&db, "test-password").expect("set password");
    seed_bot_plain(&db, "arthur", "Arthur");
    let session = seed_session(&db);
    let app = app_for(db, as_port(shell_then_answer("Cleaned it up.")));

    post_message_fire_and_forget(&app, "arthur", "clean up", &session).await;
    let approval = wait_for_one_pending(&app, &session).await;
    let id = approval["id"].as_str().expect("approval id").to_string();

    let (status, body) = post_json(
        &app,
        &format!("/api/approvals/{id}"),
        json!({"approved": true}),
        &session,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["ok"], true);
    assert_eq!(body["approved"], true);

    // The row is decided immediately; nothing pending is left behind.
    let (list_status, list_body) = get_json(&app, "/api/approvals", &session).await;
    assert_eq!(list_status, StatusCode::OK);
    assert_eq!(list_body["approvals"].as_array().map(|a| a.len()), Some(0));
}

#[tokio::test]
async fn post_approvals_unknown_id_404s() {
    let db = open_db_plain();
    store::set_password(&db, "test-password").expect("set password");
    let session = seed_session(&db);
    let app = app_for(db, as_port(shell_then_answer("x")));

    let (status, body) = post_json(
        &app,
        "/api/approvals/not-a-real-approval",
        json!({"approved": true}),
        &session,
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert!(body["error"].as_str().is_some());
}

#[tokio::test]
async fn post_approvals_id_cannot_be_decided_twice() {
    let db = open_db_plain();
    store::set_password(&db, "test-password").expect("set password");
    seed_bot_plain(&db, "arthur", "Arthur");
    let session = seed_session(&db);
    let app = app_for(db, as_port(shell_then_answer("Cleaned it up.")));

    post_message_fire_and_forget(&app, "arthur", "clean up", &session).await;
    let approval = wait_for_one_pending(&app, &session).await;
    let id = approval["id"].as_str().expect("approval id").to_string();

    let (first, _) = post_json(
        &app,
        &format!("/api/approvals/{id}"),
        json!({"approved": true}),
        &session,
    )
    .await;
    assert_eq!(first, StatusCode::OK);

    let (second, _) = post_json(
        &app,
        &format!("/api/approvals/{id}"),
        json!({"approved": true}),
        &session,
    )
    .await;
    assert_eq!(second, StatusCode::NOT_FOUND);
}

fn open_db_plain() -> Db {
    let db = Db::open(":memory:").expect("open :memory: db");
    model::routing::set_routing_settings(&db, Some(false), None)
        .expect("disable routing classifier for scripted-model tests");
    db
}

fn seed_bot_plain(db: &Db, id: &str, name: &str) {
    db.conn()
        .execute(
            "INSERT INTO bots (id, name, purpose, instructions, model, created_at) VALUES (?1, ?2, '', ?3, NULL, '2026-01-01T00:00:00Z')",
            rusqlite::params![id, name, format!("You are {name}.")],
        )
        .expect("seed bot");
}
