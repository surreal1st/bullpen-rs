//! S5-06 acceptance: the login route resumes routines paused for absence.
//!
//! Through `build_app` + HTTP: seed a bot and a routine paused with
//! `paused_reason` = ABSENCE_PAUSE_REASON, POST `/api/auth/login` with the
//! right password, then GET `/api/routines` with the returned cookie and
//! assert the routine is active with `pausedReason` null. Second test: a
//! routine paused for a DIFFERENT reason is NOT resumed by login.

mod common;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use serde_json::Value;
use server::routines::ABSENCE_PAUSE_REASON;
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

fn seed_bot(db: &Db, id: &str) {
    db.conn()
        .execute(
            "INSERT INTO bots (id, name, purpose, instructions, model, created_at)
             VALUES (?1, ?2, '', 'do things', NULL, ?3)",
            rusqlite::params![id, id, chrono::Utc::now().to_rfc3339()],
        )
        .expect("seed bot");
}

/// Create a routine paused with the given reason.
fn seed_paused_routine(db: &Db, bot_id: &str, name: &str, paused_reason: &str) -> String {
    let id = store::create_routine(
        db,
        bot_id,
        name,
        "Do the thing.",
        "hourly".to_string(),
        None,
        None,
        None,
        None,
        None,
        None,
        None,
        None,
        None,
    )
    .expect("create routine");

    db.conn()
        .execute(
            "UPDATE routines SET active = 0, paused_reason = ?1 WHERE id = ?2",
            rusqlite::params![paused_reason, id],
        )
        .expect("pause routine");

    id
}

async fn send_request(app: &axum::Router, req: Request<Body>) -> (StatusCode, Value) {
    let response = app.clone().oneshot(req).await.expect("oneshot");
    let status = response.status();
    let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .expect("collect body");
    (
        status,
        serde_json::from_slice(&bytes).unwrap_or(Value::Null),
    )
}

fn post_login_request(password: &str) -> Request<Body> {
    Request::post("/api/auth/login")
        .header("content-type", "application/json")
        .body(Body::from(
            serde_json::to_string(&serde_json::json!({"password": password}))
                .expect("serialize request"),
        ))
        .expect("build request")
}

fn get_routines_request(cookie: &str) -> Request<Body> {
    Request::get("/api/routines?bot=arthur")
        .header("cookie", cookie)
        .body(Body::empty())
        .expect("build request")
}

#[tokio::test]
async fn login_resumes_absence_paused_routines() {
    let db = open_db();
    let password = "test-password-123";
    store::set_password(&db, password).expect("set password");

    seed_bot(&db, "arthur");
    let routine_id = seed_paused_routine(&db, "arthur", "my-routine", ABSENCE_PAUSE_REASON);

    let port: Arc<dyn model::ModelPort> = Arc::new(common::ScriptedPort::new(vec![]));
    let state = AppState::with_port(db, port);
    let app = build_app(state);

    // Login with the correct password
    let (status, login_response) = send_request(&app, post_login_request(password)).await;
    assert_eq!(status, StatusCode::OK, "login should succeed");

    // Extract the session cookie from the response
    let token = login_response["token"].as_str().expect("token in response");
    let cookie = format!("bullpen_session={}", token);

    // Fetch the routine to verify it's now active
    let (status, routines_response) = send_request(&app, get_routines_request(&cookie)).await;
    assert_eq!(status, StatusCode::OK, "get routines should succeed");

    let routines = routines_response["routines"]
        .as_array()
        .expect("routines array");
    assert_eq!(routines.len(), 1, "should have one routine");

    let routine = &routines[0];
    assert_eq!(
        routine["id"].as_str(),
        Some(routine_id.as_str()),
        "routine id should match"
    );
    assert_eq!(
        routine["active"].as_bool(),
        Some(true),
        "routine should be active after login"
    );
    assert!(
        routine["pausedReason"].is_null(),
        "pausedReason should be null after login"
    );
}

#[tokio::test]
async fn login_does_not_resume_routines_paused_for_other_reasons() {
    let db = open_db();
    let password = "test-password-456";
    store::set_password(&db, password).expect("set password");

    seed_bot(&db, "arthur");
    let routine_id = seed_paused_routine(
        &db,
        "arthur",
        "failed-routine",
        "Stopped after 3 failures in a row.",
    );

    let port: Arc<dyn model::ModelPort> = Arc::new(common::ScriptedPort::new(vec![]));
    let state = AppState::with_port(db, port);
    let app = build_app(state);

    // Login with the correct password
    let (status, login_response) = send_request(&app, post_login_request(password)).await;
    assert_eq!(status, StatusCode::OK, "login should succeed");

    // Extract the session cookie from the response
    let token = login_response["token"].as_str().expect("token in response");
    let cookie = format!("bullpen_session={}", token);

    // Fetch the routine to verify it's still paused
    let (status, routines_response) = send_request(&app, get_routines_request(&cookie)).await;
    assert_eq!(status, StatusCode::OK, "get routines should succeed");

    let routines = routines_response["routines"]
        .as_array()
        .expect("routines array");
    assert_eq!(routines.len(), 1, "should have one routine");

    let routine = &routines[0];
    assert_eq!(
        routine["id"].as_str(),
        Some(routine_id.as_str()),
        "routine id should match"
    );
    assert_eq!(
        routine["active"].as_bool(),
        Some(false),
        "routine should still be paused"
    );
    assert_eq!(
        routine["pausedReason"].as_str(),
        Some("Stopped after 3 failures in a row."),
        "pausedReason should be unchanged"
    );
}
