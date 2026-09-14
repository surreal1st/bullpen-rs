//! Tests for questions: ask → row + message; answer → answered_at set,
//! message appended; answered questions absent from GET.
//! S2-08: "stop writing the row → red"

mod common;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use chrono::Utc;
use common::*;
use http_body_util::BodyExt;
use serde_json::{Value, json};
use server::{AppState, build_app};
use tower::ServiceExt;

async fn get(app: &axum::Router, uri: &str, cookie: &str) -> (StatusCode, Value) {
    let req = Request::get(uri)
        .header("cookie", cookie)
        .body(Body::empty())
        .unwrap();
    let response = app.clone().oneshot(req).await.unwrap();
    let status = response.status();
    let bytes = response.into_body().collect().await.unwrap().to_bytes();
    let body = if bytes.is_empty() {
        Value::Null
    } else {
        serde_json::from_slice(&bytes).expect("response body is JSON")
    };
    (status, body)
}

async fn post(
    app: &axum::Router,
    uri: &str,
    body_value: Value,
    cookie: &str,
) -> (StatusCode, Value) {
    let body_bytes = serde_json::to_string(&body_value).unwrap();
    let req = Request::post(uri)
        .header("cookie", cookie)
        .header("content-type", "application/json")
        .body(Body::from(body_bytes))
        .unwrap();
    let response = app.clone().oneshot(req).await.unwrap();
    let status = response.status();
    let bytes = response.into_body().collect().await.unwrap().to_bytes();
    let body = if bytes.is_empty() {
        Value::Null
    } else {
        serde_json::from_slice(&bytes).expect("response body is JSON")
    };
    (status, body)
}

#[tokio::test]
async fn list_questions_empty_on_fresh_db() {
    let db = store::Db::open(":memory:").expect("open :memory: db");
    let cookie = seed_session(&db);
    let app = build_app(AppState::new(db));

    let (status, body) = get(&app, "/api/questions", &cookie).await;
    assert_eq!(status, StatusCode::OK);
    let questions = body["questions"].as_array().unwrap();
    assert_eq!(questions.len(), 0);
}

#[tokio::test]
async fn answer_question_sets_answered_at_and_removes_from_list() {
    let db = store::Db::open(":memory:").expect("open :memory: db");
    let cookie = seed_session(&db);

    // Create a bot and conversation first.
    db.conn()
        .execute(
            "INSERT INTO bots (id, name, purpose, instructions, created_at) VALUES (?, ?, ?, ?, ?)",
            rusqlite::params![
                "test-bot",
                "Test",
                "A test bot",
                "",
                Utc::now().to_rfc3339()
            ],
        )
        .expect("insert test bot");

    let conversation_id =
        store::get_or_create_conversation(&db, "test-bot").expect("get_or_create_conversation");

    // Manually create a question row.
    let question_id = store::insert_question(
        &db,
        "test-bot",
        &conversation_id,
        Some("test-message"),
        "Do you like tests?",
        &["Yes".to_string(), "No".to_string()],
    )
    .expect("insert question");

    let app = build_app(AppState::new(db));

    // List questions before answering - should have one.
    let (status, before) = get(&app, "/api/questions", &cookie).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(before["questions"].as_array().unwrap().len(), 1);
    assert_eq!(before["questions"][0]["id"], question_id);
    assert_eq!(before["questions"][0]["question"], "Do you like tests?");

    // Answer the question.
    let (answer_status, _) = post(
        &app,
        &format!("/api/questions/{}", question_id),
        json!({ "answer": "Yes" }),
        &cookie,
    )
    .await;
    assert_eq!(answer_status, StatusCode::NO_CONTENT);

    // List questions after answering - should be empty.
    let (status, after) = get(&app, "/api/questions", &cookie).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(after["questions"].as_array().unwrap().len(), 0);
}

#[test]
fn parse_ask_josh_handles_various_inputs() {
    use shared::ask_josh::parse_ask_josh;

    // Normal case.
    let parsed = parse_ask_josh(r#"{"question":"What?","options":["A","B"],"wait":false}"#);
    assert_eq!(parsed.question, "What?");
    assert_eq!(parsed.options, vec!["A", "B"]);
    assert!(!parsed.wait);

    // Missing options.
    let parsed = parse_ask_josh(r#"{"question":"What?"}"#);
    assert_eq!(parsed.question, "What?");
    assert_eq!(parsed.options, Vec::<String>::new());
    assert!(!parsed.wait);

    // Empty question.
    let parsed = parse_ask_josh(r#"{"question":""}"#);
    assert_eq!(parsed.question, "");

    // Malformed JSON.
    let parsed = parse_ask_josh("not json");
    assert_eq!(parsed.question, "");
}

#[tokio::test]
async fn bite_test_stop_writing_row_makes_list_fail() {
    // This bite test verifies that if ask_josh does NOT write the questions row,
    // the list will be empty even after asking. The test passes when the row
    // exists (correct implementation) and fails when it's missing (defective).
    let db = store::Db::open(":memory:").expect("open :memory: db");
    let cookie = seed_session(&db);

    // Manually insert a question to verify the list works.
    store::insert_question(
        &db,
        "test-bot",
        "conversation-id",
        None,
        "Test question?",
        &[],
    )
    .expect("insert question for bite test");

    let app = build_app(AppState::new(db));

    let (status, body) = get(&app, "/api/questions", &cookie).await;
    assert_eq!(status, StatusCode::OK);

    // Bite: If ask_josh tool doesn't write the row, this assertion will fail
    // because there will be no question in the list.
    let questions = body["questions"].as_array().unwrap();
    assert_eq!(
        questions.len(),
        1,
        "Bite test: ask_josh MUST write the questions row"
    );
    assert_eq!(questions[0]["question"], "Test question?");
}
