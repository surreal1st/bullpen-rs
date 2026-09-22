//! S0-04 acceptance: build_app answers health, auth/status, auth/check and
//! roster over real HTTP via `tower::ServiceExt::oneshot`, driven against a
//! private copy of the fixture db - never the fixture itself.

use axum::Router;
use axum::body::Body;
use axum::http::{Request, StatusCode};
use chrono::{Duration, Utc};
use http_body_util::BodyExt;
use rusqlite::params;
use serde_json::Value;
use server::{AppState, build_app};
use std::fs;
use tower::ServiceExt;
use uuid::Uuid;

/// A private copy of the fixture db for one test. Never opens the fixture
/// directly - every test gets its own file so they cannot stomp each other.
fn fixture_copy(name: &str) -> store::Db {
    let unique_id = Uuid::new_v4();
    let temp = std::env::temp_dir().join(format!("bullpen_rs_first_routes_{name}_{unique_id}.db"));

    // Remove any stale WAL/SHM sidecars from previous runs
    let _ = fs::remove_file(format!("{}-wal", temp.display()));
    let _ = fs::remove_file(format!("{}-shm", temp.display()));
    let _ = fs::remove_file(&temp);

    fs::copy(store::ts_made_fixture_path(), &temp).expect("copy fixture");
    store::Db::open(temp.to_str().expect("temp path is valid utf-8")).expect("open db copy")
}

fn app_for(db: store::Db) -> Router {
    build_app(AppState::new(db))
}

async fn get(app: Router, uri: &str, cookie: Option<&str>) -> (StatusCode, Value) {
    let mut req = Request::get(uri);
    if let Some(cookie) = cookie {
        req = req.header("cookie", cookie);
    }
    let response = app.oneshot(req.body(Body::empty()).unwrap()).await.unwrap();
    let status = response.status();
    let bytes = response.into_body().collect().await.unwrap().to_bytes();
    let body = if bytes.is_empty() {
        Value::Null
    } else {
        serde_json::from_slice(&bytes).expect("response body is JSON")
    };
    (status, body)
}

fn insert_session(db: &store::Db, token: &str, expires_at: chrono::DateTime<Utc>) {
    db.conn()
        .execute(
            "INSERT INTO sessions (token, created_at, expires_at) VALUES (?1, ?2, ?3)",
            params![token, Utc::now().to_rfc3339(), expires_at.to_rfc3339()],
        )
        .expect("insert session");
}

#[tokio::test]
async fn health_reports_ok() {
    let app = app_for(fixture_copy("health"));
    let (status, body) = get(app, "/api/health", None).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["ok"], true);
}

#[tokio::test]
async fn auth_status_on_fixture_is_configured_and_signed_out() {
    let app = app_for(fixture_copy("auth_status_signed_out"));
    let (status, body) = get(app, "/api/auth/status", None).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["configured"], true, "fixture has a password row");
    assert_eq!(body["signedIn"], false);
    assert_eq!(body["role"], Value::Null);
}

#[tokio::test]
async fn valid_session_signs_in_as_owner_and_check_returns_204() {
    let db = fixture_copy("auth_status_signed_in");
    let token = "s0-04-valid-token";
    insert_session(&db, token, Utc::now() + Duration::days(1));
    let cookie = format!("bullpen_session={token}");

    let app = app_for(db);
    let (status, body) = get(app.clone(), "/api/auth/status", Some(&cookie)).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["signedIn"], true);
    assert_eq!(body["role"], "owner");

    let (status, _) = get(app, "/api/auth/check", Some(&cookie)).await;
    assert_eq!(status, StatusCode::NO_CONTENT);
}

#[tokio::test]
async fn expired_session_is_401_on_check() {
    let db = fixture_copy("auth_check_expired");
    let token = "s0-04-expired-token";
    insert_session(&db, token, Utc::now() - Duration::days(1));
    let cookie = format!("bullpen_session={token}");

    let app = app_for(db);
    let (status, _) = get(app, "/api/auth/check", Some(&cookie)).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn roster_returns_three_bots_camel_case() {
    // S1-F-05: `/api/roster` now sits behind the session gate.
    let db = fixture_copy("roster");
    let token = "s1-f-05-roster-token";
    insert_session(&db, token, Utc::now() + Duration::days(1));
    let cookie = format!("bullpen_session={token}");

    let app = app_for(db);
    let (status, body) = get(app, "/api/roster", Some(&cookie)).await;
    assert_eq!(status, StatusCode::OK);

    let bots = body["bots"].as_array().expect("bots array");
    assert_eq!(bots.len(), 3);
    for bot in bots {
        let obj = bot.as_object().expect("bot is an object");
        assert!(
            obj.contains_key("sectionId"),
            "expected camelCase sectionId"
        );
        assert!(
            obj.contains_key("hasRoutine"),
            "expected camelCase hasRoutine"
        );
        assert!(
            !obj.contains_key("section_id"),
            "should not have snake_case section_id"
        );
    }
}
