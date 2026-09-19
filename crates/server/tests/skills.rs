//! S10-01 acceptance: the `/api/skills*` + `/api/bots/:id/skills*` routes
//! (`crates/server/src/routes/skills.rs`) and the `use_skill` tool
//! (`crates/server/src/tools/use_skill.rs`).
//!
//! Route tests drive the real HTTP API through `build_app`, same posture
//! `tests/goals.rs` already takes, with local `get_json`/`put_json`/
//! `delete_json` helpers (this crate compiles each test file separately, so
//! these are small enough to duplicate rather than share).
//!
//! The `use_skill` tool test drives `RunManager` directly with a
//! `ScriptedPort`, the same posture `tests/goal_tools.rs` already takes for
//! `set_goal`/`update_goal`/`reflect` - it is a tool-dispatch behaviour, not
//! a route behaviour, so there is no HTTP surface to call it through.
//!
//! The prompt-carries-description-never-body test lives in `tests/
//! prompt.rs` instead, next to every other `build_prompt` acceptance test
//! (same "no HTTP" exception S1-04 already established there), and the
//! delete-orphan test lives in `crates/store/src/skills.rs`'s own
//! `#[cfg(test)]` module, since an HTTP `GET` cannot tell "the orphan row
//! was deleted" apart from "the orphan row was left behind and the JOIN
//! just excludes it" - both read back as an empty list either way.

mod common;

use std::sync::{Arc, Mutex};

use axum::body::Body;
use axum::http::{Request, StatusCode};
use common::{ScriptedPort, drain, own_conversation, seed_user_message};
use model::ladder::Trigger;
use model::{ModelEvent, ModelMessage, ToolCall};
use serde_json::{Value, json};
use server::runs::{RunEvent, RunManager, StartOptions};
use server::{AppState, build_app};
use store::Db;
use tower::ServiceExt;

fn open_db() -> Db {
    let db = Db::open(":memory:").expect("open :memory: db");
    model::routing::set_routing_settings(&db, Some(false), None)
        .expect("disable routing classifier");
    server::judge::set_judge_enabled(&db, false).expect("disable judge");
    db
}

fn seed_bot(db: &Db, id: &str, name: &str) {
    db.conn()
        .execute(
            "INSERT INTO bots (id, name, purpose, instructions, model, created_at)
             VALUES (?1, ?2, '', 'do things', NULL, ?3)",
            rusqlite::params![id, name, chrono::Utc::now().to_rfc3339()],
        )
        .expect("seed bot");
}

async fn get_json(app: &axum::Router, path: &str, session: &str) -> (StatusCode, Value) {
    let request = Request::get(path)
        .header("cookie", session)
        .body(Body::empty())
        .unwrap();
    let response = app.clone().oneshot(request).await.unwrap();
    let status = response.status();
    let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap();
    (
        status,
        serde_json::from_slice(&bytes).unwrap_or(Value::Null),
    )
}

async fn put_json(
    app: &axum::Router,
    path: &str,
    session: &str,
    body: Value,
) -> (StatusCode, Value) {
    let request = Request::put(path)
        .header("cookie", session)
        .header("content-type", "application/json")
        .body(Body::from(body.to_string()))
        .unwrap();
    let response = app.clone().oneshot(request).await.unwrap();
    let status = response.status();
    let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap();
    (
        status,
        serde_json::from_slice(&bytes).unwrap_or(Value::Null),
    )
}

async fn delete_json(app: &axum::Router, path: &str, session: &str) -> (StatusCode, Value) {
    let request = Request::delete(path)
        .header("cookie", session)
        .body(Body::empty())
        .unwrap();
    let response = app.clone().oneshot(request).await.unwrap();
    let status = response.status();
    let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap();
    (
        status,
        serde_json::from_slice(&bytes).unwrap_or(Value::Null),
    )
}

// ---------------------------------------------------------------------
// 1. round-trip
// ---------------------------------------------------------------------

#[tokio::test]
async fn a_skill_round_trips_through_put_then_get() {
    let db = open_db();
    let session = common::seed_session(&db);
    let port: Arc<dyn model::ModelPort> = Arc::new(ScriptedPort::new(vec![]));
    let state = AppState::with_port(db, port);
    let app = build_app(state);

    let (status, body) = put_json(
        &app,
        "/api/skills/pdf-extract",
        &session,
        json!({ "description": "When Josh sends a PDF and wants the numbers out.", "body": "Read the PDF with pdftotext, then..." }),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["skill"]["name"], "pdf-extract");
    assert_eq!(
        body["skill"]["description"],
        "When Josh sends a PDF and wants the numbers out."
    );
    assert_eq!(
        body["skill"]["body"],
        "Read the PDF with pdftotext, then..."
    );

    let (status, body) = get_json(&app, "/api/skills/pdf-extract", &session).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["skill"]["name"], "pdf-extract");
    assert_eq!(
        body["skill"]["description"],
        "When Josh sends a PDF and wants the numbers out."
    );
    assert_eq!(
        body["skill"]["body"],
        "Read the PDF with pdftotext, then..."
    );
}

// ---------------------------------------------------------------------
// 2. list strips body, carries bytes
// ---------------------------------------------------------------------

#[tokio::test]
async fn list_skills_strips_body_and_carries_bytes() {
    let db = open_db();
    let session = common::seed_session(&db);
    let port: Arc<dyn model::ModelPort> = Arc::new(ScriptedPort::new(vec![]));
    let state = AppState::with_port(db, port);
    let app = build_app(state);

    let long_body = "x".repeat(137);
    put_json(
        &app,
        "/api/skills/big-body",
        &session,
        json!({ "description": "d", "body": long_body }),
    )
    .await;

    let (status, body) = get_json(&app, "/api/skills", &session).await;
    assert_eq!(status, StatusCode::OK);
    let entry = body["skills"]
        .as_array()
        .expect("skills array")
        .iter()
        .find(|s| s["name"] == "big-body")
        .expect("big-body listed")
        .clone();

    assert!(
        entry.get("body").is_none(),
        "listing must not carry the body field at all: {entry:?}"
    );
    assert_eq!(entry["bytes"].as_u64(), Some(137));
}

// ---------------------------------------------------------------------
// 3. unusable name -> 400
// ---------------------------------------------------------------------

#[tokio::test]
async fn a_name_that_normalises_to_empty_is_a_400() {
    let db = open_db();
    let session = common::seed_session(&db);
    let port: Arc<dyn model::ModelPort> = Arc::new(ScriptedPort::new(vec![]));
    let state = AppState::with_port(db, port);
    let app = build_app(state);

    let (status, body) = put_json(
        &app,
        "/api/skills/---",
        &session,
        json!({ "description": "d", "body": "b" }),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(body["error"], "that name is not usable");
}

// ---------------------------------------------------------------------
// 4. PUT twice updates, one row
// ---------------------------------------------------------------------

#[tokio::test]
async fn put_twice_with_the_same_name_updates_leaving_exactly_one_row() {
    let db = open_db();
    let session = common::seed_session(&db);
    let port: Arc<dyn model::ModelPort> = Arc::new(ScriptedPort::new(vec![]));
    let state = AppState::with_port(db, port);
    let app = build_app(state);

    let (status, first) = put_json(
        &app,
        "/api/skills/reuse-me",
        &session,
        json!({ "description": "first description", "body": "first body" }),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let first_id = first["skill"]["id"].as_str().expect("id").to_string();

    let (status, second) = put_json(
        &app,
        "/api/skills/reuse-me",
        &session,
        json!({ "description": "second description", "body": "second body" }),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let second_id = second["skill"]["id"].as_str().expect("id").to_string();

    assert_eq!(
        first_id, second_id,
        "the second PUT must update the same row, not insert a new one"
    );
    assert_eq!(second["skill"]["description"], "second description");
    assert_eq!(second["skill"]["body"], "second body");

    let (_, list) = get_json(&app, "/api/skills", &session).await;
    let matches: Vec<&Value> = list["skills"]
        .as_array()
        .expect("skills array")
        .iter()
        .filter(|s| s["name"] == "reuse-me")
        .collect();
    assert_eq!(
        matches.len(),
        1,
        "exactly one row named reuse-me must exist after two PUTs: {matches:?}"
    );
}

// ---------------------------------------------------------------------
// 5. enabling for one bot does not enable for another
// ---------------------------------------------------------------------

#[tokio::test]
async fn enabling_a_skill_for_one_bot_does_not_enable_it_for_another() {
    let db = open_db();
    seed_bot(&db, "arthur", "Arthur");
    seed_bot(&db, "boris", "Boris");
    let session = common::seed_session(&db);
    let port: Arc<dyn model::ModelPort> = Arc::new(ScriptedPort::new(vec![]));
    let state = AppState::with_port(db, port);
    let app = build_app(state);

    put_json(
        &app,
        "/api/skills/only-arthurs",
        &session,
        json!({ "description": "d", "body": "b" }),
    )
    .await;

    let (status, body) = put_json(
        &app,
        "/api/bots/arthur/skills/only-arthurs",
        &session,
        json!({ "on": true }),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["skills"].as_array().unwrap().len(), 1);

    let (_, arthur_skills) = get_json(&app, "/api/bots/arthur/skills", &session).await;
    assert_eq!(
        arthur_skills["skills"],
        json!(["only-arthurs"]),
        "arthur must have the skill enabled"
    );

    let (_, boris_skills) = get_json(&app, "/api/bots/boris/skills", &session).await;
    assert_eq!(
        boris_skills["skills"],
        json!([]),
        "boris must NOT have arthur's skill enabled"
    );
}

// ---------------------------------------------------------------------
// route edges: 404s, malformed bodies, `on` coercion
// ---------------------------------------------------------------------

#[tokio::test]
async fn get_skill_404s_for_an_unknown_name() {
    let db = open_db();
    let session = common::seed_session(&db);
    let port: Arc<dyn model::ModelPort> = Arc::new(ScriptedPort::new(vec![]));
    let state = AppState::with_port(db, port);
    let app = build_app(state);

    let (status, body) = get_json(&app, "/api/skills/does-not-exist", &session).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert_eq!(body["error"], "no such skill");
}

#[tokio::test]
async fn bot_skills_routes_404_for_an_unknown_bot() {
    let db = open_db();
    let session = common::seed_session(&db);
    let port: Arc<dyn model::ModelPort> = Arc::new(ScriptedPort::new(vec![]));
    let state = AppState::with_port(db, port);
    let app = build_app(state);

    let (status, body) = get_json(&app, "/api/bots/nobody/skills", &session).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert_eq!(body["error"], "no such bot");

    let (status, body) = put_json(
        &app,
        "/api/bots/nobody/skills/whatever",
        &session,
        json!({ "on": true }),
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert_eq!(body["error"], "no such bot");
}

#[tokio::test]
async fn on_is_true_only_for_a_literal_true() {
    let db = open_db();
    seed_bot(&db, "arthur", "Arthur");
    let session = common::seed_session(&db);
    let port: Arc<dyn model::ModelPort> = Arc::new(ScriptedPort::new(vec![]));
    let state = AppState::with_port(db, port);
    let app = build_app(state);

    put_json(
        &app,
        "/api/skills/loose-truthy",
        &session,
        json!({ "description": "d", "body": "b" }),
    )
    .await;

    // "true" (a string) and 1 (a number) must both coerce to false - only
    // the literal JSON boolean `true` switches a skill on.
    let (status, body) = put_json(
        &app,
        "/api/bots/arthur/skills/loose-truthy",
        &session,
        json!({ "on": "true" }),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["skills"], json!([]));

    let (status, body) = put_json(
        &app,
        "/api/bots/arthur/skills/loose-truthy",
        &session,
        json!({ "on": 1 }),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["skills"], json!([]));

    let (status, body) = put_json(
        &app,
        "/api/bots/arthur/skills/loose-truthy",
        &session,
        json!({ "on": true }),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["skills"], json!(["loose-truthy"]));
}

// ---------------------------------------------------------------------
// 8 (HTTP half): deleting a skill is observable through the bot's list
// ---------------------------------------------------------------------

#[tokio::test]
async fn deleting_a_skill_removes_it_from_a_bots_enabled_list() {
    let db = open_db();
    seed_bot(&db, "arthur", "Arthur");
    let session = common::seed_session(&db);
    let port: Arc<dyn model::ModelPort> = Arc::new(ScriptedPort::new(vec![]));
    let state = AppState::with_port(db, port);
    let app = build_app(state);

    put_json(
        &app,
        "/api/skills/temporary",
        &session,
        json!({ "description": "d", "body": "b" }),
    )
    .await;
    put_json(
        &app,
        "/api/bots/arthur/skills/temporary",
        &session,
        json!({ "on": true }),
    )
    .await;

    let (status, body) = delete_json(&app, "/api/skills/temporary", &session).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["ok"], true);

    let (status, _) = delete_json(&app, "/api/skills/temporary", &session).await;
    assert_eq!(status, StatusCode::NOT_FOUND);

    let (_, arthur_skills) = get_json(&app, "/api/bots/arthur/skills", &session).await;
    assert_eq!(arthur_skills["skills"], json!([]));
}

// ---------------------------------------------------------------------
// 7. use_skill names what the bot DOES have
// ---------------------------------------------------------------------

/// Turn 1 calls `tool_name` with `args_json`; turn 2 answers plainly.
/// Mirrors `tests/goal_tools.rs`'s identical helper.
fn tool_call_then_answer(tool_name: &str, args_json: &str) -> ScriptedPort {
    ScriptedPort::new(vec![
        vec![ModelEvent::ToolCalls {
            calls: vec![ToolCall {
                id: "c1".to_string(),
                name: tool_name.to_string(),
                arguments: args_json.to_string(),
            }],
            usage: None,
        }],
        vec![
            ModelEvent::Delta {
                text: "Done.".to_string(),
            },
            ModelEvent::Done {
                model: "test/model".to_string(),
                usage: None,
                finish_reason: None,
            },
        ],
    ])
}

fn tool_result(events: &[RunEvent], tool_name: &str) -> Option<String> {
    events.iter().find_map(|e| match e {
        RunEvent::ToolResult { name, result } if name == tool_name => Some(result.clone()),
        _ => None,
    })
}

#[tokio::test]
async fn use_skill_for_an_unlisted_name_names_what_the_bot_does_have() {
    let db = open_db();
    seed_bot(&db, "arthur", "Arthur");
    store::skills::save_skill(
        &db,
        store::skills::SkillInput {
            name: "only-skill".to_string(),
            description: "d".to_string(),
            body: "b".to_string(),
            source: None,
        },
        chrono::Utc::now(),
    )
    .expect("save_skill")
    .expect("save_skill upserted");
    store::skills::set_bot_skill(&db, "arthur", "only-skill", true).expect("set_bot_skill");

    let db = Arc::new(Mutex::new(db));
    let conversation_id = own_conversation(&db, "arthur");
    seed_user_message(&db, &conversation_id, "go");

    let port: Arc<dyn model::ModelPort> = Arc::new(tool_call_then_answer(
        "use_skill",
        r#"{"name":"nonexistent"}"#,
    ));
    let manager = Arc::new(RunManager::new(Arc::clone(&db), port));
    let run_id = manager.start(StartOptions {
        bot_id: "arthur".to_string(),
        conversation_id,
        model: "test/model".to_string(),
        messages: vec![ModelMessage::user("go")],
        trigger: Trigger::Chat,
        room: false,
    });
    let events = drain(manager.subscribe(&run_id)).await;

    let result = tool_result(&events, "use_skill").expect("use_skill must have run");
    assert_eq!(
        result,
        "No skill called \"nonexistent\". You have: only-skill."
    );
}

#[tokio::test]
async fn use_skill_with_no_skills_enabled_says_so() {
    let db = open_db();
    seed_bot(&db, "arthur", "Arthur");

    let db = Arc::new(Mutex::new(db));
    let conversation_id = own_conversation(&db, "arthur");
    seed_user_message(&db, &conversation_id, "go");

    let port: Arc<dyn model::ModelPort> =
        Arc::new(tool_call_then_answer("use_skill", r#"{"name":"anything"}"#));
    let manager = Arc::new(RunManager::new(Arc::clone(&db), port));
    let run_id = manager.start(StartOptions {
        bot_id: "arthur".to_string(),
        conversation_id,
        model: "test/model".to_string(),
        messages: vec![ModelMessage::user("go")],
        trigger: Trigger::Chat,
        room: false,
    });
    let events = drain(manager.subscribe(&run_id)).await;

    let result = tool_result(&events, "use_skill").expect("use_skill must have run");
    assert_eq!(
        result,
        "No skill called \"anything\", and this bot has none enabled."
    );
}
