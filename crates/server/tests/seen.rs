//! F14 (S1-F-11) acceptance: `POST /api/bots/:id/seen|unseen`, ported from
//! `app.ts:2366-2382`. Drives `build_app` through `tower::ServiceExt::oneshot`,
//! same seam every server test uses.
//!
//! Every route here sits behind the S1-F-05 session gate, so each test seeds
//! a password + session on its own `db` first (`common::seed_session`) and
//! carries the resulting cookie on every request.

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

async fn post(app: Router, uri: &str, cookie: &str) -> (StatusCode, Value) {
    let response = app
        .oneshot(
            Request::post(uri)
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

fn unread_of(bots: &Value, id: &str) -> u64 {
    bots.as_array()
        .expect("bots array")
        .iter()
        .find(|b| b["id"] == id)
        .unwrap_or_else(|| panic!("bot {id} not in roster response"))
        .get("unread")
        .and_then(Value::as_u64)
        .expect("unread field is a non-negative integer")
}

// An unsigned request to either route is 401 (once a password IS set -
// `require_session` checks "configured" before "signed in", so a server
// with no password at all answers 503 first, same as every other gated
// route) - `/api/bots/*/seen|unseen` are not in `auth::OPEN_PATHS`, same
// gate as every other `/api/*` route since S1-F-05.
#[tokio::test]
async fn unsigned_seen_and_unseen_requests_are_401() {
    let db = open_db();
    store::set_password(&db, "test-password").expect("seed test password");
    seed_bot(&db, "arthur", "Arthur");
    let app = app_for(db);

    let (status, _) = post(app.clone(), "/api/bots/arthur/seen", "").await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
    let (status, _) = post(app, "/api/bots/arthur/unseen", "").await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn seen_and_unseen_on_a_nonexistent_bot_is_404() {
    let db = open_db();
    let cookie = seed_session(&db);
    let app = app_for(db);

    let (status, body) = post(app.clone(), "/api/bots/nobody/seen", &cookie).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert_eq!(body["error"], "no such bot");

    let (status, body) = post(app, "/api/bots/nobody/unseen", &cookie).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert_eq!(body["error"], "no such bot");
}

// F14's core claim: opening a bot (POST .../seen) clears its unread count,
// and the menu's "Mark as Unread" (POST .../unseen) makes the last reply
// unread again. Bite: comment out `mark_bot_seen`'s `UPDATE bots SET
// last_seen_at = ...` (leaving the 200 response otherwise unchanged) and
// `unread_after_seen_is_zero` goes red - the roster it re-fetches still
// shows the pre-seen count.
#[tokio::test]
async fn mark_seen_clears_unread_then_mark_unseen_restores_it() {
    let db = open_db();
    let cookie = seed_session(&db);
    seed_bot(&db, "arthur", "Arthur");
    let conversation_id =
        store::get_or_create_conversation(&db, "arthur").expect("get_or_create_conversation");
    store::append_message(
        &db,
        &conversation_id,
        "assistant",
        "Done.",
        store::NewMessage::default(),
    )
    .expect("append message");

    let app = app_for(db);

    let (status, body) = get(app.clone(), "/api/roster", &cookie).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(
        unread_of(&body["bots"], "arthur"),
        1,
        "a fresh assistant reply with no last_seen_at is unread"
    );

    let (status, body) = post(app.clone(), "/api/bots/arthur/seen", &cookie).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(
        unread_of(&body["bots"], "arthur"),
        0,
        "the seen response's own roster already reflects the clear"
    );
    let (status, body) = get(app.clone(), "/api/roster", &cookie).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(unread_of(&body["bots"], "arthur"), 0);

    let (status, body) = post(app.clone(), "/api/bots/arthur/unseen", &cookie).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(
        unread_of(&body["bots"], "arthur"),
        1,
        "\"Mark as Unread\" backdates last_seen_at past the last reply"
    );
    let (status, body) = get(app, "/api/roster", &cookie).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(unread_of(&body["bots"], "arthur"), 1);
}

// `mark_bot_unseen` deliberately excludes room conversations from "the
// bot's own last message" (see that handler's doc comment) - a room reply
// must not touch the OWNER bot's own unread clock, since `list_roster`'s
// own unread count already excludes room messages for that same bot id.
#[tokio::test]
async fn mark_unseen_ignores_a_room_reply_and_stays_a_no_op_with_none() {
    let db = open_db();
    let cookie = seed_session(&db);
    seed_bot(&db, "arthur", "Arthur");
    seed_bot(&db, "grok", "Grok");

    // A room the bot owns, with a reply in it - and nothing in the bot's own
    // direct conversation yet.
    let room = store::create_room(&db, "Growth", &["arthur".to_string(), "grok".to_string()])
        .expect("create_room");
    store::append_message(
        &db,
        &room.id,
        "assistant",
        "From the room.",
        store::NewMessage::default(),
    )
    .expect("append room message");

    let app = app_for(db);
    let (status, body) = post(app, "/api/bots/arthur/unseen", &cookie).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(
        unread_of(&body["bots"], "arthur"),
        0,
        "a bot with no message in its OWN conversation is a no-op, even with a room reply"
    );
}
