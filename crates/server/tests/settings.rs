//! S2-06: settings routes - theme, timezone, rules, models. Port of
//! `projects/bullpen-night/src/server/app.ts:2902-3080,4374-4400`.

use axum::Router;
use axum::body::Body;
use axum::http::Request;
use model::catalog::FixtureCatalog;
use serde_json::{Value, json};
use server::AppState;
use std::sync::Arc;
use store::Db;
use tower::ServiceExt;

mod common;
use common::seed_session;

fn open_db() -> Db {
    let db = Db::open(":memory:").expect("open :memory: db");
    model::routing::ensure_routing_tables(&db).expect("ensure routing tables");
    db
}

fn app_for(db: Db) -> Router {
    let state = AppState::new(db);
    server::build_app(state)
}

fn app_with_catalog(db: Db, catalog: Arc<dyn model::catalog::Catalog>) -> Router {
    let state = AppState::with_catalog(db, catalog);
    server::build_app(state)
}

// Fixture catalog for models tests
fn fixture_catalog() -> Arc<dyn model::catalog::Catalog> {
    let json = r#"[
        {
            "id": "anthropic/claude-sonnet-5",
            "name": "Claude Sonnet 5",
            "inPerM": 3.0,
            "outPerM": 15.0,
            "contextLength": 200000,
            "supportsTools": true,
            "supportsImages": true,
            "supportsReasoning": false,
            "providerCount": 2,
            "supportsCaching": true
        },
        {
            "id": "anthropic/claude-fable-5.1",
            "name": "Claude Fable 5.1",
            "inPerM": 1.5,
            "outPerM": 6.0,
            "contextLength": 100000,
            "supportsTools": true,
            "supportsImages": true,
            "supportsReasoning": false,
            "providerCount": 2,
            "supportsCaching": true
        }
    ]"#;
    Arc::new(FixtureCatalog::from_json(json).expect("parse fixture"))
}

// Helpers for making requests
async fn get_route(app: &Router, path: &str, session: &str) -> (u16, Value) {
    let request = Request::get(path)
        .header("cookie", session)
        .body(Body::empty())
        .unwrap();

    let response = app.clone().oneshot(request).await.unwrap();
    let status = response.status().as_u16();
    let body = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap();
    let value: Value = serde_json::from_slice(&body).unwrap_or(json!({}));
    (status, value)
}

async fn put_route(app: &Router, path: &str, session: &str, body: Value) -> (u16, Value) {
    let body_json = body.to_string();
    let request = Request::put(path)
        .header("cookie", session)
        .header("content-type", "application/json")
        .body(Body::from(body_json))
        .unwrap();

    let response = app.clone().oneshot(request).await.unwrap();
    let status = response.status().as_u16();
    let body = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap();
    let value: Value = serde_json::from_slice(&body).unwrap_or(json!({}));
    (status, value)
}

#[tokio::test]
async fn test_theme_get_put_roundtrip() {
    let db = open_db();
    let session = seed_session(&db);
    let app = app_for(db);

    // GET returns default theme
    let (status, response) = get_route(&app, "/api/theme", &session).await;
    assert_eq!(status, 200);
    assert_eq!(
        response["theme"]["accent"], "seam",
        "default theme should have seam accent"
    );

    // PUT sets theme
    let theme = json!({
        "accent": "ember",
        "mine": "neutral",
        "ground": "ink",
        "radius": "sharp"
    });
    let (status, response) =
        put_route(&app, "/api/theme", &session, json!({ "theme": theme })).await;
    assert_eq!(status, 200);
    assert_eq!(response["theme"]["accent"], "ember");
    assert_eq!(response["theme"]["mine"], "neutral");

    // GET returns the set theme
    let (status, response) = get_route(&app, "/api/theme", &session).await;
    assert_eq!(status, 200);
    assert_eq!(response["theme"]["accent"], "ember");
    assert_eq!(response["theme"]["mine"], "neutral");
}

#[tokio::test]
async fn test_timezone_get_put_roundtrip() {
    let db = open_db();
    let session = seed_session(&db);
    let app = app_for(db);

    // GET returns "auto" by default
    let (status, response) = get_route(&app, "/api/timezone", &session).await;
    assert_eq!(status, 200);
    assert_eq!(response["timezone"], "auto");

    // PUT sets timezone
    let (status, response) = put_route(
        &app,
        "/api/timezone",
        &session,
        json!({ "timezone": "America/Chicago" }),
    )
    .await;
    assert_eq!(status, 200);
    assert_eq!(response["timezone"], "America/Chicago");

    // GET returns the set timezone
    let (status, response) = get_route(&app, "/api/timezone", &session).await;
    assert_eq!(status, 200);
    assert_eq!(response["timezone"], "America/Chicago");
}

#[tokio::test]
async fn test_rules_get_put_roundtrip() {
    let db = open_db();
    let session = seed_session(&db);
    let app = app_for(db);

    // GET returns the house rules
    let (status, response) = get_route(&app, "/api/rules", &session).await;
    assert_eq!(status, 200);
    assert!(response["rules"].is_string());

    // PUT sets new rules
    let (status, response) = put_route(
        &app,
        "/api/rules",
        &session,
        json!({ "rules": "Test rules" }),
    )
    .await;
    assert_eq!(status, 200);
    assert_eq!(response["rules"], "Test rules");

    // GET returns the set rules
    let (status, response) = get_route(&app, "/api/rules", &session).await;
    assert_eq!(status, 200);
    assert_eq!(response["rules"], "Test rules");
}

#[tokio::test]
async fn test_default_model_get_put_roundtrip() {
    let db = open_db();
    let session = seed_session(&db);
    let app = app_for(db);

    // GET returns default model (cheap default)
    let (status, response) = get_route(&app, "/api/default-model", &session).await;
    assert_eq!(status, 200);
    assert!(response["model"].is_string());

    // PUT sets model
    let (status, response) = put_route(
        &app,
        "/api/default-model",
        &session,
        json!({ "model": "anthropic/claude-sonnet-5" }),
    )
    .await;
    assert_eq!(status, 200);
    assert_eq!(response["model"], "anthropic/claude-sonnet-5");

    // GET returns the set model
    let (status, response) = get_route(&app, "/api/default-model", &session).await;
    assert_eq!(status, 200);
    assert_eq!(response["model"], "anthropic/claude-sonnet-5");
}

#[tokio::test]
async fn test_default_model_refuses_premium() {
    let db = open_db();
    let session = seed_session(&db);
    let app = app_for(db);

    // PUT with a premium model (contains "fable") should be refused
    let (status, response) = put_route(
        &app,
        "/api/default-model",
        &session,
        json!({ "model": "anthropic/claude-fable-5.1" }),
    )
    .await;
    assert_eq!(status, 400);
    assert!(
        response["error"]
            .as_str()
            .unwrap_or("")
            .contains("premium model")
    );
}

#[tokio::test]
async fn test_mid_model_get_put_roundtrip() {
    let db = open_db();
    let session = seed_session(&db);
    let app = app_for(db);

    // GET returns default mid model
    let (status, response) = get_route(&app, "/api/mid-model", &session).await;
    assert_eq!(status, 200);
    assert!(response["model"].is_string());

    // PUT sets model
    let (status, response) = put_route(
        &app,
        "/api/mid-model",
        &session,
        json!({ "model": "anthropic/claude-sonnet-5" }),
    )
    .await;
    assert_eq!(status, 200);
    assert_eq!(response["model"], "anthropic/claude-sonnet-5");
}

#[tokio::test]
async fn test_tier1_models_get_put_roundtrip() {
    let db = open_db();
    let session = seed_session(&db);
    let app = app_for(db);

    // GET returns tier1 models
    let (status, response) = get_route(&app, "/api/tier1-models", &session).await;
    assert_eq!(status, 200);
    assert!(response["models"]["code"].is_string());
    assert!(response["models"]["reason"].is_string());
    assert!(response["models"]["vision"].is_string());
    assert_eq!(response["kinds"], json!(["code", "reason", "vision"]));

    // PUT sets a code tier1 model
    let (status, response) = put_route(
        &app,
        "/api/tier1-models",
        &session,
        json!({
            "kind": "code",
            "model": "anthropic/claude-sonnet-5"
        }),
    )
    .await;
    assert_eq!(status, 200);
    assert_eq!(response["kind"], "code");
    assert_eq!(response["model"], "anthropic/claude-sonnet-5");

    // GET returns the updated model
    let (status, response) = get_route(&app, "/api/tier1-models", &session).await;
    assert_eq!(status, 200);
    assert_eq!(response["models"]["code"], "anthropic/claude-sonnet-5");
}

#[tokio::test]
async fn test_premium_model_get_put_roundtrip() {
    let db = open_db();
    let session = seed_session(&db);
    let app = app_for(db);

    // GET returns default premium model
    let (status, response) = get_route(&app, "/api/premium-model", &session).await;
    assert_eq!(status, 200);
    assert!(response["model"].is_string());

    // PUT sets model
    let (status, response) = put_route(
        &app,
        "/api/premium-model",
        &session,
        json!({ "model": "anthropic/claude-fable-5.1" }),
    )
    .await;
    assert_eq!(status, 200);
    assert_eq!(response["model"], "anthropic/claude-fable-5.1");
}

#[tokio::test]
async fn test_models_search() {
    let db = open_db();
    let session = seed_session(&db);
    let catalog = fixture_catalog();
    let app = app_with_catalog(db, catalog);

    // GET /api/models returns all models
    let (status, response) = get_route(&app, "/api/models", &session).await;
    assert_eq!(status, 200);
    assert_eq!(response["models"].as_array().unwrap().len(), 2);
    assert_eq!(response["total"], 2);
    assert_eq!(response["defaultModel"], response["defaultModel"]);

    // Search by model name
    let (status, response) = get_route(&app, "/api/models?q=sonnet", &session).await;
    assert_eq!(status, 200);
    assert_eq!(response["models"].as_array().unwrap().len(), 1);
    assert_eq!(response["models"][0]["id"], "anthropic/claude-sonnet-5");

    // Search by model id
    let (status, response) = get_route(&app, "/api/models?q=fable", &session).await;
    assert_eq!(status, 200);
    assert_eq!(response["models"].as_array().unwrap().len(), 1);
    assert_eq!(response["models"][0]["name"], "Claude Fable 5.1");
}

#[tokio::test]
async fn test_routing_get_put_toggle() {
    let db = open_db();
    let session = seed_session(&db);
    let app = app_for(db);

    // GET returns routing settings
    let (status, response) = get_route(&app, "/api/routing", &session).await;
    assert_eq!(status, 200);
    assert!(response["enabled"].is_boolean());
    assert!(response["text"].is_string());
    assert!(response["log"].is_array());

    // PUT to toggle enabled
    let (status, _response) =
        put_route(&app, "/api/routing", &session, json!({ "enabled": false })).await;
    assert_eq!(status, 200);

    // GET should show toggled value
    let (status, response) = get_route(&app, "/api/routing", &session).await;
    assert_eq!(status, 200);
    assert_eq!(response["enabled"], false);
}

#[tokio::test]
async fn test_second_opinion_get_put_roundtrip() {
    let db = open_db();
    let session = seed_session(&db);
    let app = app_for(db);

    // GET returns default (false)
    let (status, response) = get_route(&app, "/api/second-opinion", &session).await;
    assert_eq!(status, 200);
    assert_eq!(response["enabled"], false);

    // PUT to enable
    let (status, response) = put_route(
        &app,
        "/api/second-opinion",
        &session,
        json!({ "enabled": true }),
    )
    .await;
    assert_eq!(status, 200);
    assert_eq!(response["enabled"], true);

    // GET should show enabled
    let (status, response) = get_route(&app, "/api/second-opinion", &session).await;
    assert_eq!(status, 200);
    assert_eq!(response["enabled"], true);
}
