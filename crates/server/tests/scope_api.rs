//! S11-02: scope guard and scoped roster lists.

mod common;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use http_body_util::BodyExt;
use serde_json::Value;
use server::{AppState, build_app};
use store::{CreateSessionOpts, OWNER_ID, adopt_owner, create_member, create_session_with};
use tower::ServiceExt;

fn app_for(db: store::Db) -> axum::Router {
    build_app(AppState::new(db))
}

fn bearer(token: &str) -> String {
    format!("Bearer {token}")
}

#[tokio::test]
async fn member_roster_excludes_owners_bots() {
    let db = store::Db::open(":memory:").expect("open");
    store::set_password(&db, "owner-password-long-enough").expect("password");
    store::adopt_owner(&db).expect("adopt");

    let owner_token =
        create_session_with(&db, CreateSessionOpts::owner_sign_in(OWNER_ID.to_string()))
            .expect("owner session");

    let member = create_member(&db, "Kellie", "member-password-long", None).expect("member");
    let member_token = create_session_with(
        &db,
        CreateSessionOpts {
            user_id: Some(member.id.clone()),
            stamp_last_login: false,
        },
    )
    .expect("member session");

    let app = app_for(db);

    let create_owner_bot = app
        .clone()
        .oneshot(
            Request::post("/api/bots")
                .header("authorization", bearer(&owner_token))
                .header("content-type", "application/json")
                .body(Body::from(r#"{"name":"Josh Bot"}"#))
                .unwrap(),
        )
        .await
        .expect("create");
    assert_eq!(create_owner_bot.status(), StatusCode::CREATED);

    let create_member_bot = app
        .clone()
        .oneshot(
            Request::post("/api/bots")
                .header("authorization", bearer(&member_token))
                .header("content-type", "application/json")
                .body(Body::from(r#"{"name":"Kellie Bot"}"#))
                .unwrap(),
        )
        .await
        .expect("create member bot");
    assert_eq!(create_member_bot.status(), StatusCode::CREATED);

    let roster = app
        .oneshot(
            Request::get("/api/roster")
                .header("authorization", bearer(&member_token))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .expect("roster");
    assert_eq!(roster.status(), StatusCode::OK);
    let bytes = roster.into_body().collect().await.unwrap().to_bytes();
    let body: Value = serde_json::from_slice(&bytes).unwrap();
    let names: Vec<&str> = body["bots"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|b| b["name"].as_str())
        .collect();
    assert!(names.contains(&"Kellie Bot"));
    assert!(!names.contains(&"Josh Bot"));
}

#[tokio::test]
async fn member_cannot_get_spend() {
    let db = store::Db::open(":memory:").expect("open");
    store::set_password(&db, "owner-password-long-enough").expect("password");
    adopt_owner(&db).expect("adopt");
    let member = create_member(&db, "Kellie", "member-password-long", None).expect("member");
    let member_token = create_session_with(
        &db,
        CreateSessionOpts {
            user_id: Some(member.id),
            ..Default::default()
        },
    )
    .expect("session");

    let app = app_for(db);
    let res = app
        .oneshot(
            Request::get("/api/spend")
                .header("authorization", bearer(&member_token))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn users_me_still_works_for_member() {
    let db = store::Db::open(":memory:").expect("open");
    store::set_password(&db, "owner-password-long-enough").expect("password");
    adopt_owner(&db).expect("adopt");
    let member = create_member(&db, "Kellie", "member-password-long", None).expect("member");
    let member_token = create_session_with(
        &db,
        CreateSessionOpts {
            user_id: Some(member.id.clone()),
            stamp_last_login: false,
        },
    )
    .expect("session");

    let app = app_for(db);
    let res = app
        .oneshot(
            Request::get("/api/users/me")
                .header("authorization", bearer(&member_token))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::OK);
    let bytes = res.into_body().collect().await.unwrap().to_bytes();
    let body: Value = serde_json::from_slice(&bytes).unwrap();
    assert_eq!(body["user"]["role"], "member");
    assert_ne!(body["user"]["id"], OWNER_ID);
}
