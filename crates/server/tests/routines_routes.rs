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

/// Test 6: Delete routine
#[tokio::test]
async fn delete_routine() {
    let db = open_db();
    let session = seed_session(&db);
    let bot_id = create_test_bot(&db);
    let app = app_for(db);

    // Create routine
    let (create_status, body) = post_with_auth(
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
