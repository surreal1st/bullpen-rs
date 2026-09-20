//! S10-09: W5 `propose_tool` — key bites from bullpen-night `bot-tools.test.ts`.

mod common;

use std::sync::Arc;
use std::time::Duration;

use axum::Router;
use axum::body::Body;
use axum::http::{Request, StatusCode};
use common::as_port;
use common::{ScriptedPort, own_conversation, seed_session, seed_user_message};
use model::{ModelEvent, ModelMessage, ModelRequest, ToolCall};
use serde_json::{Value, json};
use server::runs::{RunEvent, RunManager, StartOptions};
use server::{AppState, build_app};
use std::sync::Mutex;
use store::Db;
use tempfile::TempDir;
use tower::ServiceExt;

const WORD_COUNT: &str = r#"
export async function run(args: ToolArgs, ctx: ToolContext) {
  const text = typeof args.text === "string" ? args.text : "";
  ctx.log("counting " + text.length + " characters");
  const words = text.split(" ").filter(function (word: string) { return word.length > 0; });
  return { words: words.length };
}
"#;

fn word_count_proposal() -> Value {
    json!({
        "name": "word_count",
        "description": "Count the words in a piece of text and return { words }.",
        "parameters": { "type": "object", "properties": { "text": { "type": "string" } }, "required": ["text"] },
        "examples": [{ "args": { "text": "one two three" }, "expect": "{\"words\":3}" }],
        "source": WORD_COUNT,
    })
}

fn call_then_answer(name: &str, args: Value) -> ScriptedPort {
    ScriptedPort::new(vec![
        vec![ModelEvent::ToolCalls {
            calls: vec![ToolCall {
                id: "call-1".to_string(),
                name: name.to_string(),
                arguments: args.to_string(),
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

struct W5Fixture {
    _temp: TempDir,
    session: String,
    app: Router,
    port: Arc<ScriptedPort>,
}

fn open_w5_fixture(port: ScriptedPort) -> W5Fixture {
    let temp = TempDir::new().expect("tempdir");
    let data_dir = temp.path().join("data");
    std::fs::create_dir_all(&data_dir).expect("data dir");
    let db_path = data_dir.join("bullpen.db");
    let db = Db::open(db_path.to_str().expect("utf8 path")).expect("open db");
    model::routing::set_routing_settings(&db, Some(false), None).expect("routing off");
    server::judge::set_judge_enabled(&db, false).expect("judge off");
    store::set_password(&db, "test-password").expect("password");
    db.conn()
        .execute(
            "INSERT INTO bots (id, name, purpose, instructions, model, created_at) VALUES ('arthur','Arthur','','You are Arthur.',NULL,'2026-01-01T00:00:00Z')",
            [],
        )
        .expect("seed arthur");
    let session = seed_session(&db);
    let db_path_str = db_path.to_string_lossy().to_string();
    let data_dir_str = data_dir.to_string_lossy().to_string();
    let conversation_id = store::get_or_create_conversation(&db, "arthur").expect("conv");
    store::append_message(
        &db,
        &conversation_id,
        "user",
        "hello",
        store::NewMessage::default(),
    )
    .expect("seed msg");
    let port = Arc::new(port);
    let state = AppState::with_port(db, port.clone());
    state.configure_w5(db_path_str.clone(), data_dir_str.clone());
    let app = build_app(state);
    W5Fixture {
        _temp: temp,
        session,
        app,
        port,
    }
}

fn tool_results(requests: &[ModelRequest]) -> Vec<String> {
    let mut out = Vec::new();
    for request in requests {
        for message in &request.messages {
            if message.role == "tool"
                && let model::MessageContent::Text(text) = &message.content
            {
                out.push(text.clone());
            }
        }
    }
    out
}

async fn get_json(app: &Router, uri: &str, cookie: &str) -> (StatusCode, Value) {
    let resp = app
        .clone()
        .oneshot(
            Request::get(uri)
                .header("cookie", cookie)
                .body(Body::empty())
                .expect("request"),
        )
        .await
        .expect("oneshot");
    let status = resp.status();
    let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
        .await
        .expect("body");
    let json = if bytes.is_empty() {
        Value::Null
    } else {
        serde_json::from_slice(&bytes).expect("json")
    };
    (status, json)
}

async fn post_message(app: &Router, cookie: &str, text: &str) {
    let resp = app
        .clone()
        .oneshot(
            Request::post("/api/bots/arthur/messages")
                .header("content-type", "application/json")
                .header("cookie", cookie)
                .body(Body::from(json!({ "text": text }).to_string()))
                .expect("request"),
        )
        .await
        .expect("oneshot");
    assert_eq!(resp.status(), StatusCode::OK);
}

async fn wait_tool_results(port: &ScriptedPort) -> Vec<String> {
    for _ in 0..1200 {
        let results = tool_results(&port.requests());
        if !results.is_empty() {
            return results;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    panic!(
        "no tool results in time (model calls: {})",
        port.requests().len()
    );
}

async fn tool_names(app: &Router, cookie: &str) -> Vec<String> {
    let (status, body) = get_json(app, "/api/tools", cookie).await;
    assert_eq!(status, StatusCode::OK);
    body["tools"]
        .as_array()
        .unwrap_or(&vec![])
        .iter()
        .filter_map(|t| t["name"].as_str())
        .map(String::from)
        .collect()
}

fn night_available() -> bool {
    std::path::Path::new("/mnt/d/rainmade/projects/bullpen-night/src/server/bot-tools.ts").is_file()
}

#[tokio::test]
async fn word_count_parks_via_run_manager() {
    if !night_available() {
        return;
    }
    let temp = TempDir::new().expect("tempdir");
    let data_dir = temp.path().join("data");
    std::fs::create_dir_all(&data_dir).expect("data");
    let db_path = data_dir.join("bullpen.db");
    let db_path_str = db_path.to_string_lossy().to_string();
    let db = Db::open(&db_path_str).expect("open");
    model::routing::set_routing_settings(&db, Some(false), None).expect("routing");
    server::judge::set_judge_enabled(&db, false).expect("judge");
    db.conn()
        .execute(
            "INSERT INTO bots (id, name, purpose, instructions, model, created_at) VALUES ('arthur','Arthur','','You are Arthur.',NULL,'2026-01-01T00:00:00Z')",
            [],
        )
        .expect("seed");
    let db = Arc::new(Mutex::new(db));
    let conversation_id = own_conversation(&db, "arthur");
    seed_user_message(&db, &conversation_id, "write counter");
    let port = as_port(call_then_answer("propose_tool", word_count_proposal()));
    let manager = Arc::new(RunManager::new(Arc::clone(&db), port));
    manager.set_w5_paths(db_path_str.clone(), data_dir.to_string_lossy().to_string());
    let run_id = manager.start(StartOptions {
        bot_id: "arthur".to_string(),
        conversation_id,
        model: "test/model".to_string(),
        messages: vec![ModelMessage::user("write counter")],
        trigger: model::ladder::Trigger::Chat,
        room: false,
    });
    let mut rx = manager.subscribe(&run_id);
    let mut saw_approval = false;
    for _ in 0..600 {
        if let Ok(event) = rx.try_recv()
            && matches!(event, RunEvent::ApprovalNeeded { .. })
        {
            saw_approval = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    assert!(saw_approval, "expected ApprovalNeeded for word_count");
}

#[tokio::test]
async fn refuses_type_error_without_approval_card() {
    if !night_available() {
        eprintln!("skip: bullpen-night not available for W5 node bridge");
        return;
    }
    let args = json!({
        "name": "broken_count",
        "description": "Broken tool for testing type errors in propose flow.",
        "parameters": { "type": "object", "properties": {} },
        "examples": [{ "args": {}, "expect": "ok" }],
        "source": "export async function run(args: ToolArgs, ctx: ToolContext) {\n  const total: number = \"not a number\";\n  return total;\n}\n",
    });
    let fx = open_w5_fixture(call_then_answer("propose_tool", args));
    post_message(&fx.app, &fx.session, "write me a tool").await;
    let results = wait_tool_results(&fx.port).await;
    let said = results.join("\n");
    assert!(
        said.contains("does not typecheck"),
        "expected typecheck refusal, got: {said}"
    );
    let (status, body) = get_json(&fx.app, "/api/approvals", &fx.session).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["approvals"].as_array().map(|a| a.len()), Some(0));
    let names = tool_names(&fx.app, &fx.session).await;
    assert!(!names.contains(&"broken_count".to_string()));
}

#[tokio::test]
async fn refuses_import_without_approval_card() {
    if !night_available() {
        return;
    }
    let args = json!({
        "name": "reader",
        "description": "Try to import fs — should be refused at guard time.",
        "parameters": { "type": "object", "properties": {} },
        "examples": [{ "args": {}, "expect": "ok" }],
        "source": "import { readFileSync } from \"node:fs\";\nexport async function run(args: ToolArgs, ctx: ToolContext) {\n  return readFileSync(\"/etc/passwd\", \"utf8\");\n}\n",
    });
    let fx = open_w5_fixture(call_then_answer("propose_tool", args));
    post_message(&fx.app, &fx.session, "write me a tool").await;
    let results = wait_tool_results(&fx.port).await;
    let said = results.join("\n");
    assert!(said.contains("may not import anything"), "got: {said}");
    assert!(said.contains("line 1"), "got: {said}");
    let (status, body) = get_json(&fx.app, "/api/approvals", &fx.session).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["approvals"].as_array().map(|a| a.len()), Some(0));
}

async fn wait_run_status(db: &Arc<Mutex<Db>>, run_id: &str, target: &str) {
    for _ in 0..400 {
        let status = {
            let db = db.lock().expect("db");
            db.conn()
                .query_row(
                    "SELECT status FROM runs WHERE id = ?1",
                    rusqlite::params![run_id],
                    |row| row.get::<_, String>(0),
                )
                .unwrap_or_default()
        };
        if status == target {
            return;
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    panic!("run {run_id} never reached {target}");
}

async fn drain_until_approval(
    mut rx: tokio::sync::mpsc::UnboundedReceiver<RunEvent>,
) -> Option<String> {
    for _ in 0..600 {
        if let Ok(RunEvent::ApprovalNeeded { approval_id, .. }) = rx.try_recv() {
            return Some(approval_id);
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    None
}

#[tokio::test]
async fn approved_word_count_becomes_live_on_roster() {
    if !night_available() {
        return;
    }
    let temp = TempDir::new().expect("tempdir");
    let data_dir = temp.path().join("data");
    std::fs::create_dir_all(&data_dir).expect("data");
    let db_path = data_dir.join("bullpen.db");
    let db_path_str = db_path.to_string_lossy().to_string();
    let db = Db::open(&db_path_str).expect("open");
    model::routing::set_routing_settings(&db, Some(false), None).expect("routing");
    server::judge::set_judge_enabled(&db, false).expect("judge");
    db.conn()
        .execute(
            "INSERT INTO bots (id, name, purpose, instructions, model, created_at) VALUES ('arthur','Arthur','','You are Arthur.',NULL,'2026-01-01T00:00:00Z')",
            [],
        )
        .expect("seed");
    let db = Arc::new(Mutex::new(db));
    let conversation_id = own_conversation(&db, "arthur");
    seed_user_message(&db, &conversation_id, "write counter");
    let manager = Arc::new(RunManager::new(
        Arc::clone(&db),
        as_port(call_then_answer("propose_tool", word_count_proposal())),
    ));
    manager.set_w5_paths(db_path_str.clone(), data_dir.to_string_lossy().to_string());
    let run_id = manager.start(StartOptions {
        bot_id: "arthur".to_string(),
        conversation_id,
        model: "test/model".to_string(),
        messages: vec![ModelMessage::user("write counter")],
        trigger: model::ladder::Trigger::Chat,
        room: false,
    });
    let approval_id = drain_until_approval(manager.subscribe(&run_id))
        .await
        .expect("approval");
    let toolbox_before = manager.toolbox_for(
        "arthur",
        model::ladder::Trigger::Chat,
        false,
        "test/model",
        None,
    );
    assert!(!toolbox_before.specs.iter().any(|s| s.name == "word_count"));
    assert!(manager.decide_approval(&approval_id, true, None).await);
    wait_run_status(&db, &run_id, "done").await;
    let toolbox_after = manager.toolbox_for(
        "arthur",
        model::ladder::Trigger::Chat,
        false,
        "test/model",
        None,
    );
    assert!(toolbox_after.specs.iter().any(|s| s.name == "word_count"));
}

#[tokio::test]
async fn revoke_removes_tool_from_roster() {
    if !night_available() {
        return;
    }
    let temp = TempDir::new().expect("tempdir");
    let data_dir = temp.path().join("data");
    std::fs::create_dir_all(&data_dir).expect("data");
    let db_path = data_dir.join("bullpen.db");
    let db_path_str = db_path.to_string_lossy().to_string();
    let db = Db::open(&db_path_str).expect("open");
    model::routing::set_routing_settings(&db, Some(false), None).expect("routing");
    server::judge::set_judge_enabled(&db, false).expect("judge");
    db.conn()
        .execute(
            "INSERT INTO bots (id, name, purpose, instructions, model, created_at) VALUES ('arthur','Arthur','','You are Arthur.',NULL,'2026-01-01T00:00:00Z')",
            [],
        )
        .expect("seed");
    let db = Arc::new(Mutex::new(db));
    let conversation_id = own_conversation(&db, "arthur");
    seed_user_message(&db, &conversation_id, "write counter");
    let manager = Arc::new(RunManager::new(
        Arc::clone(&db),
        as_port(call_then_answer("propose_tool", word_count_proposal())),
    ));
    manager.set_w5_paths(db_path_str.clone(), data_dir.to_string_lossy().to_string());
    let run_id = manager.start(StartOptions {
        bot_id: "arthur".to_string(),
        conversation_id,
        model: "test/model".to_string(),
        messages: vec![ModelMessage::user("write counter")],
        trigger: model::ladder::Trigger::Chat,
        room: false,
    });
    let approval_id = drain_until_approval(manager.subscribe(&run_id))
        .await
        .expect("approval");
    assert!(manager.decide_approval(&approval_id, true, None).await);
    wait_run_status(&db, &run_id, "done").await;
    assert!(server::bot_tools::is_bot_made_tool(&db, "word_count"));
    assert!(server::bot_tools::revoke_tool(&db, "word_count"));
    assert!(!server::bot_tools::is_bot_made_tool(&db, "word_count"));
    let toolbox = manager.toolbox_for(
        "arthur",
        model::ladder::Trigger::Chat,
        false,
        "test/model",
        None,
    );
    assert!(!toolbox.specs.iter().any(|s| s.name == "word_count"));
}
