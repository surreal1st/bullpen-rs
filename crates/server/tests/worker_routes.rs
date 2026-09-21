//! S9-05: workers HTTP + bot worker assignment through `build_app`.

mod common;

use axum::Router;
use axum::body::Body;
use axum::http::{Request, StatusCode};
use serde_json::{Value, json};
use server::{AppState, build_app};
use store::Db;
use tower::ServiceExt;

use common::seed_session;

fn open_db() -> Db {
    Db::open(":memory:").expect("open db")
}

fn app(db: Db) -> Router {
    build_app(AppState::new(db))
}

fn seed_bot(db: &Db, id: &str) {
    db.conn()
        .execute(
            "INSERT INTO bots (id, name, purpose, instructions, model, created_at, permissions) VALUES (?1, ?2, '', 'i', NULL, '2026-01-01T00:00:00Z', ?3)",
            rusqlite::params![id, id, "{}"],
        )
        .expect("seed bot");
}

async fn json_request(
    app: &Router,
    method: &str,
    path: &str,
    session: &str,
    body: Option<Value>,
) -> (u16, Value) {
    let mut builder = Request::builder().method(method).uri(path);
    if !session.is_empty() {
        builder = builder.header("cookie", session);
    }
    if body.is_some() {
        builder = builder.header("content-type", "application/json");
    }
    let request = builder
        .body(Body::from(body.map(|b| b.to_string()).unwrap_or_default()))
        .unwrap();
    let response = app.clone().oneshot(request).await.unwrap();
    let status = response.status().as_u16();
    let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap();
    let value: Value = serde_json::from_slice(&bytes).unwrap_or(json!({}));
    (status, value)
}

#[tokio::test]
async fn workers_crud_and_bot_assignment() {
    let db = open_db();
    seed_bot(&db, "arthur");
    let session = seed_session(&db);
    let app = app(db);

    let (status, body) = json_request(&app, "GET", "/api/workers", &session, None).await;
    assert_eq!(status, StatusCode::OK.as_u16());
    assert_eq!(body["workers"].as_array().unwrap().len(), 0);

    let (status, body) = json_request(
        &app,
        "POST",
        "/api/workers",
        &session,
        Some(json!({
            "label": "Workstation",
            "kind": "ssh",
            "sshUser": "josh",
            "sshHost": "workstation.example"
        })),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED.as_u16());
    let worker_id = body["worker"]["id"].as_str().unwrap().to_string();

    let (status, body) = json_request(&app, "GET", "/api/bots/arthur/worker", &session, None).await;
    assert_eq!(status, StatusCode::OK.as_u16());
    assert!(body["workerId"].is_null());

    let (status, body) = json_request(
        &app,
        "PUT",
        "/api/bots/arthur/worker",
        &session,
        Some(json!({ "workerId": worker_id })),
    )
    .await;
    assert_eq!(status, StatusCode::OK.as_u16());
    assert_eq!(body["workerId"].as_str(), Some(worker_id.as_str()));

    let (status, _) = json_request(
        &app,
        "POST",
        &format!("/api/workers/{worker_id}/test"),
        &session,
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK.as_u16());

    let (status, body) = json_request(
        &app,
        "DELETE",
        &format!("/api/workers/{worker_id}"),
        &session,
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK.as_u16());
    assert!(body["workers"].as_array().unwrap().is_empty());
}
