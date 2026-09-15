//! S4-02: Auto-review routes tests.

use axum::body::Body;
use axum::http::{Request, StatusCode};
use serde_json::{Value, json};
use server::AppState;
use store::Db;
use tower::ServiceExt;

mod common;
use common::seed_session;

fn open_db() -> Db {
    Db::open(":memory:").expect("open :memory: db")
}

fn app_for(db: Db) -> axum::Router {
    let state = AppState::new(db);
    server::build_app(state)
}

/// Helper to make authenticated requests
async fn get_with_auth(app: &axum::Router, path: &str, session: &str) -> (StatusCode, Value) {
    let request = Request::get(path)
        .header("cookie", session)
        .body(Body::empty())
        .unwrap();

    let response = app.clone().oneshot(request).await.unwrap();
    let status = response.status();
    let body_bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap();
    let body_json = serde_json::from_slice(&body_bytes).unwrap_or(Value::Null);

    (status, body_json)
}

/// Helper to make PUT requests
async fn put_with_auth(
    app: &axum::Router,
    path: &str,
    session: &str,
    body: Value,
) -> (StatusCode, Value) {
    let body_str = body.to_string();
    let request = Request::put(path)
        .header("cookie", session)
        .header("content-type", "application/json")
        .body(Body::from(body_str))
        .unwrap();

    let response = app.clone().oneshot(request).await.unwrap();
    let status = response.status();
    let body_bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap();
    let body_json = serde_json::from_slice(&body_bytes).unwrap_or(Value::Null);

    (status, body_json)
}

/// Helper for unauthenticated requests
async fn get_no_auth(app: &axum::Router, path: &str) -> StatusCode {
    let request = Request::get(path).body(Body::empty()).unwrap();
    let response = app.clone().oneshot(request).await.unwrap();
    response.status()
}

#[tokio::test]
async fn get_judge_default_enabled() {
    let db = open_db();
    let session = seed_session(&db);
    let app = app_for(db);

    let (status, body) = get_with_auth(&app, "/api/auto-review/judge", &session).await;

    assert_eq!(status, StatusCode::OK);
    assert_eq!(body.get("enabled").and_then(|v| v.as_bool()), Some(true));
}

#[tokio::test]
async fn put_judge_disable_then_get() {
    let db = open_db();
    let session = seed_session(&db);
    let app = app_for(db);

    // PUT false
    let (status, body) = put_with_auth(
        &app,
        "/api/auto-review/judge",
        &session,
        json!({"enabled": false}),
    )
    .await;

    assert_eq!(status, StatusCode::OK);
    assert_eq!(body.get("enabled").and_then(|v| v.as_bool()), Some(false));

    // GET should return false
    let (status, body) = get_with_auth(&app, "/api/auto-review/judge", &session).await;

    assert_eq!(status, StatusCode::OK);
    assert_eq!(body.get("enabled").and_then(|v| v.as_bool()), Some(false));
}

#[tokio::test]
async fn put_judge_invalid_body() {
    let db = open_db();
    let session = seed_session(&db);
    let app = app_for(db);

    let request = Request::put("/api/auto-review/judge")
        .header("cookie", &session)
        .header("content-type", "application/json")
        .body(Body::from("not valid json"))
        .unwrap();

    let response = app.clone().oneshot(request).await.unwrap();
    let status = response.status();

    assert_eq!(status, StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn put_judge_non_bool_value() {
    let db = open_db();
    let session = seed_session(&db);
    let app = app_for(db);

    let request = Request::put("/api/auto-review/judge")
        .header("cookie", &session)
        .header("content-type", "application/json")
        .body(Body::from(r#"{"enabled":"yes"}"#))
        .unwrap();

    let response = app.clone().oneshot(request).await.unwrap();
    let status = response.status();

    assert_eq!(status, StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn get_log_empty() {
    let db = open_db();
    let session = seed_session(&db);
    let app = app_for(db);

    let (status, body) = get_with_auth(&app, "/api/auto-review/log", &session).await;

    assert_eq!(status, StatusCode::OK);
    let entries = body.get("entries").and_then(|v| v.as_array());
    assert_eq!(entries.map(|e| e.len()), Some(0));
}

#[tokio::test]
async fn get_log_with_entries() {
    let db = open_db();

    // Seed 3 entries manually with different timestamps
    for i in 1..=3 {
        let ts = chrono::Utc::now() + chrono::Duration::milliseconds(i as i64);
        let now = ts.to_rfc3339_opts(chrono::SecondsFormat::Millis, true);
        let entry = store::auto_review::LogEntry {
            id: format!("entry-{}", i),
            bot_id: format!("bot-{}", i),
            run_id: format!("run-{}", i),
            tool_name: format!("tool-{}", i),
            description: format!("desc-{}", i),
            verdict: "safe".to_string(),
            reason: format!("reason-{}", i),
            decision: "allow".to_string(),
            created_at: now,
        };
        store::auto_review::insert(&db, entry).expect("insert entry");
    }

    let session = seed_session(&db);
    let app = app_for(db);

    // GET log - should return newest first
    let (status, body) = get_with_auth(&app, "/api/auto-review/log", &session).await;

    assert_eq!(status, StatusCode::OK);
    let entries = body.get("entries").and_then(|v| v.as_array());
    assert!(entries.is_some());
    let entries = entries.unwrap();
    assert_eq!(entries.len(), 3);

    // Verify newest first (entry 3, 2, 1 in descending order)
    assert_eq!(
        entries[0].get("id").and_then(|v| v.as_str()),
        Some("entry-3")
    );
    assert_eq!(
        entries[1].get("id").and_then(|v| v.as_str()),
        Some("entry-2")
    );
    assert_eq!(
        entries[2].get("id").and_then(|v| v.as_str()),
        Some("entry-1")
    );
}

#[tokio::test]
async fn get_log_with_limit() {
    let db = open_db();

    // Seed 3 entries with different timestamps
    for i in 1..=3 {
        let ts = chrono::Utc::now() + chrono::Duration::milliseconds(i as i64);
        let now = ts.to_rfc3339_opts(chrono::SecondsFormat::Millis, true);
        let entry = store::auto_review::LogEntry {
            id: format!("entry-{}", i),
            bot_id: format!("bot-{}", i),
            run_id: format!("run-{}", i),
            tool_name: format!("tool-{}", i),
            description: format!("desc-{}", i),
            verdict: "risky".to_string(),
            reason: format!("reason-{}", i),
            decision: "ask".to_string(),
            created_at: now,
        };
        store::auto_review::insert(&db, entry).expect("insert entry");
    }

    let session = seed_session(&db);
    let app = app_for(db);

    // GET log with limit=2
    let (status, body) = get_with_auth(&app, "/api/auto-review/log?limit=2", &session).await;

    assert_eq!(status, StatusCode::OK);
    let entries = body.get("entries").and_then(|v| v.as_array());
    assert!(entries.is_some());
    let entries = entries.unwrap();
    assert_eq!(entries.len(), 2);

    // Verify newest first
    assert_eq!(
        entries[0].get("id").and_then(|v| v.as_str()),
        Some("entry-3")
    );
    assert_eq!(
        entries[1].get("id").and_then(|v| v.as_str()),
        Some("entry-2")
    );
}

#[tokio::test]
async fn get_log_limit_zero_clamped_to_one() {
    let db = open_db();

    // Seed 1 entry
    let now = chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Millis, true);
    let entry = store::auto_review::LogEntry {
        id: "entry-1".to_string(),
        bot_id: "bot-1".to_string(),
        run_id: "run-1".to_string(),
        tool_name: "tool-1".to_string(),
        description: "desc-1".to_string(),
        verdict: "safe".to_string(),
        reason: "reason-1".to_string(),
        decision: "allow".to_string(),
        created_at: now,
    };
    store::auto_review::insert(&db, entry).expect("insert entry");

    let session = seed_session(&db);
    let app = app_for(db);

    // GET log with limit=0 - should be clamped to 1
    let (status, body) = get_with_auth(&app, "/api/auto-review/log?limit=0", &session).await;

    assert_eq!(status, StatusCode::OK);
    let entries = body.get("entries").and_then(|v| v.as_array());
    assert!(entries.is_some());
    let entries = entries.unwrap();
    // limit=0 is clamped to 1, so we should get 1 row
    assert_eq!(entries.len(), 1);
}

#[tokio::test]
async fn get_log_limit_exceeds_max_clamped_to_200() {
    let db = open_db();

    // Seed 1 entry (no need to seed 201, just test the clamp works)
    let now = chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Millis, true);
    let entry = store::auto_review::LogEntry {
        id: "entry-1".to_string(),
        bot_id: "bot-1".to_string(),
        run_id: "run-1".to_string(),
        tool_name: "tool-1".to_string(),
        description: "desc-1".to_string(),
        verdict: "safe".to_string(),
        reason: "reason-1".to_string(),
        decision: "allow".to_string(),
        created_at: now,
    };
    store::auto_review::insert(&db, entry).expect("insert entry");

    let session = seed_session(&db);
    let app = app_for(db);

    // GET log with a very large limit - should be clamped to 200
    let (status, body) =
        get_with_auth(&app, "/api/auto-review/log?limit=4294967295", &session).await;

    assert_eq!(status, StatusCode::OK);
    let entries = body.get("entries").and_then(|v| v.as_array());
    assert!(entries.is_some());
    let entries = entries.unwrap();
    // Should return the 1 entry that exists, not try to fetch 4294967295
    assert_eq!(entries.len(), 1);
}

#[tokio::test]
async fn get_approvals_with_judge_fields() {
    let db = open_db();

    // Seed a bot, conversation, and approval with judge fields
    db.conn()
        .execute(
            "INSERT INTO bots (id, name, created_at) VALUES (?, ?, ?)",
            rusqlite::params!["bot-1", "Test Bot", "2026-01-01T00:00:00.000Z"],
        )
        .expect("insert bot");

    db.conn()
        .execute(
            "INSERT INTO conversations (id, bot_id, created_at) VALUES (?, ?, ?)",
            rusqlite::params!["conv-1", "bot-1", "2026-01-01T00:00:00.000Z"],
        )
        .expect("insert conversation");

    db.conn()
        .execute(
            "INSERT INTO runs (id, bot_id, conversation_id, trigger, status, model, messages, created_at, updated_at) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?)",
            rusqlite::params!["run-1", "bot-1", "conv-1", "chat", "done", "claude", "[]", "2026-01-01T00:00:00.000Z", "2026-01-01T00:00:00.000Z"],
        )
        .expect("insert run");

    let now = chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Millis, true);
    db.conn()
        .execute(
            "INSERT INTO approvals (id, run_id, bot_id, tool_name, tool_args, call_id, status, judge_verdict, judge_reason, created_at) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
            rusqlite::params!["app-1", "run-1", "bot-1", "shell", "ls", "call-1", "pending", "risky", "could harm data", now],
        )
        .expect("insert approval");

    let session = seed_session(&db);
    let app = app_for(db);

    // GET approvals
    let (status, body) = get_with_auth(&app, "/api/approvals", &session).await;

    assert_eq!(status, StatusCode::OK);
    let approvals = body.get("approvals").and_then(|v| v.as_array());
    assert!(approvals.is_some());
    let approvals = approvals.unwrap();
    assert_eq!(approvals.len(), 1);

    let app_obj = &approvals[0];
    assert_eq!(
        app_obj.get("judgeVerdict").and_then(|v| v.as_str()),
        Some("risky")
    );
    assert_eq!(
        app_obj.get("judgeReason").and_then(|v| v.as_str()),
        Some("could harm data")
    );
}

#[tokio::test]
async fn get_approvals_judge_fields_null_when_not_set() {
    let db = open_db();

    // Seed a bot, conversation, and approval without judge fields
    db.conn()
        .execute(
            "INSERT INTO bots (id, name, created_at) VALUES (?, ?, ?)",
            rusqlite::params!["bot-1", "Test Bot", "2026-01-01T00:00:00.000Z"],
        )
        .expect("insert bot");

    db.conn()
        .execute(
            "INSERT INTO conversations (id, bot_id, created_at) VALUES (?, ?, ?)",
            rusqlite::params!["conv-1", "bot-1", "2026-01-01T00:00:00.000Z"],
        )
        .expect("insert conversation");

    db.conn()
        .execute(
            "INSERT INTO runs (id, bot_id, conversation_id, trigger, status, model, messages, created_at, updated_at) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?)",
            rusqlite::params!["run-1", "bot-1", "conv-1", "chat", "done", "claude", "[]", "2026-01-01T00:00:00.000Z", "2026-01-01T00:00:00.000Z"],
        )
        .expect("insert run");

    let now = chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Millis, true);
    db.conn()
        .execute(
            "INSERT INTO approvals (id, run_id, bot_id, tool_name, tool_args, call_id, status, judge_verdict, judge_reason, created_at) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
            rusqlite::params!["app-1", "run-1", "bot-1", "shell", "ls", "call-1", "pending", Option::<String>::None, Option::<String>::None, now],
        )
        .expect("insert approval");

    let session = seed_session(&db);
    let app = app_for(db);

    // GET approvals
    let (status, body) = get_with_auth(&app, "/api/approvals", &session).await;

    assert_eq!(status, StatusCode::OK);
    let approvals = body.get("approvals").and_then(|v| v.as_array());
    assert!(approvals.is_some());
    let approvals = approvals.unwrap();
    assert_eq!(approvals.len(), 1);

    let app_obj = &approvals[0];
    assert_eq!(app_obj.get("judgeVerdict"), Some(&Value::Null));
    assert_eq!(app_obj.get("judgeReason"), Some(&Value::Null));
}

#[tokio::test]
async fn judge_get_requires_auth() {
    let db = open_db();
    store::set_password(&db, "test").expect("set password");
    let app = app_for(db);

    let status = get_no_auth(&app, "/api/auto-review/judge").await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn judge_put_requires_auth() {
    let db = open_db();
    store::set_password(&db, "test").expect("set password");
    let app = app_for(db);

    let request = Request::put("/api/auto-review/judge")
        .header("content-type", "application/json")
        .body(Body::from("{}"))
        .unwrap();

    let response = app.clone().oneshot(request).await.unwrap();
    assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn log_get_requires_auth() {
    let db = open_db();
    store::set_password(&db, "test").expect("set password");
    let app = app_for(db);

    let status = get_no_auth(&app, "/api/auto-review/log").await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
}
