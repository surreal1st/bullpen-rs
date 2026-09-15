//! S2-05/S2-F-04: spend month-to-date and ceiling gate tests.

mod common;

use std::sync::Arc;

use axum::Router;
use axum::body::{Body, Bytes};
use axum::http::{Request, StatusCode};
use common::{as_port, seed_session};
use http_body_util::BodyExt;
use model::ModelPort;
use serde_json::{Value, json};
use server::{AppState, build_app};
use store::Db;
use tower::ServiceExt;

use server::spend;

/// S2-04: routing defaults to enabled; disabled here so a scripted run's
/// only reply is never consumed by the classifier call instead of the
/// turn it scripted it for - same reasoning every other test-db helper in
/// this suite gives.
fn open_db() -> Db {
    let db = Db::open(":memory:").expect("open :memory: db");
    model::routing::set_routing_settings(&db, Some(false), None)
        .expect("disable routing classifier for scripted-model tests");
    server::judge::set_judge_enabled(&db, false).expect("disable judge for scripted-model tests");
    db
}

fn seed_bot(db: &Db, id: &str, name: &str) {
    db.conn()
        .execute(
            "INSERT INTO bots (id, name, purpose, instructions, model, created_at) VALUES (?1, ?2, '', ?3, NULL, '2026-01-01T00:00:00Z')",
            rusqlite::params![id, name, format!("You are {name}.")],
        )
        .expect("seed bot");
}

/// Seeds one provider-reported assistant message with an explicit
/// `created_at`, so a test can put spend into a specific calendar month -
/// `store::append_message` always stamps `now()`, so this goes around it
/// with the same INSERT shape (`crates/store/src/messages.rs`).
fn seed_assistant_message(db: &Db, bot_id: &str, created_at: &str, cost_usd: f64) {
    let conversation_id =
        store::get_or_create_conversation(db, bot_id).expect("get_or_create_conversation");
    let seq: i64 = db
        .conn()
        .query_row(
            "SELECT COALESCE(MAX(seq), 0) + 1 FROM messages WHERE conversation_id = ?1",
            rusqlite::params![conversation_id],
            |row| row.get(0),
        )
        .expect("next seq");
    let id = format!("{bot_id}-{created_at}");
    db.conn()
        .execute(
            "INSERT INTO messages (id, conversation_id, seq, role, content, model, error, created_at,
                                    attachment_id, bot_id, cost_usd, input_tokens, output_tokens, cached_tokens)
             VALUES (?1, ?2, ?3, 'assistant', 'done', NULL, NULL, ?4, NULL, NULL, ?5, 0, 0, 0)",
            rusqlite::params![id, conversation_id, seq, created_at, cost_usd],
        )
        .expect("seed assistant message");
}

fn post_req(uri: &str, body: Value, cookie: &str) -> Request<Body> {
    Request::post(uri)
        .header("content-type", "application/json")
        .header("cookie", cookie)
        .body(Body::from(body.to_string()))
        .expect("build request")
}

fn get_req(uri: &str, cookie: &str) -> Request<Body> {
    Request::get(uri)
        .header("cookie", cookie)
        .body(Body::empty())
        .expect("build request")
}

async fn get_json(app: Router, uri: &str, cookie: &str) -> (StatusCode, Value) {
    let resp = app.oneshot(get_req(uri, cookie)).await.expect("oneshot");
    let status = resp.status();
    let bytes = resp
        .into_body()
        .collect()
        .await
        .expect("collect")
        .to_bytes();
    let json = if bytes.is_empty() {
        Value::Null
    } else {
        serde_json::from_slice(&bytes).expect("response body is JSON")
    };
    (status, json)
}

/// Reads one SSE frame's `data:` line as JSON, same helper every other
/// run-driving test file (`tests/rooms.rs`) uses against `build_app`'s
/// stream.
async fn read_next_frame(
    stream: &mut (impl futures::Stream<Item = Result<Bytes, axum::Error>> + Unpin),
    buf: &mut String,
) -> Option<Value> {
    loop {
        if let Some(pos) = buf.find("\n\n") {
            let event_text: String = buf.drain(..pos + 2).collect();
            let data_line = event_text.lines().find(|l| l.starts_with("data:"))?;
            return serde_json::from_str(data_line["data:".len()..].trim()).ok();
        }
        let chunk = futures::StreamExt::next(stream).await?.ok()?;
        buf.push_str(&String::from_utf8_lossy(&chunk));
    }
}

#[test]
fn spend_query_works_on_empty_db() {
    let db = store::Db::open(":memory:").expect("open test db");

    // When there are no messages, query should return empty
    let now = chrono::Utc::now();
    let month = spend::current_month(now);
    let bot_spends = spend::spend_by_bot(&db, &month).expect("query spend by bot");

    assert_eq!(bot_spends.len(), 0);
}

#[test]
fn put_ceiling_sets_and_returns_the_value() {
    let db = store::Db::open(":memory:").expect("open test db");

    // Default ceiling
    let default = spend::get_ceiling(&db);
    assert_eq!(default, 10.0);

    // Set new ceiling
    let new_val = spend::set_ceiling(&db, 25.5).expect("set ceiling");
    assert_eq!(new_val, 25.5);

    // Verify it persists
    let stored = spend::get_ceiling(&db);
    assert_eq!(stored, 25.5);
}

#[test]
fn put_ceiling_cleans_negative_to_zero() {
    let db = store::Db::open(":memory:").expect("open test db");

    let result = spend::set_ceiling(&db, -5.0).expect("set ceiling");
    assert_eq!(result, 0.0);
    assert_eq!(spend::get_ceiling(&db), 0.0);
}

#[test]
fn gate_run_denies_when_over_ceiling() {
    let ceiling = 10.0;
    let db = store::Db::open(":memory:").expect("open test db");
    let result = spend::gate_run(&db, ceiling, Some(11.0));

    match result {
        spend::GateResult::Denied { reason } => {
            assert!(reason.contains("Spend ceiling reached"));
            assert!(reason.contains("$11.00"));
            assert!(reason.contains("$10.00"));
        }
        _ => panic!("expected Denied, got Allowed"),
    }
}

#[test]
fn gate_run_allows_when_under_ceiling() {
    let ceiling = 10.0;
    let db = store::Db::open(":memory:").expect("open test db");
    let result = spend::gate_run(&db, ceiling, Some(5.0));

    match result {
        spend::GateResult::Allowed { warning } => {
            assert!(warning.is_none());
        }
        _ => panic!("expected Allowed with no warning, got {:?}", result),
    }
}

#[test]
fn gate_run_warns_at_15_percent_headroom() {
    let ceiling = 100.0;
    let db = store::Db::open(":memory:").expect("open test db");
    // At 85% used: 15% headroom, should warn
    let result = spend::gate_run(&db, ceiling, Some(85.0));

    match result {
        spend::GateResult::Allowed { warning } => {
            assert!(warning.is_some());
            let w = warning.unwrap();
            assert!(w.contains("$15.00"));
            assert!(w.contains("$100.00"));
            assert!(w.contains("ceiling stops"));
        }
        _ => panic!("expected Allowed with warning, got {:?}", result),
    }
}

#[test]
fn gate_run_allows_on_network_failure() {
    let ceiling = 10.0;
    let db = store::Db::open(":memory:").expect("open test db");
    // None = network read failed
    let result = spend::gate_run(&db, ceiling, None);

    match result {
        spend::GateResult::Allowed { warning } => {
            assert!(warning.is_some());
            let w = warning.unwrap();
            assert!(w.contains("Could not read"));
        }
        _ => panic!("expected Allowed with warning, got {:?}", result),
    }
}

#[test]
fn current_month_formats_correctly() {
    let dt = chrono::DateTime::parse_from_rfc3339("2026-09-14T12:00:00Z")
        .unwrap()
        .with_timezone(&chrono::Utc);

    let month = spend::current_month(dt);
    assert_eq!(month, "2026-09");
}

#[test]
fn ceiling_gate_denies_posts_when_at_ceiling() {
    let db = store::Db::open(":memory:").expect("open test db");

    // Set ceiling to 0 (no spend allowed)
    spend::set_ceiling(&db, 0.0).expect("set ceiling to 0");

    // Verify that gate_run denies when ceiling is 0 and account_usage is 0
    let result = spend::gate_run(&db, 0.0, Some(0.0));
    match result {
        spend::GateResult::Denied { reason } => {
            assert!(reason.contains("Spend ceiling reached"));
        }
        _ => panic!("expected Denied when at zero ceiling"),
    }
}

// ---------------------------------------------------------------------
// T1: `spend_by_bot` must filter by month. Bite: delete
// `AND substr(m.created_at, 1, 7) = ?` at `spend.rs:~118` and this goes red
// (September would pick up arthur's August row too).
// ---------------------------------------------------------------------

#[test]
fn spend_by_bot_returns_only_the_current_month() {
    let db = store::Db::open(":memory:").expect("open test db");
    seed_bot(&db, "arthur", "Arthur");
    seed_bot(&db, "riley", "Riley");

    seed_assistant_message(&db, "arthur", "2026-09-05T00:00:00Z", 3.0);
    seed_assistant_message(&db, "arthur", "2026-08-05T00:00:00Z", 7.0);
    seed_assistant_message(&db, "riley", "2026-09-06T00:00:00Z", 2.0);
    seed_assistant_message(&db, "riley", "2026-07-06T00:00:00Z", 9.0);

    let september = spend::spend_by_bot(&db, "2026-09").expect("query spend by bot");
    let mut by_id: std::collections::HashMap<String, f64> = september
        .into_iter()
        .map(|row| (row.bot_id, row.cost_usd))
        .collect();
    assert_eq!(by_id.remove("arthur"), Some(3.0), "arthur, September only");
    assert_eq!(by_id.remove("riley"), Some(2.0), "riley, September only");
    assert!(by_id.is_empty(), "no other bots in September");

    let august = spend::spend_by_bot(&db, "2026-08").expect("query spend by bot");
    assert_eq!(august.len(), 1);
    assert_eq!(august[0].bot_id, "arthur");
    assert_eq!(august[0].cost_usd, 7.0);
}

// ---------------------------------------------------------------------
// F11: `GET /api/spend` must honour `?month=`.
// ---------------------------------------------------------------------

#[tokio::test]
async fn get_spend_honours_the_month_query_param() {
    let db = open_db();
    let cookie = seed_session(&db);
    seed_bot(&db, "arthur", "Arthur");
    seed_assistant_message(&db, "arthur", "2026-09-05T00:00:00Z", 3.0);
    seed_assistant_message(&db, "arthur", "2026-08-05T00:00:00Z", 7.0);

    let port: Arc<dyn ModelPort> = as_port(model::fake::text_port("hi", "test/model"));
    let credits = Arc::new(spend::FakeCredits::usage(0.0));
    let app = build_app(AppState::with_port_and_credits(db, port, credits));

    let (status, body) = get_json(app, "/api/spend?month=2026-08", &cookie).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["month"], "2026-08");
    let bots = body["bots"].as_array().expect("bots array");
    assert_eq!(bots.len(), 1);
    assert_eq!(bots[0]["botId"], "arthur");
    assert_eq!(bots[0]["costUsd"], 7.0);
}

// ---------------------------------------------------------------------
// F3/D6: the ceiling gate, driven over HTTP with a real `CreditsPort`.
// Bite: pass `None` again at `routes/messages.rs` (drop `account_usage`)
// and `posting_over_the_ceiling_answers_402_and_writes_nothing` goes red -
// the account never reads as over the ceiling, so the POST answers 200
// instead of 402.
// ---------------------------------------------------------------------

#[tokio::test]
async fn posting_over_the_ceiling_answers_402_and_writes_nothing() {
    let db = open_db();
    let cookie = seed_session(&db);
    seed_bot(&db, "arthur", "Arthur");
    spend::set_ceiling(&db, 1.0).expect("set ceiling");

    let port: Arc<dyn ModelPort> =
        as_port(model::fake::text_port("should never run", "test/model"));
    let credits = Arc::new(spend::FakeCredits::usage(1.5));
    let app = build_app(AppState::with_port_and_credits(db, port, credits));

    let resp = app
        .clone()
        .oneshot(post_req(
            "/api/bots/arthur/messages",
            json!({"text": "hi"}),
            &cookie,
        ))
        .await
        .expect("oneshot");
    assert_eq!(resp.status(), StatusCode::PAYMENT_REQUIRED);
    let body: Value = serde_json::from_slice(
        &resp
            .into_body()
            .collect()
            .await
            .expect("collect")
            .to_bytes(),
    )
    .expect("json body");
    assert_eq!(body["kind"], "spend-ceiling");
    let error = body["error"].as_str().expect("error string");
    assert!(error.contains("Spend ceiling reached"), "{error}");
    assert!(error.contains("$1.50"), "{error}");
    assert!(error.contains("$1.00"), "{error}");

    // No user message (and so no run) was ever written: the gate denies
    // before `store::append_message` runs.
    let (status, body) = get_json(app, "/api/bots/arthur/conversation", &cookie).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(
        body["messages"].as_array().map(Vec::len),
        Some(0),
        "denied POST must not have appended a message"
    );
}

// ---------------------------------------------------------------------
// D6: `Allowed { warning }` must reach the run as its first event.
// ---------------------------------------------------------------------

#[tokio::test]
async fn near_ceiling_run_starts_with_the_warning_as_its_first_event() {
    let db = open_db();
    let cookie = seed_session(&db);
    seed_bot(&db, "arthur", "Arthur");
    spend::set_ceiling(&db, 100.0).expect("set ceiling");

    // 90 used of 100: 10% headroom, inside the 15% warning band.
    let port: Arc<dyn ModelPort> = as_port(model::fake::text_port("Hello, Josh.", "test/model"));
    let credits = Arc::new(spend::FakeCredits::usage(90.0));
    let app = build_app(AppState::with_port_and_credits(db, port, credits));

    let resp = app
        .oneshot(post_req(
            "/api/bots/arthur/messages",
            json!({"text": "hi"}),
            &cookie,
        ))
        .await
        .expect("oneshot");
    assert_eq!(resp.status(), StatusCode::OK);

    let mut stream = resp.into_body().into_data_stream();
    let mut buf = String::new();
    let run_frame = read_next_frame(&mut stream, &mut buf)
        .await
        .expect("run frame");
    assert_eq!(run_frame["type"], "run");

    let first_event = read_next_frame(&mut stream, &mut buf)
        .await
        .expect("first run event");
    assert_eq!(first_event["type"], "notice");
    let message = first_event["message"].as_str().expect("message");
    assert!(message.contains("left before"), "{message}");
}

// ---------------------------------------------------------------------
// gate_run allows on a credits read failure (see its own doc): the run
// still has to start.
// ---------------------------------------------------------------------

#[tokio::test]
async fn credits_read_error_still_starts_the_run() {
    let db = open_db();
    let cookie = seed_session(&db);
    seed_bot(&db, "arthur", "Arthur");
    spend::set_ceiling(&db, 10.0).expect("set ceiling");

    let port: Arc<dyn ModelPort> = as_port(model::fake::text_port("Hello, Josh.", "test/model"));
    let credits = Arc::new(spend::FakeCredits::failing("network blip"));
    let app = build_app(AppState::with_port_and_credits(db, port, credits));

    let resp = app
        .oneshot(post_req(
            "/api/bots/arthur/messages",
            json!({"text": "hi"}),
            &cookie,
        ))
        .await
        .expect("oneshot");
    assert_eq!(resp.status(), StatusCode::OK);

    let mut stream = resp.into_body().into_data_stream();
    let mut buf = String::new();
    let run_frame = read_next_frame(&mut stream, &mut buf)
        .await
        .expect("run frame");
    assert_eq!(run_frame["type"], "run");

    let first_event = read_next_frame(&mut stream, &mut buf)
        .await
        .expect("first run event");
    assert_eq!(first_event["type"], "notice");
    let message = first_event["message"].as_str().expect("message");
    assert!(message.contains("Could not read"), "{message}");
}
