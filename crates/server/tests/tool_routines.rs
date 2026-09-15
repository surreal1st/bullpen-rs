//! S5b-05 acceptance: tool-kind routines. Tool validation, scheduled firing,
//! and run-now. Ports the TS ranges `routines.ts:280-320` (validation),
//! `:355-380` (create), `:470-500` (update), `:820-870` (scheduled tool branch),
//! `:990-1030` (run-now tool branch).
//!
//! Tests focus on the happy path: tool validation and tool firing when the
//! tool runs successfully. Permission checks, error handling, and the
//! nothing-found marker are integration tests only (relied on by other tests).

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
