//! S12-09: `attachmentId` on `POST /api/bots/:id/messages`.

mod common;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use http_body_util::BodyExt;
use serde_json::Value;
use server::{AppState, build_app};
use store::{BotDraft, create_bot, list_messages};
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
async fn message_stores_attachment_id() {
    let temp = tempfile::tempdir().expect("tempdir");
    let db_path = temp.path().join("test.db");
    let db_path_str = db_path.to_str().expect("utf8 path");
    let db = store::Db::open(db_path_str).expect("open");
    store::set_password(&db, "owner-password-long-enough").expect("password");
    let token = store::create_session(&db).expect("session");
    let bot = create_bot(
        &db,
        BotDraft {
            name: "Riley".into(),
            purpose: "test".into(),
            instructions: String::new(),
            model: None,
        },
    )
    .expect("bot");
    let app = app_with_data(db, temp.path().to_str().unwrap());

    let upload = app
        .clone()
        .oneshot(
            Request::post("/api/attachments")
                .header("authorization", bearer(&token))
                .header("x-file-name", "note.txt")
                .header("content-type", "text/plain")
                .body(Body::from("hello"))
                .unwrap(),
        )
        .await
        .expect("upload");
    assert_eq!(upload.status(), StatusCode::CREATED);
    let bytes = upload.into_body().collect().await.unwrap().to_bytes();
    let body: Value = serde_json::from_slice(&bytes).unwrap();
    let attachment_id = body["attachment"]["id"].as_str().unwrap().to_string();

    let send = app
        .clone()
        .oneshot(
            Request::post(format!("/api/bots/{}/messages", bot.id))
                .header("authorization", bearer(&token))
                .header("content-type", "application/json")
                .body(Body::from(format!(
                    r#"{{"text":"see attached","attachmentId":"{attachment_id}"}}"#
                )))
                .unwrap(),
        )
        .await
        .expect("send");
    assert_eq!(send.status(), StatusCode::OK);

    let db = store::Db::open(db_path_str).expect("reopen");
    let conv = store::get_or_create_conversation(&db, &bot.id).expect("conv");
    let messages = list_messages(&db, &conv).expect("messages");
    let user = messages
        .iter()
        .find(|m| m.role == "user")
        .expect("user row");
    assert_eq!(user.attachment_id.as_deref(), Some(attachment_id.as_str()));
}

#[tokio::test]
async fn unknown_attachment_id_returns_404() {
    let temp = tempfile::tempdir().expect("tempdir");
    let db = store::Db::open(":memory:").expect("open");
    store::set_password(&db, "owner-password-long-enough").expect("password");
    let token = store::create_session(&db).expect("session");
    let bot = create_bot(
        &db,
        BotDraft {
            name: "Riley".into(),
            purpose: "test".into(),
            instructions: String::new(),
            model: None,
        },
    )
    .expect("bot");
    let app = app_with_data(db, temp.path().to_str().unwrap());

    let send = app
        .oneshot(
            Request::post(format!("/api/bots/{}/messages", bot.id))
                .header("authorization", bearer(&token))
                .header("content-type", "application/json")
                .body(Body::from(
                    r#"{"text":"nope","attachmentId":"00000000-0000-0000-0000-000000000000"}"#,
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(send.status(), StatusCode::NOT_FOUND);
}
