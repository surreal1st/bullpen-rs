//! S10-06: marketplace templates list + bot install routes.

mod common;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use common::seed_session;
use serde_json::{Value, json};
use server::{AppState, build_app};
use store::Db;
use tower::ServiceExt;

fn open_db() -> Db {
    let db = Db::open(":memory:").expect("open :memory: db");
    model::routing::set_routing_settings(&db, Some(false), None)
        .expect("disable routing classifier");
    server::judge::set_judge_enabled(&db, false).expect("disable judge");
    db
}

fn fixture_catalog() -> std::sync::Arc<dyn model::catalog::Catalog> {
    let json = r#"[
        {
            "id": "grok-3",
            "name": "Grok 3",
            "inPerM": 1.0,
            "outPerM": 2.0,
            "contextLength": 128000,
            "supportsTools": true,
            "supportsImages": false,
            "supportsReasoning": false,
            "providerCount": 1,
            "supportsCaching": false
        }
    ]"#;
    std::sync::Arc::new(model::FixtureCatalog::from_json(json).unwrap())
}

fn no_network_directory() {
    unsafe { std::env::set_var("BULLPEN_BOT_DIRECTORY_URL", "http://127.0.0.1:1/no-network") };
}

fn templates_dir() -> String {
    std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../templates")
        .canonicalize()
        .expect("templates dir")
        .to_string_lossy()
        .to_string()
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

#[tokio::test]
async fn templates_list_includes_bundled_writer() {
    unsafe { std::env::set_var("BULLPEN_TEMPLATES_DIR", templates_dir()) };
    no_network_directory();
    let db = open_db();
    let session = seed_session(&db);
    let app = build_app(AppState::new(db));
    let (status, body) = get_json(&app, "/api/marketplace/templates", &session).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["ok"], true);
    let names: Vec<_> = body["cards"]
        .as_array()
        .unwrap()
        .iter()
        .map(|c| c["name"].as_str().unwrap())
        .collect();
    assert!(names.iter().any(|n| n.contains("Writer")));
}

#[tokio::test]
async fn install_starter_template_creates_bot() {
    unsafe { std::env::set_var("BULLPEN_TEMPLATES_DIR", templates_dir()) };
    no_network_directory();
    let db = open_db();
    let session = seed_session(&db);
    let app = build_app(AppState::with_catalog(db, fixture_catalog()));
    let (status, body) = post_json(
        &app,
        "/api/marketplace/install",
        &session,
        json!({ "kind": "bot", "name": "Researcher" }),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED);
    assert_eq!(body["ok"], true);
    assert_eq!(body["installed"], "bot");
    let grants = body["grants"].as_str().unwrap_or_default();
    assert!(grants.contains("Researcher"));
    let (_st, roster) = get_json(&app, "/api/roster", &session).await;
    assert!(
        roster["bots"]
            .as_array()
            .unwrap()
            .iter()
            .any(|b| b["name"].as_str() == Some("Researcher"))
    );
}

#[tokio::test]
async fn install_refuses_duplicate_name() {
    unsafe { std::env::set_var("BULLPEN_TEMPLATES_DIR", templates_dir()) };
    no_network_directory();
    let db = open_db();
    let session = seed_session(&db);
    let app = build_app(AppState::with_catalog(db, fixture_catalog()));
    let body = json!({ "kind": "bot", "name": "Writer" });
    let (s1, _) = post_json(&app, "/api/marketplace/install", &session, body.clone()).await;
    assert_eq!(s1, StatusCode::CREATED);
    let (s2, body2) = post_json(&app, "/api/marketplace/install", &session, body).await;
    assert_eq!(s2, StatusCode::BAD_REQUEST);
    assert!(
        body2["error"]
            .as_str()
            .unwrap()
            .to_lowercase()
            .contains("already")
    );
}
