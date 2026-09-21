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

const MEMBER_PASSWORD: &str = "kellie-long-password";

async fn add_member(app: axum::Router, owner_token: &str, name: &str) -> String {
    let response = app
        .clone()
        .oneshot(
            Request::post("/api/users/invite")
                .header("authorization", format!("Bearer {owner_token}"))
                .body(Body::empty())
                .expect("request"),
        )
        .await
        .expect("response");
    assert_eq!(response.status(), StatusCode::CREATED);
    let bytes = response
        .into_body()
        .collect()
        .await
        .expect("body")
        .to_bytes();
    let json: Value = serde_json::from_slice(&bytes).expect("json");
    let invite_token = json["invite"]["token"].as_str().expect("token");

    let response = app
        .clone()
        .oneshot(
            Request::post(format!("/api/invites/{invite_token}/claim"))
                .header("content-type", "application/json")
                .body(Body::from(format!(
                    r#"{{"name":"{name}","password":"{MEMBER_PASSWORD}"}}"#
                )))
                .expect("request"),
        )
        .await
        .expect("response");
    assert_eq!(response.status(), StatusCode::CREATED);
    let bytes = response
        .into_body()
        .collect()
        .await
        .expect("body")
        .to_bytes();
    let json: Value = serde_json::from_slice(&bytes).expect("json");
    json["token"].as_str().expect("session").to_string()
}

#[tokio::test]
async fn owner_lists_users_and_mints_invite() {
    let db = store::Db::open(":memory:").expect("open");
    store::set_password(&db, "owner-password-long-enough").expect("password");
    let app = app_for(db);
    let owner_token = login(app.clone(), "owner-password-long-enough").await;

    let response = app
        .clone()
        .oneshot(
            Request::get("/api/users")
                .header("authorization", format!("Bearer {owner_token}"))
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
    assert_eq!(body["users"][0]["id"], OWNER_ID);

    let member_token = add_member(app.clone(), &owner_token, "Kellie").await;

    let response = app
        .oneshot(
            Request::get("/api/auth/status")
                .header("authorization", format!("Bearer {member_token}"))
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
    assert_eq!(body["role"], "member");
}

#[tokio::test]
async fn member_cannot_list_users() {
    let db = store::Db::open(":memory:").expect("open");
    store::set_password(&db, "owner-password-long-enough").expect("password");
    let app = app_for(db);
    let owner_token = login(app.clone(), "owner-password-long-enough").await;
    let member_token = add_member(app.clone(), &owner_token, "Kellie").await;

    let response = app
        .oneshot(
            Request::get("/api/users")
                .header("authorization", format!("Bearer {member_token}"))
                .body(Body::empty())
                .expect("request"),
        )
        .await
        .expect("response");
    assert_eq!(response.status(), StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn invite_valid_and_claim_without_session() {
    let db = store::Db::open(":memory:").expect("open");
    store::set_password(&db, "owner-password-long-enough").expect("password");
    let app = app_for(db);
    let owner_token = login(app.clone(), "owner-password-long-enough").await;

    let response = app
        .clone()
        .oneshot(
            Request::post("/api/users/invite")
                .header("authorization", format!("Bearer {owner_token}"))
                .body(Body::empty())
                .expect("request"),
        )
        .await
        .expect("response");
    let bytes = response
        .into_body()
        .collect()
        .await
        .expect("body")
        .to_bytes();
    let json: Value = serde_json::from_slice(&bytes).expect("json");
    let token = json["invite"]["token"].as_str().expect("token");

    let response = app
        .clone()
        .oneshot(
            Request::get(format!("/api/invites/{token}"))
                .body(Body::empty())
                .expect("request"),
        )
        .await
        .expect("response");
    assert_eq!(response.status(), StatusCode::OK);

    let response = app
        .clone()
        .oneshot(
            Request::post(format!("/api/invites/{token}/claim"))
                .header("content-type", "application/json")
                .body(Body::from(format!(
                    r#"{{"name":"Jesse","password":"{MEMBER_PASSWORD}"}}"#
                )))
                .expect("request"),
        )
        .await
        .expect("response");
    assert_eq!(response.status(), StatusCode::CREATED);

    let response = app
        .oneshot(
            Request::get(format!("/api/invites/{token}"))
                .body(Body::empty())
                .expect("request"),
        )
        .await
        .expect("response");
    assert_eq!(response.status(), StatusCode::NOT_FOUND);
}
