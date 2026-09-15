//! S5-04: Routines routes tests.

use axum::body::Body;
use axum::http::{Request, StatusCode};
use rusqlite::params;
use serde_json::{Value, json};
use server::AppState;
use store::Db;
use tower::ServiceExt;
use uuid::Uuid;

mod common;
use common::seed_session;

fn open_db() -> Db {
    Db::open(":memory:").expect("open :memory: db")
}

fn create_test_bot(db: &Db) -> String {
    let bot_id = Uuid::new_v4().to_string();
    let now = chrono::Utc::now().to_rfc3339();
    db.conn()
        .execute(
            "INSERT INTO bots (id, name, purpose, instructions, created_at) VALUES (?1, ?2, ?3, ?4, ?5)",
            params![&bot_id, "Test Bot", "Testing", "Do things", &now],
        )
        .expect("create test bot");
    bot_id
}

fn app_for(db: Db) -> axum::Router {
    let state = AppState::new(db);
    server::build_app(state)
}

async fn get_with_auth(app: &axum::Router, path: &str, session: &str) -> (StatusCode, Value) {
    let request = Request::get(path)
        .header("cookie", session)
        .body(Body::empty())
        .unwrap();

    let response = app.clone().oneshot(request).await.unwrap();
    let status = response.status();
    let body_bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap();
    let body_json = serde_json::from_slice(&body_bytes).unwrap_or(Value::Null);

    (status, body_json)
}

async fn post_with_auth(
    app: &axum::Router,
    path: &str,
    session: &str,
    body: Value,
) -> (StatusCode, Value) {
    let body_str = body.to_string();
    let request = Request::post(path)
        .header("cookie", session)
        .header("content-type", "application/json")
        .body(Body::from(body_str))
        .unwrap();

    let response = app.clone().oneshot(request).await.unwrap();
    let status = response.status();
    let body_bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap();
    let body_json = serde_json::from_slice(&body_bytes).unwrap_or(Value::Null);

    (status, body_json)
}

async fn patch_with_auth(
    app: &axum::Router,
    path: &str,
    session: &str,
    body: Value,
) -> (StatusCode, Value) {
    let body_str = body.to_string();
    let request = Request::patch(path)
        .header("cookie", session)
        .header("content-type", "application/json")
        .body(Body::from(body_str))
        .unwrap();

    let response = app.clone().oneshot(request).await.unwrap();
    let status = response.status();
    let body_bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap();
    let body_json = serde_json::from_slice(&body_bytes).unwrap_or(Value::Null);

    (status, body_json)
}

async fn delete_with_auth(app: &axum::Router, path: &str, session: &str) -> (StatusCode, Value) {
    let request = Request::delete(path)
        .header("cookie", session)
        .body(Body::empty())
        .unwrap();

    let response = app.clone().oneshot(request).await.unwrap();
    let status = response.status();
    let body_bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap();
    let body_json = serde_json::from_slice(&body_bytes).unwrap_or(Value::Null);

    (status, body_json)
}

async fn get_no_auth(app: &axum::Router, path: &str) -> StatusCode {
    let request = Request::get(path).body(Body::empty()).unwrap();
    let response = app.clone().oneshot(request).await.unwrap();
    response.status()
}

/// Test 1: 401 without a session
#[tokio::test]
async fn list_routines_requires_auth() {
    let db = open_db();
    // Set password so auth checks work (without a password, the server returns 503)
    store::set_password(&db, "test-password").expect("set test password");
    let app = app_for(db);

    let status = get_no_auth(&app, "/api/routines").await;
    assert_eq!(status, StatusCode::UNAUTHORIZED, "should require auth");
}

/// Test 2: Create routine and list shows it with correct fields
#[tokio::test]
async fn create_routine_and_list() {
    let db = open_db();
    let session = seed_session(&db);
    let bot_id = create_test_bot(&db);
    let app = app_for(db);

    // Create routine
    let (status, body) = post_with_auth(
        &app,
        "/api/routines",
        &session,
        json!({
            "botId": &bot_id,
            "name": "Test Routine",
            "prompt": "Do something",
            "schedule": "every 15 minutes",
        }),
    )
    .await;

    assert_eq!(status, StatusCode::CREATED, "should create routine");
    let routine_id = body
        .get("routine")
        .and_then(|r| r.get("id"))
        .and_then(|v| v.as_str());
    assert!(routine_id.is_some(), "response should have routine.id");

    let routine_id = routine_id.unwrap();
    assert_eq!(
        body.get("routine")
            .and_then(|r| r.get("name"))
            .and_then(|v| v.as_str()),
        Some("Test Routine"),
        "routine name matches"
    );

    // Verify it appears in list
    let (status, body) = get_with_auth(&app, "/api/routines", &session).await;
    assert_eq!(status, StatusCode::OK);
    let routines = body.get("routines").and_then(|v| v.as_array());
    assert!(routines.is_some(), "should have routines array");
    let routines = routines.unwrap();
    assert_eq!(routines.len(), 1, "should have one routine");
    assert_eq!(
        routines[0].get("id").and_then(|v| v.as_str()),
        Some(routine_id),
        "routine id matches"
    );

    // Check that nextRunAt was computed
    assert!(
        routines[0].get("nextRunAt").is_some(),
        "nextRunAt should be set"
    );
}

/// Test 3: Bad schedule returns 400 with error text
#[tokio::test]
async fn bad_schedule_returns_400() {
    let db = open_db();
    let session = seed_session(&db);
    let bot_id = create_test_bot(&db);
    let app = app_for(db);

    let (status, body) = post_with_auth(
        &app,
        "/api/routines",
        &session,
        json!({
            "botId": &bot_id,
            "name": "Bad Schedule",
            "prompt": "Do something",
            "schedule": "every 0 minutes",
        }),
    )
    .await;

    assert_eq!(
        status,
        StatusCode::BAD_REQUEST,
        "should reject bad schedule"
    );
    assert!(body.get("error").is_some(), "should have error message");
}

/// Test 4: PATCH updates schedule and recomputes nextRunAt
#[tokio::test]
async fn patch_schedule_updates_next_run_at() {
    let db = open_db();
    let session = seed_session(&db);
    let bot_id = create_test_bot(&db);
    let app = app_for(db);

    // Create routine with 15-minute interval
    let (status, body) = post_with_auth(
        &app,
        "/api/routines",
        &session,
        json!({
            "botId": &bot_id,
            "name": "Test",
            "prompt": "Do something",
            "schedule": "every 15 minutes",
        }),
    )
    .await;

    assert_eq!(status, StatusCode::CREATED);
    let routine_id = body
        .get("routine")
        .and_then(|r| r.get("id"))
        .and_then(|v| v.as_str())
        .unwrap();
    let old_next_run = body
        .get("routine")
        .and_then(|r| r.get("nextRunAt"))
        .and_then(|v| v.as_str());

    // Update schedule
    let (status, body) = patch_with_auth(
        &app,
        &format!("/api/routines/{}", routine_id),
        &session,
        json!({
            "schedule": "every 30 minutes",
        }),
    )
    .await;

    assert_eq!(status, StatusCode::OK, "should patch routine");
    let new_next_run = body
        .get("routine")
        .and_then(|r| r.get("nextRunAt"))
        .and_then(|v| v.as_str());

    // nextRunAt should have changed
    assert_ne!(
        old_next_run, new_next_run,
        "nextRunAt should be different after schedule change"
    );
}

/// Test 5: Active toggle
#[tokio::test]
async fn toggle_routine_active() {
    let db = open_db();
    let session = seed_session(&db);
    let bot_id = create_test_bot(&db);
    let app = app_for(db);

    // Create routine (starts inactive)
    let (status, body) = post_with_auth(
        &app,
        "/api/routines",
        &session,
        json!({
            "botId": &bot_id,
            "name": "Test",
            "prompt": "Do something",
            "schedule": "hourly",
        }),
    )
    .await;

    assert_eq!(status, StatusCode::CREATED);
    let routine_id = body
        .get("routine")
        .and_then(|r| r.get("id"))
        .and_then(|v| v.as_str())
        .unwrap();

    // Activate it
    let (status, body) = post_with_auth(
        &app,
        &format!("/api/routines/{}/active", routine_id),
        &session,
        json!({"active": true}),
    )
    .await;

    assert_eq!(status, StatusCode::OK);
    assert_eq!(
        body.get("routine")
            .and_then(|r| r.get("active"))
            .and_then(|v| v.as_bool()),
        Some(true),
        "should be active"
    );

    // Deactivate it
    let (status, body) = post_with_auth(
        &app,
        &format!("/api/routines/{}/active", routine_id),
        &session,
        json!({"active": false}),
    )
    .await;

    assert_eq!(status, StatusCode::OK);
    assert_eq!(
        body.get("routine")
            .and_then(|r| r.get("active"))
            .and_then(|v| v.as_bool()),
        Some(false),
        "should be inactive"
    );
}

/// S5-F-01/F2: pressing "Start" used to write `next_run_at = NULL`
/// unconditionally (`set_routine_active(&db, &id, active, None)`), and
/// `due_routines`/`fire_due` both require `next_run_at IS NOT NULL` - a
/// routine started this way could never fire again. This is the bug's own
/// end-to-end proof: Start must hand back a non-null, FUTURE `nextRunAt`,
/// and that instant must actually be the one `fire_due` finds due.
///
/// Uses `AppState::with_port`/`ScriptedPort` rather than `app_for`'s real
/// `AppState::new` (which would reach for a real `OpenRouterPort`) because
/// `fire_due` here really does start a run - same posture as `tests/
/// routines_fire.rs`. Routing and the judge are explicitly disabled on this
/// db, per the ticket header rule, even though `open_db` (shared with every
/// other test in this file) does not do that itself.
#[tokio::test]
async fn starting_a_routine_gives_it_a_non_null_next_run_at_that_fires() {
    let db = open_db();
    model::routing::set_routing_settings(&db, Some(false), None)
        .expect("disable routing classifier");
    server::judge::set_judge_enabled(&db, false).expect("disable judge");
    let session = seed_session(&db);
    let bot_id = create_test_bot(&db);

    let scripted =
        std::sync::Arc::new(common::ScriptedPort::new(vec![common::text_script("done")]));
    let port: std::sync::Arc<dyn model::ModelPort> = scripted.clone();
    let state = AppState::with_port(db, port);
    let app = server::build_app(state.clone());

    let (status, body) = post_with_auth(
        &app,
        "/api/routines",
        &session,
        json!({
            "botId": &bot_id,
            "name": "Test",
            "prompt": "Do something",
            "schedule": "every 15 minutes",
        }),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED);
    let routine_id = body
        .get("routine")
        .and_then(|r| r.get("id"))
        .and_then(|v| v.as_str())
        .unwrap()
        .to_string();

    let (status, body) = post_with_auth(
        &app,
        &format!("/api/routines/{}/active", routine_id),
        &session,
        json!({"active": true}),
    )
    .await;
    assert_eq!(status, StatusCode::OK);

    let next_run_at = body
        .get("routine")
        .and_then(|r| r.get("nextRunAt"))
        .and_then(|v| v.as_str())
        .expect("F2: Start must set a non-null nextRunAt")
        .to_string();
    let next_run_at: chrono::DateTime<chrono::Utc> =
        next_run_at.parse().expect("nextRunAt is a valid timestamp");
    assert!(
        next_run_at > chrono::Utc::now(),
        "nextRunAt should be in the future right after Start"
    );

    // Prove it, don't just read it: fire at the computed instant and
    // confirm THIS routine is what came due.
    let started =
        server::routines::fire_due(&state, next_run_at + chrono::Duration::seconds(1)).await;
    assert_eq!(
        started.len(),
        1,
        "the routine started via POST /:id/active must fire on the next tick"
    );
    assert_eq!(started[0].0, routine_id);
}

/// Test 6: Delete routine
#[tokio::test]
async fn delete_routine() {
    let db = open_db();
    let session = seed_session(&db);
    let bot_id = create_test_bot(&db);
    let app = app_for(db);

    // Create routine
    let (_create_status, body) = post_with_auth(
        &app,
        "/api/routines",
        &session,
        json!({
            "botId": &bot_id,
            "name": "Test",
            "prompt": "Do something",
            "schedule": "hourly",
        }),
    )
    .await;

    let routine_id = body
        .get("routine")
        .and_then(|r| r.get("id"))
        .and_then(|v| v.as_str())
        .unwrap();

    // Delete it
    let (delete_status, _body) =
        delete_with_auth(&app, &format!("/api/routines/{}", routine_id), &session).await;

    assert_eq!(delete_status, StatusCode::OK, "should delete routine");

    // Verify it's gone
    let (list_status, body) = get_with_auth(&app, "/api/routines", &session).await;
    assert_eq!(list_status, StatusCode::OK);
    let empty_vec = vec![];
    let routines = body
        .get("routines")
        .and_then(|v| v.as_array())
        .unwrap_or(&empty_vec);
    assert!(routines.is_empty(), "routine should be deleted");
}

/// Test 7: Get routine runs (capped at 20)
#[tokio::test]
async fn get_routine_runs_limited() {
    let db = open_db();
    let session = seed_session(&db);
    let bot_id = create_test_bot(&db);
    let app = app_for(db);

    // Create routine
    let (_status, body) = post_with_auth(
        &app,
        "/api/routines",
        &session,
        json!({
            "botId": &bot_id,
            "name": "Test",
            "prompt": "Do something",
            "schedule": "hourly",
        }),
    )
    .await;

    let routine_id = body
        .get("routine")
        .and_then(|r| r.get("id"))
        .and_then(|v| v.as_str())
        .unwrap();

    // Get runs (should be empty for new routine)
    let (_status, body) = get_with_auth(
        &app,
        &format!("/api/routines/{}/runs", routine_id),
        &session,
    )
    .await;

    let empty_vec = vec![];
    let runs = body
        .get("runs")
        .and_then(|v| v.as_array())
        .unwrap_or(&empty_vec);
    assert_eq!(runs.len(), 0, "new routine should have no runs");
}
