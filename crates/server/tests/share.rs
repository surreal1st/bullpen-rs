//! S11-05: bot share links (port of `export-share-templates.test.ts` share section).

mod common;

use axum::Router;
use axum::body::Body;
use axum::http::{Request, StatusCode};
use http_body_util::BodyExt;
use serde_json::{Value, json};
use server::{AppState, build_app};
use store::Db;
use tower::ServiceExt;

use common::seed_session;

fn open_db() -> Db {
    Db::open(":memory:").expect("open")
}

fn app_for(db: Db) -> Router {
    build_app(AppState::new(db))
}

fn seed_bot(db: &Db, id: &str, name: &str) {
    db.conn()
        .execute(
            "INSERT INTO bots (id, name, purpose, instructions, model, created_at, permissions) VALUES (?1, ?2, '', ?3, NULL, '2026-01-01T00:00:00Z', ?4)",
            rusqlite::params![id, name, format!("You are {name}."), "{}"],
        )
        .expect("seed bot");
}

async fn post_json(app: &Router, path: &str, cookie: &str) -> (u16, Value) {
    let req = Request::post(path)
        .header("cookie", cookie)
        .body(Body::empty())
        .unwrap();
    let resp = app.clone().oneshot(req).await.unwrap();
    let status = resp.status().as_u16();
    let bytes = resp.into_body().collect().await.unwrap().to_bytes();
    let body = serde_json::from_slice(&bytes).unwrap_or(json!({}));
    (status, body)
}

async fn get_route(app: &Router, path: &str, cookie: Option<&str>) -> (u16, Vec<u8>) {
    let mut req = Request::get(path);
    if let Some(c) = cookie {
        req = req.header("cookie", c);
    }
    let resp = app
        .clone()
        .oneshot(req.body(Body::empty()).unwrap())
        .await
        .unwrap();
    let status = resp.status().as_u16();
    let bytes = resp
        .into_body()
        .collect()
        .await
        .unwrap()
        .to_bytes()
        .to_vec();
    (status, bytes)
}

#[tokio::test]
async fn share_link_serves_export_without_session() {
    let db = open_db();
    seed_bot(&db, "share-me", "Share Me");
    let session = seed_session(&db);
    let app = app_for(db);

    let (status, body) = post_json(&app, "/api/bots/share-me/share", &session).await;
    assert_eq!(status, 201, "{body:?}");
    let token = body["share"]["token"].as_str().expect("token");

    let (status, bytes) = get_route(&app, &format!("/api/share/{token}"), None).await;
    assert_eq!(status, StatusCode::OK.as_u16());
    let text = String::from_utf8(bytes).expect("utf8");
    assert!(text.contains("Share Me"));
    assert!(text.contains("---"));
}

#[tokio::test]
async fn invalid_share_token_is_404() {
    let app = app_for(open_db());
    let (status, _) = get_route(&app, "/api/share/not-a-real-token", None).await;
    assert_eq!(status, StatusCode::NOT_FOUND.as_u16());
}

#[tokio::test]
async fn revoke_share_link_stops_public_export() {
    let db = open_db();
    seed_bot(&db, "revoke-me", "Revoke Me");
    let session = seed_session(&db);
    let app = app_for(db);

    let (status, body) = post_json(&app, "/api/bots/revoke-me/share", &session).await;
    assert_eq!(status, 201);
    let token = body["share"]["token"].as_str().unwrap().to_string();

    let del = Request::delete("/api/bots/revoke-me/share")
        .header("cookie", session)
        .body(Body::empty())
        .unwrap();
    let resp = app.clone().oneshot(del).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);

    let (status, _) = get_route(&app, &format!("/api/share/{token}"), None).await;
    assert_eq!(status, StatusCode::NOT_FOUND.as_u16());
}

#[tokio::test]
async fn creating_a_second_share_replaces_the_first() {
    let db = open_db();
    seed_bot(&db, "replace-me", "Replace Me");
    let session = seed_session(&db);
    let app = app_for(db);

    let (_, body1) = post_json(&app, "/api/bots/replace-me/share", &session).await;
    let token1 = body1["share"]["token"].as_str().unwrap().to_string();
    let (_, body2) = post_json(&app, "/api/bots/replace-me/share", &session).await;
    let token2 = body2["share"]["token"].as_str().unwrap().to_string();

    assert_ne!(token1, token2);
    let (s1, _) = get_route(&app, &format!("/api/share/{token1}"), None).await;
    let (s2, _) = get_route(&app, &format!("/api/share/{token2}"), None).await;
    assert_eq!(s1, StatusCode::NOT_FOUND.as_u16());
    assert_eq!(s2, StatusCode::OK.as_u16());
}
