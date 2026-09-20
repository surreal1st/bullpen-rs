//! S7-01: connector registry and per-bot enablement routes.

mod common;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use common::seed_session;
use serde_json::{Value, json};
use server::{AppState, build_app};
use store::Db;
use tower::ServiceExt;

fn open_db() -> Db {
    let db = Db::open(":memory:").expect("open db");
    model::routing::set_routing_settings(&db, Some(false), None).expect("routing off");
    server::judge::set_judge_enabled(&db, false).expect("judge off");
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

async fn put_json(
    app: &axum::Router,
    path: &str,
    session: &str,
    body: Value,
) -> (StatusCode, Value) {
    let request = Request::put(path)
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

async fn delete_path(app: &axum::Router, path: &str, session: &str) -> (StatusCode, Value) {
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

#[tokio::test]
async fn create_list_and_delete_connector_strips_auth_header() {
    let db = open_db();
    let session = seed_session(&db);
    let app = build_app(AppState::new(db));

    let (status, body) = post_json(
        &app,
        "/api/connectors",
        &session,
        json!({
            "name": "Test MCP",
            "url": "https://mcp.example.com/v1",
            "authHeader": "Bearer secret-token"
        }),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED);
    assert_eq!(body["reachable"], false);
    let id = body["connector"]["id"].as_str().unwrap().to_string();
    assert!(body["connector"]["authHeader"].is_null());

    let (list_status, list) = get_json(&app, "/api/connectors", &session).await;
    assert_eq!(list_status, StatusCode::OK);
    let row = list["connectors"]
        .as_array()
        .unwrap()
        .iter()
        .find(|c| c["id"].as_str() == Some(&id))
        .expect("listed");
    assert!(row.get("authHeader").is_none());

    let (del_status, _) = delete_path(&app, &format!("/api/connectors/{id}"), &session).await;
    assert_eq!(del_status, StatusCode::OK);

    let (_, after) = get_json(&app, "/api/connectors", &session).await;
    assert!(after["connectors"].as_array().unwrap().is_empty());
}

#[tokio::test]
async fn bot_connector_enablement_round_trip() {
    let db = open_db();
    seed_bot(&db, "arthur", "Arthur");
    let session = seed_session(&db);
    let app = build_app(AppState::new(db));

    let (_, created) = post_json(
        &app,
        "/api/connectors",
        &session,
        json!({ "name": "GitHub", "url": "https://api.githubcopilot.com/mcp/" }),
    )
    .await;
    let connector_id = created["connector"]["id"].as_str().unwrap();

    let (status, body) = put_json(
        &app,
        &format!("/api/bots/arthur/connectors/{connector_id}"),
        &session,
        json!({ "enabled": true }),
    )
    .await;
    assert_eq!(status, StatusCode::OK);

    let (_, bots) = get_json(&app, "/api/bots/arthur/connectors", &session).await;
    let github = bots["connectors"]
        .as_array()
        .unwrap()
        .iter()
        .find(|c| c["id"].as_str() == Some(connector_id))
        .expect("github row");
    assert_eq!(github["enabled"], true);
}
