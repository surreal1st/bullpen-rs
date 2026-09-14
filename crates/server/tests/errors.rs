//! S1-F-03 acceptance: errors are responses, not panics.
//!
//! 1. B1: a preview line with a stray `]` before its `[` used to slice an
//!    out-of-order byte range and panic `first_line` - `/api/roster` must
//!    stay 200.
//! 2. B20: a malformed JSON body must not silently become the type's
//!    default (`unwrap_or_default()`) - it gets a 400 naming the fault.
//! 3. B2: a poisoned db mutex (a panic under the lock, same as B1/B11/B12
//!    used to cause) must not fail every request after the first - the
//!    guard recovers with `PoisonError::into_inner`.
//!
//! Drives `build_app` through `tower::ServiceExt::oneshot`, same seam every
//! server test uses - never `RunManager`/`AppState` internals directly.
//!
//! S1-F-05: every route here now sits behind the session gate, so each test
//! seeds a password + session on its own `db` first (`common::seed_session`)
//! and carries the resulting cookie on every request.

mod common;

use axum::Router;
use axum::body::Body;
use axum::http::{Request, StatusCode};
use common::seed_session;
use http_body_util::BodyExt;
use serde_json::Value;
use server::{AppState, build_app};
use tower::ServiceExt;

fn open_db() -> store::Db {
    store::Db::open(":memory:").expect("open :memory: db")
}

fn seed_bot(db: &store::Db, id: &str, name: &str) {
    db.conn()
        .execute(
            "INSERT INTO bots (id, name, purpose, instructions, model, created_at) VALUES (?1, ?2, '', ?3, NULL, '2026-01-01T00:00:00Z')",
            rusqlite::params![id, name, format!("You are {name}.")],
        )
        .expect("seed bot");
}

fn app_for(db: store::Db) -> Router {
    build_app(AppState::new(db))
}

async fn get(app: Router, uri: &str, cookie: &str) -> (StatusCode, Value) {
    let response = app
        .oneshot(
            Request::get(uri)
                .header("cookie", cookie)
                .body(Body::empty())
                .expect("build request"),
        )
        .await
        .expect("oneshot");
    let status = response.status();
    let bytes = response
        .into_body()
        .collect()
        .await
        .expect("collect")
        .to_bytes();
    let body = if bytes.is_empty() {
        Value::Null
    } else {
        serde_json::from_slice(&bytes).expect("response body is JSON")
    };
    (status, body)
}

// 1. B1: a bracket before its link in a preview line must not 500 the
// roster. Bite: revert the `if end <= start { break; }` guard in
// `store::roster::first_line` and this goes red (panics inside the request,
// which axum turns into a connection error `oneshot` reports as an error
// rather than a clean response).
#[tokio::test]
async fn a_stray_bracket_before_a_link_in_a_preview_does_not_500_the_roster() {
    let db = open_db();
    let cookie = seed_session(&db);
    seed_bot(&db, "arthur", "Arthur");
    let conversation_id =
        store::get_or_create_conversation(&db, "arthur").expect("get_or_create_conversation");
    store::append_message(
        &db,
        &conversation_id,
        "assistant",
        "Done] see [docs](https://x)",
        store::NewMessage::default(),
    )
    .expect("append message");

    let app = app_for(db);
    let (status, body) = get(app, "/api/roster", &cookie).await;
    assert_eq!(status, StatusCode::OK);
    let bots = body["bots"].as_array().expect("bots array");
    assert_eq!(bots.len(), 1);
}

// 2. B20: malformed JSON to `POST /api/rooms` is a 400 naming the real
// fault, not a silently-defaulted room.
#[tokio::test]
async fn malformed_json_body_to_create_room_is_400_with_a_parse_error() {
    let db = open_db();
    let cookie = seed_session(&db);
    let app = app_for(db);
    let req = Request::post("/api/rooms")
        .header("content-type", "application/json")
        .header("cookie", &cookie)
        .body(Body::from("{not valid json"))
        .expect("build request");
    let resp = app.oneshot(req).await.expect("oneshot");
    let status = resp.status();
    let bytes = resp
        .into_body()
        .collect()
        .await
        .expect("collect")
        .to_bytes();
    let body: Value = serde_json::from_slice(&bytes).expect("response body is JSON");

    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(body["error"], "invalid JSON body");
}

// 3. B2: a panic under the db lock (the debug-only `/api/__test/poison`
// route) poisons the `Mutex` - the NEXT request must still succeed instead
// of every later `lock()` panicking too.
#[tokio::test]
async fn a_poisoned_db_mutex_recovers_on_the_next_request() {
    let db = open_db();
    let cookie = seed_session(&db);
    let app = app_for(db);

    let poisoning = app.clone();
    let poison_cookie = cookie.clone();
    let joined = tokio::spawn(async move {
        poisoning
            .oneshot(
                Request::post("/api/__test/poison")
                    .header("cookie", poison_cookie)
                    .body(Body::empty())
                    .expect("build request"),
            )
            .await
    })
    .await;
    assert!(
        joined.is_err(),
        "expected the poison route to panic the spawned task"
    );

    let (status, _) = get(app, "/api/roster", &cookie).await;
    assert_eq!(
        status,
        StatusCode::OK,
        "the db mutex should have recovered from the poison"
    );
}
