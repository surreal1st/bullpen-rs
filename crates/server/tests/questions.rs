//! Tests for questions: ask → row + message; answer → answered_at set,
//! message appended; answered questions absent from GET.
//! S2-F-05: ask_josh must write the row (driven via RunManager); answer
//! must check answered_at IS NULL and return 404 on no such question.

mod common;

use std::sync::Arc;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use common::*;
use http_body_util::BodyExt;
use model::ladder::Trigger;
use model::{ModelEvent, ModelMessage, ToolCall};
use serde_json::{Value, json};
use server::runs::{RunEvent, RunManager, StartOptions};
use server::{AppState, build_app};
use store::Db;
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
async fn ask_josh_tool_writes_question_row_and_posts_message() {
    let db_arc = Arc::new(std::sync::Mutex::new(
        Db::open(":memory:").expect("open :memory: db"),
    ));
    // Disable routing for scripted tests
    {
        let db = db_arc.lock().unwrap();
        model::routing::set_routing_settings(&db, Some(false), None)
            .expect("disable routing classifier for scripted-model tests");
        seed_session(&db);
    }

    // Set up the bot and conversation
    {
        let db = db_arc.lock().unwrap();
        db.conn()
            .execute(
                "INSERT INTO bots (id, name, purpose, instructions, model, created_at) VALUES (?1, ?2, '', ?3, NULL, '2026-01-01T00:00:00Z')",
                rusqlite::params!["arthur", "Arthur", "You are Arthur."],
            )
            .expect("seed bot");
    }

    let conversation_id = {
        let db = db_arc.lock().unwrap();
        store::get_or_create_conversation(&db, "arthur").expect("get_or_create_conversation")
    };

    {
        let db = db_arc.lock().unwrap();
        store::append_message(
            &db,
            &conversation_id,
            "user",
            "ask me something",
            store::NewMessage::default(),
        )
        .expect("append user message");
    }

    // Script: first turn has ask_josh tool call, second turn has final answer
    let port = ScriptedPort::new(vec![
        vec![
            ModelEvent::ToolCalls {
                calls: vec![ToolCall {
                    id: "c1".to_string(),
                    name: "ask_josh".to_string(),
                    arguments: "{\"question\":\"What color?\",\"options\":[\"Red\",\"Blue\"]}"
                        .to_string(),
                }],
                usage: None,
            },
            ModelEvent::Done {
                model: "test/model".to_string(),
                usage: None,
                finish_reason: None,
            },
        ],
        vec![
            ModelEvent::Delta {
                text: "Waiting for your answer.".to_string(),
            },
            ModelEvent::Done {
                model: "test/model".to_string(),
                usage: None,
                finish_reason: None,
            },
        ],
    ]);

    let manager = Arc::new(RunManager::new(Arc::clone(&db_arc), as_port(port)));

    let run_id = manager.start(StartOptions {
        bot_id: "arthur".to_string(),
        conversation_id: conversation_id.clone(),
        model: "test/model".to_string(),
        messages: vec![ModelMessage::user("ask me something")],
        trigger: Trigger::Chat,
        room: false,
    });

    let events = drain(manager.subscribe(&run_id)).await;
    assert!(
        events
            .iter()
            .any(|e| matches!(e, RunEvent::ToolCall { name, .. } if name == "ask_josh")),
        "expected ask_josh tool call event, got {events:?}"
    );

    // Verify the question row was written
    {
        let db = db_arc.lock().unwrap();
        let questions = store::list_all_open(&db).expect("list questions");
        assert_eq!(
            questions.len(),
            1,
            "expected one open question from ask_josh tool call, got {}",
            questions.len()
        );
        assert_eq!(questions[0].question, "What color?");
        assert_eq!(questions[0].options, vec!["Red", "Blue"]);
    }

    // Verify the message was posted to the conversation
    {
        let db = db_arc.lock().unwrap();
        let messages = store::list_messages(&db, &conversation_id).expect("list messages");
        let assistant_messages: Vec<_> =
            messages.iter().filter(|m| m.role == "assistant").collect();
        assert!(
            assistant_messages
                .iter()
                .any(|m| m.content.contains("What color?")),
            "expected ask_josh message in conversation, got: {messages:?}"
        );
    }
}

#[tokio::test]
async fn answer_question_sets_answered_at_and_removes_from_list() {
    let db = store::Db::open(":memory:").expect("open :memory: db");
    let cookie = seed_session(&db);

    // Create a bot and conversation first.
    db.conn()
        .execute(
            "INSERT INTO bots (id, name, purpose, instructions, created_at) VALUES (?, ?, ?, ?, ?)",
            rusqlite::params!["test-bot", "Test", "A test bot", "", "2026-01-01T00:00:00Z"],
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

    // Answer the question - should return 200 {ok:true}
    let (answer_status, answer_body) = post(
        &app,
        &format!("/api/questions/{}", question_id),
        json!({ "answer": "Yes" }),
        &cookie,
    )
    .await;
    assert_eq!(answer_status, StatusCode::OK);
    assert_eq!(answer_body["ok"], true);

    // List questions after answering - should be empty.
    let (status, after) = get(&app, "/api/questions", &cookie).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(after["questions"].as_array().unwrap().len(), 0);
}

#[tokio::test]
async fn answer_twice_returns_404_on_second_attempt() {
    let db = store::Db::open(":memory:").expect("open :memory: db");
    let cookie = seed_session(&db);

    db.conn()
        .execute(
            "INSERT INTO bots (id, name, purpose, instructions, created_at) VALUES (?, ?, ?, ?, ?)",
            rusqlite::params!["test-bot", "Test", "A test bot", "", "2026-01-01T00:00:00Z"],
        )
        .expect("insert test bot");

    let conversation_id =
        store::get_or_create_conversation(&db, "test-bot").expect("get_or_create_conversation");

    let question_id = store::insert_question(
        &db,
        "test-bot",
        &conversation_id,
        Some("test-message"),
        "Do you like tests?",
        &[],
    )
    .expect("insert question");

    let app = build_app(AppState::new(db));

    // Answer once - should succeed
    let (status1, _) = post(
        &app,
        &format!("/api/questions/{}", question_id),
        json!({ "answer": "Yes" }),
        &cookie,
    )
    .await;
    assert_eq!(status1, StatusCode::OK);

    // Answer again - should return 404
    let (status2, body2) = post(
        &app,
        &format!("/api/questions/{}", question_id),
        json!({ "answer": "No" }),
        &cookie,
    )
    .await;
    assert_eq!(status2, StatusCode::NOT_FOUND);
    assert_eq!(body2["error"], "no such open question");
}

#[tokio::test]
async fn answer_unknown_question_returns_404() {
    let db = store::Db::open(":memory:").expect("open :memory: db");
    let cookie = seed_session(&db);
    let app = build_app(AppState::new(db));

    let (status, body) = post(
        &app,
        "/api/questions/nonexistent-id",
        json!({ "answer": "Yes" }),
        &cookie,
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert_eq!(body["error"], "no such open question");
}

#[tokio::test]
async fn answer_empty_text_returns_400() {
    let db = store::Db::open(":memory:").expect("open :memory: db");
    let cookie = seed_session(&db);

    db.conn()
        .execute(
            "INSERT INTO bots (id, name, purpose, instructions, created_at) VALUES (?, ?, ?, ?, ?)",
            rusqlite::params!["test-bot", "Test", "A test bot", "", "2026-01-01T00:00:00Z"],
        )
        .expect("insert test bot");

    let conversation_id =
        store::get_or_create_conversation(&db, "test-bot").expect("get_or_create_conversation");

    let question_id = store::insert_question(
        &db,
        "test-bot",
        &conversation_id,
        None,
        "Do you like tests?",
        &[],
    )
    .expect("insert question");

    let app = build_app(AppState::new(db));

    let (status, body) = post(
        &app,
        &format!("/api/questions/{}", question_id),
        json!({ "answer": "" }),
        &cookie,
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(body["error"], "answer cannot be empty");
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
