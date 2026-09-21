//! S11-01: `/api/users/me` and adopt-owner login path.

mod common;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use common::seed_session;
use http_body_util::BodyExt;
use serde_json::Value;
use server::{AppState, build_app};
use store::OWNER_ID;
use tower::ServiceExt;

fn app_for(db: store::Db) -> axum::Router {
    build_app(AppState::new(db))
}

async fn login(app: axum::Router, password: &str) -> String {
    let response = app
        .clone()
        .oneshot(
            Request::post("/api/auth/login")
                .header("content-type", "application/json")
                .body(Body::from(format!(r#"{{"password":"{password}"}}"#)))
                .expect("request"),
        )
        .await
        .expect("response");
    assert_eq!(response.status(), StatusCode::OK);
    let bytes = response
        .into_body()
        .collect()
        .await
        .expect("body")
        .to_bytes();
    let json: Value = serde_json::from_slice(&bytes).expect("json");
    json["token"].as_str().expect("token").to_string()
}

#[tokio::test]
async fn users_me_returns_owner_after_login() {
    let db = store::Db::open(":memory:").expect("open");
    store::set_password(&db, "owner-password-long-enough").expect("password");
    let app = app_for(db);
    let token = login(app.clone(), "owner-password-long-enough").await;

    let response = app
        .oneshot(
            Request::get("/api/users/me")
                .header("authorization", format!("Bearer {token}"))
                .body(Body::empty())
                .expect("request"),
        )
        .await
        .expect("response");

    assert_eq!(response.status(), StatusCode::OK);
    let bytes = response
        .into_body()
        .collect()
        .await
        .expect("body")
        .to_bytes();
    let body: Value = serde_json::from_slice(&bytes).expect("json");

    assert_eq!(body["user"]["id"], OWNER_ID);
    assert_eq!(body["user"]["name"], "Josh Johnson");
    assert_eq!(body["user"]["role"], "owner");
    assert!(body["effectiveCeiling"].is_number());
}

#[tokio::test]
async fn users_me_works_with_seeded_session_cookie() {
    let db = store::Db::open(":memory:").expect("open");
    let cookie = seed_session(&db);
    let app = app_for(db);

    let response = app
        .oneshot(
            Request::get("/api/users/me")
                .header("cookie", cookie)
                .body(Body::empty())
                .expect("request"),
        )
        .await
        .expect("response");

    assert_eq!(response.status(), StatusCode::OK);
}
