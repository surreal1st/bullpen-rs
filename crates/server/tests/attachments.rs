//! S12-01: attachments and library HTTP.

mod common;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use http_body_util::BodyExt;
use serde_json::Value;
use server::{AppState, build_app};
use store::{CreateSessionOpts, OWNER_ID, adopt_owner, create_member, create_session_with};
use tower::ServiceExt;

fn bearer(token: &str) -> String {
    format!("Bearer {token}")
}

fn app_with_data(db: store::Db, data_dir: &str) -> axum::Router {
    let state = AppState::new(db);
    state.configure_w5(":memory:".to_string(), data_dir.to_string());
    build_app(state)
}

#[tokio::test]
async fn upload_round_trips_bytes() {
    let temp = tempfile::tempdir().expect("tempdir");
    let db = store::Db::open(":memory:").expect("open");
    store::set_password(&db, "owner-password-long-enough").expect("password");
    let token = store::create_session(&db).expect("session");
    let app = app_with_data(db, temp.path().to_str().unwrap());

    let upload = app
        .clone()
        .oneshot(
            Request::post("/api/attachments")
                .header("authorization", bearer(&token))
                .header("x-file-name", "hello.txt")
                .header("content-type", "text/plain")
                .body(Body::from("hello shelf"))
                .unwrap(),
        )
        .await
        .expect("upload");
    assert_eq!(upload.status(), StatusCode::CREATED);
    let bytes = upload.into_body().collect().await.unwrap().to_bytes();
    let body: Value = serde_json::from_slice(&bytes).unwrap();
    let id = body["attachment"]["id"].as_str().unwrap();

    let fetch = app
        .oneshot(
            Request::get(format!("/api/attachments/{id}"))
                .header("authorization", bearer(&token))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .expect("get");
    assert_eq!(fetch.status(), StatusCode::OK);
    let got = fetch.into_body().collect().await.unwrap().to_bytes();
    assert_eq!(got.as_ref(), b"hello shelf");
}

#[tokio::test]
async fn library_prefix_search_finds_name() {
    let temp = tempfile::tempdir().expect("tempdir");
    let db = store::Db::open(":memory:").expect("open");
    store::set_password(&db, "owner-password-long-enough").expect("password");
    let token = store::create_session(&db).expect("session");
    let app = app_with_data(db, temp.path().to_str().unwrap());

    for (name, body) in [
        ("quarterly-report.pdf", b"x".as_slice()),
        ("cat-photo.png", b"y".as_slice()),
    ] {
        let res = app
            .clone()
            .oneshot(
                Request::post("/api/attachments")
                    .header("authorization", bearer(&token))
                    .header("x-file-name", name)
                    .header("content-type", "application/octet-stream")
                    .body(Body::from(body))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::CREATED);
    }

    let list = app
        .oneshot(
            Request::get("/api/library?q=quar")
                .header("authorization", bearer(&token))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(list.status(), StatusCode::OK);
    let bytes = list.into_body().collect().await.unwrap().to_bytes();
    let body: Value = serde_json::from_slice(&bytes).unwrap();
    let names: Vec<&str> = body["items"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|i| i["name"].as_str())
        .collect();
    assert_eq!(names, vec!["quarterly-report.pdf"]);
}

#[tokio::test]
async fn member_cannot_fetch_owners_attachment() {
    let temp = tempfile::tempdir().expect("tempdir");
    let db = store::Db::open(":memory:").expect("open");
    store::set_password(&db, "owner-password-long-enough").expect("password");
    adopt_owner(&db).expect("adopt");

    let owner_token =
        create_session_with(&db, CreateSessionOpts::owner_sign_in(OWNER_ID.to_string()))
            .expect("owner session");
    let member = create_member(&db, "Kellie", "member-password-long", None).expect("member");
    let member_token = create_session_with(
        &db,
        CreateSessionOpts {
            user_id: Some(member.id),
            stamp_last_login: false,
        },
    )
    .expect("member session");

    let app = app_with_data(db, temp.path().to_str().unwrap());

    let upload = app
        .clone()
        .oneshot(
            Request::post("/api/attachments")
                .header("authorization", bearer(&owner_token))
                .header("x-file-name", "secret.txt")
                .header("content-type", "text/plain")
                .body(Body::from("owner only"))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(upload.status(), StatusCode::CREATED);
    let bytes = upload.into_body().collect().await.unwrap().to_bytes();
    let body: Value = serde_json::from_slice(&bytes).unwrap();
    let id = body["attachment"]["id"].as_str().unwrap();

    let stolen = app
        .oneshot(
            Request::get(format!("/api/attachments/{id}"))
                .header("authorization", bearer(&member_token))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(stolen.status(), StatusCode::NOT_FOUND);
}
