//! S7-01: connector registry and per-bot enablement routes.

mod common;

use async_trait::async_trait;
use axum::body::Body;
use axum::http::{Request, StatusCode};
use common::seed_session;
use reqwest::header::HeaderMap;
use serde_json::{Value, json};
use server::egress::Resolver;
use server::mcp::{McpHttpResponse, McpTransport};
use server::{AppState, build_app};
use std::sync::Arc;
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

struct FakeMcpTransport;

#[async_trait]
impl McpTransport for FakeMcpTransport {
    async fn post(
        &self,
        _url: &str,
        _headers: HeaderMap,
        body: String,
    ) -> Result<McpHttpResponse, String> {
        let parsed: serde_json::Value = serde_json::from_str(&body).unwrap_or(json!({}));
        let method = parsed.get("method").and_then(|m| m.as_str()).unwrap_or("");
        let id = parsed.get("id").cloned().unwrap_or(json!("1"));
        let result = match method {
            "initialize" => json!({ "protocolVersion": "2025-06-18" }),
            "tools/list" => json!({
                "tools": [{
                    "name": "search",
                    "description": "Searches.",
                    "inputSchema": { "type": "object", "properties": { "q": { "type": "string" } } }
                }]
            }),
            _ => json!({}),
        };
        let payload = json!({ "jsonrpc": "2.0", "id": id, "result": result }).to_string();
        Ok(McpHttpResponse {
            status: 200,
            headers: HeaderMap::new(),
            body: payload,
        })
    }
}

struct PublicResolver;

#[async_trait]
impl Resolver for PublicResolver {
    async fn resolve(&self, _host: &str) -> Result<Vec<String>, String> {
        Ok(vec!["93.184.216.34".to_string()])
    }
}

struct PrivateResolver;

#[async_trait]
impl Resolver for PrivateResolver {
    async fn resolve(&self, _host: &str) -> Result<Vec<String>, String> {
        Ok(vec!["127.0.0.1".to_string()])
    }
}

#[tokio::test]
async fn create_list_and_delete_connector_strips_auth_header() {
    let db = open_db();
    let session = seed_session(&db);
    let app = build_app(AppState::with_mcp(
        db,
        Arc::new(FakeMcpTransport),
        Arc::new(PublicResolver),
    ));

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
    assert_eq!(body["reachable"], true);
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
    let app = build_app(AppState::with_mcp(
        db,
        Arc::new(FakeMcpTransport),
        Arc::new(PublicResolver),
    ));

    let (_, created) = post_json(
        &app,
        "/api/connectors",
        &session,
        json!({ "name": "GitHub", "url": "https://api.githubcopilot.com/mcp/" }),
    )
    .await;
    let connector_id = created["connector"]["id"].as_str().unwrap();

    let (status, _body) = put_json(
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

#[tokio::test]
async fn connector_tools_route_lists_mcp_tools() {
    let db = open_db();
    let session = seed_session(&db);
    let app = build_app(AppState::with_mcp(
        db,
        Arc::new(FakeMcpTransport),
        Arc::new(PublicResolver),
    ));

    let (_, created) = post_json(
        &app,
        "/api/connectors",
        &session,
        json!({ "name": "Fake", "url": "https://fake-mcp.test/mcp" }),
    )
    .await;
    let id = created["connector"]["id"].as_str().unwrap();
    let (status, body) = get_json(&app, &format!("/api/connectors/{id}/tools"), &session).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["tools"].as_array().unwrap().len(), 1);
    assert_eq!(body["tools"][0]["name"], "search");
}

#[tokio::test]
async fn private_mcp_host_is_not_reachable() {
    let db = open_db();
    let session = seed_session(&db);
    let app = build_app(AppState::with_mcp(
        db,
        Arc::new(FakeMcpTransport),
        Arc::new(PrivateResolver),
    ));
    let (status, body) = post_json(
        &app,
        "/api/connectors",
        &session,
        json!({ "name": "Bad", "url": "https://fake-mcp.test/mcp" }),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED);
    assert_eq!(body["reachable"], false);
    assert!(
        body["error"]
            .as_str()
            .unwrap_or("")
            .contains("inside this network")
    );
}
