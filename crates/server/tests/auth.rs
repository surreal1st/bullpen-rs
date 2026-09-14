//! S1-F-05 acceptance: the `/api/*` session gate (B7/F1), the login/logout
//! routes it needs to be usable at all (F18), and thread ownership (B14).
//! Drives `build_app` through `tower::ServiceExt::oneshot`, same seam every
//! server test uses.

mod common;

use axum::Router;
use axum::body::Body;
use axum::http::{Request, StatusCode};
use common::seed_session;
use http_body_util::BodyExt;
use serde_json::{Value, json};
use server::{AppState, build_app};
use store::Db;
use tower::ServiceExt;

fn open_db() -> Db {
    Db::open(":memory:").expect("open :memory: db")
}

fn seed_bot(db: &Db, id: &str, name: &str) {
    db.conn()
        .execute(
            "INSERT INTO bots (id, name, purpose, instructions, model, created_at) VALUES (?1, ?2, '', ?3, NULL, '2026-01-01T00:00:00Z')",
            rusqlite::params![id, name, format!("You are {name}.")],
        )
        .expect("seed bot");
}

fn app_for(db: Db) -> Router {
    build_app(AppState::new(db))
}

async fn send(req: Request<Body>, app: Router) -> (StatusCode, Value) {
    let resp = app.oneshot(req).await.expect("oneshot");
    let status = resp.status();
    let bytes = resp
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

fn get_req(uri: &str, cookie: Option<&str>) -> Request<Body> {
    let mut builder = Request::get(uri);
    if let Some(cookie) = cookie {
        builder = builder.header("cookie", cookie);
    }
    builder.body(Body::empty()).expect("build request")
}

fn post_req(uri: &str, body: Value, cookie: Option<&str>) -> Request<Body> {
    let mut builder = Request::post(uri).header("content-type", "application/json");
    if let Some(cookie) = cookie {
        builder = builder.header("cookie", cookie);
    }
    builder
        .body(Body::from(body.to_string()))
        .expect("build request")
}

// 1. B7/F1: no session at all on a protected route -> 401 "Sign in to
//    Bullpen.", never reaching the handler (so no run starts, no OpenRouter
//    key gets spent).
#[tokio::test]
async fn unsigned_request_to_a_protected_route_is_401() {
    let db = open_db();
    store::set_password(&db, "correct horse").expect("set password");
    seed_bot(&db, "arthur", "Arthur");
    let app = app_for(db);

    let (status, body) = send(
        post_req("/api/bots/arthur/messages", json!({"text": "hi"}), None),
        app,
    )
    .await;

    assert_eq!(status, StatusCode::UNAUTHORIZED);
    assert_eq!(body["error"], "Sign in to Bullpen.");
}

// F1: an unconfigured server (no password ever set) refuses a protected
// route with 503, not a silent pass-through - a fresh deploy is exactly
// when it is most exposed.
#[tokio::test]
async fn unconfigured_server_refuses_a_protected_route_with_503() {
    let db = open_db();
    seed_bot(&db, "arthur", "Arthur");
    let app = app_for(db);

    let (status, body) = send(get_req("/api/roster", None), app).await;

    assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
    assert_eq!(body["error"], "This Bullpen has no password set yet.");
    assert_eq!(body["setup"], true);
}

// 2. A valid session proceeds past the gate to the real handler.
#[tokio::test]
async fn valid_session_proceeds_past_the_gate() {
    let db = open_db();
    let cookie = seed_session(&db);
    seed_bot(&db, "arthur", "Arthur");
    let app = app_for(db);

    let (status, body) = send(get_req("/api/roster", Some(&cookie)), app).await;

    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["bots"].as_array().expect("bots array").len(), 1);
}

// 3. Open paths (health, auth/status, auth/login) never require a session,
// even on an unconfigured server - the gate itself must not lock out the
// one path that lets Josh set a password in the first place.
#[tokio::test]
async fn open_paths_never_require_a_session() {
    let app = app_for(open_db());
    let (status, _) = send(get_req("/api/health", None), app).await;
    assert_eq!(status, StatusCode::OK);
}

// 4. B14: `threadId` from the body must belong to the posting bot (or be a
// room it is in) - a foreign thread id is a 404, indistinguishable from a
// thread that does not exist at all.
#[tokio::test]
async fn posting_into_another_bots_thread_is_404() {
    let db = open_db();
    let cookie = seed_session(&db);
    seed_bot(&db, "arthur", "Arthur");
    seed_bot(&db, "riley", "Riley");
    let rileys_thread = store::get_or_create_conversation(&db, "riley").expect("riley's thread");
    let app = app_for(db);

    let (status, body) = send(
        post_req(
            "/api/bots/arthur/messages",
            json!({"text": "hi", "threadId": rileys_thread}),
            Some(&cookie),
        ),
        app,
    )
    .await;

    assert_eq!(status, StatusCode::NOT_FOUND);
    assert_eq!(body["error"], "no such thread");
}

// B14: a thread id that names nothing at all gets the SAME 404 - a probe
// cannot tell "wrong owner" from "no such thread".
#[tokio::test]
async fn posting_into_a_nonexistent_thread_is_404() {
    let db = open_db();
    let cookie = seed_session(&db);
    seed_bot(&db, "arthur", "Arthur");
    let app = app_for(db);

    let (status, body) = send(
        post_req(
            "/api/bots/arthur/messages",
            json!({"text": "hi", "threadId": "no-such-thread-id"}),
            Some(&cookie),
        ),
        app,
    )
    .await;

    assert_eq!(status, StatusCode::NOT_FOUND);
    assert_eq!(body["error"], "no such thread");
}

// 5. F18: `POST /api/auth/login` with the right password sets a cookie and
// returns a token; that cookie then passes the gate on its own.
#[tokio::test]
async fn login_with_the_right_password_signs_in() {
    let db = open_db();
    store::set_password(&db, "correct horse battery staple").expect("set password");
    seed_bot(&db, "arthur", "Arthur");
    let app = app_for(db);

    let resp = app
        .clone()
        .oneshot(post_req(
            "/api/auth/login",
            json!({"password": "correct horse battery staple"}),
            None,
        ))
        .await
        .expect("oneshot");
    assert_eq!(resp.status(), StatusCode::OK);
    let set_cookie = resp
        .headers()
        .get("set-cookie")
        .expect("set-cookie header")
        .to_str()
        .expect("set-cookie is valid utf-8")
        .to_string();
    assert!(set_cookie.starts_with("bullpen_session="));
    assert!(set_cookie.contains("HttpOnly"));
    let bytes = resp
        .into_body()
        .collect()
        .await
        .expect("collect")
        .to_bytes();
    let body: Value = serde_json::from_slice(&bytes).expect("json body");
    assert_eq!(body["ok"], true);
    assert!(body["token"].as_str().is_some_and(|t| !t.is_empty()));

    // The cookie the login route just set gets past the gate on its own.
    let cookie = set_cookie
        .split(';')
        .next()
        .expect("cookie has at least one part")
        .to_string();
    let (status, _) = send(get_req("/api/roster", Some(&cookie)), app).await;
    assert_eq!(status, StatusCode::OK);
}

// F18: the wrong password is a 401 with the same message login always uses,
// and never sets a cookie.
#[tokio::test]
async fn login_with_the_wrong_password_is_401() {
    let db = open_db();
    store::set_password(&db, "correct horse battery staple").expect("set password");
    let app = app_for(db);

    let resp = app
        .oneshot(post_req(
            "/api/auth/login",
            json!({"password": "wrong"}),
            None,
        ))
        .await
        .expect("oneshot");
    assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
    assert!(resp.headers().get("set-cookie").is_none());
    let bytes = resp
        .into_body()
        .collect()
        .await
        .expect("collect")
        .to_bytes();
    let body: Value = serde_json::from_slice(&bytes).expect("json body");
    assert_eq!(body["error"], "That password is not right.");
}

// F18: no password ever set -> login answers 503, the same body the gate
// itself uses for an unconfigured server.
#[tokio::test]
async fn login_before_any_password_is_set_is_503() {
    let app = app_for(open_db());

    let (status, body) = send(
        post_req("/api/auth/login", json!({"password": "anything"}), None),
        app,
    )
    .await;

    assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
    assert_eq!(body["error"], "This Bullpen has no password set yet.");
    assert_eq!(body["setup"], true);
}

// F18: ten wrong passwords in the throttle window -> the eleventh answers
// 429 without even checking the password, per `LoginThrottle`'s
// `ATTEMPT_LIMIT`.
#[tokio::test]
async fn ten_failed_logins_throttle_the_eleventh() {
    let db = open_db();
    store::set_password(&db, "correct horse battery staple").expect("set password");
    let app = app_for(db);

    for _ in 0..10 {
        let (status, _) = send(
            post_req("/api/auth/login", json!({"password": "wrong"}), None),
            app.clone(),
        )
        .await;
        assert_eq!(status, StatusCode::UNAUTHORIZED);
    }

    let (status, body) = send(
        post_req("/api/auth/login", json!({"password": "wrong"}), None),
        app,
    )
    .await;
    assert_eq!(status, StatusCode::TOO_MANY_REQUESTS);
    assert_eq!(body["error"], "Too many attempts. Wait a few minutes.");
}

// 6. F18: logout clears the cookie and destroys the session server-side -
// the SAME token is rejected by the gate afterward, not just the client's
// copy of the cookie.
#[tokio::test]
async fn logout_destroys_the_session_not_just_the_cookie() {
    let db = open_db();
    let cookie = seed_session(&db);
    let app = app_for(db);

    let (status, _) = send(get_req("/api/roster", Some(&cookie)), app.clone()).await;
    assert_eq!(status, StatusCode::OK, "session should start out valid");

    let resp = app
        .clone()
        .oneshot(post_req("/api/auth/logout", json!({}), Some(&cookie)))
        .await
        .expect("oneshot");
    assert_eq!(resp.status(), StatusCode::OK);
    let set_cookie = resp
        .headers()
        .get("set-cookie")
        .expect("set-cookie header")
        .to_str()
        .expect("valid utf-8")
        .to_string();
    assert!(set_cookie.contains("Max-Age=0"), "got: {set_cookie}");

    let (status, body) = send(get_req("/api/roster", Some(&cookie)), app).await;
    assert_eq!(
        status,
        StatusCode::UNAUTHORIZED,
        "the destroyed session must not still work"
    );
    assert_eq!(body["error"], "Sign in to Bullpen.");
}

// F18: logout with no session at all still succeeds - clearing a stale or
// absent cookie must never itself require being signed in.
#[tokio::test]
async fn logout_with_no_session_still_succeeds() {
    let app = app_for(open_db());
    let (status, body) = send(post_req("/api/auth/logout", json!({}), None), app).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["ok"], true);
}
