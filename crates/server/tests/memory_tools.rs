//! S3-03 acceptance: the four memory tools (`note`, `remember_shared`,
//! `project_remember`, `search_memory`) and the tiered recall block they
//! feed. Drives `RunManager` with a `ScriptedPort` the same way
//! `tests/runs.rs`'s `say_tool_call_mid_run_posts_and_the_run_continues`
//! does - a tool call turn, then a final-answer turn - and reads
//! `memory_log` rows directly the same way `tests/common/mod.rs`'s helpers
//! read other tables raw, since `store::LogEntry` carries no `scope` of its
//! own for a test to assert on otherwise.

mod common;

use std::sync::{Arc, Mutex};

use common::{ScriptedPort, own_conversation, seed_bot, seed_user_message};
use model::ladder::Trigger;
use model::{ModelEvent, ModelMessage, ToolCall};
use rusqlite::OptionalExtension;
use server::runs::{RunManager, StartOptions};
use store::{Db, Scope};

fn open_db() -> Arc<Mutex<Db>> {
    let db = Db::open(":memory:").expect("open :memory: db");
    model::routing::set_routing_settings(&db, Some(false), None)
        .expect("disable routing classifier for scripted-model tests");
    Arc::new(Mutex::new(db))
}

/// Turn 1 calls `tool_name` with `args_json`; turn 2 answers plainly.
/// Mirrors `tests/runs.rs`'s `say` round trip.
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

async fn run_tool(
    db: &Arc<Mutex<Db>>,
    bot_id: &str,
    tool_name: &str,
    args_json: &str,
) -> Arc<ScriptedPort> {
    let conversation_id = own_conversation(db, bot_id);
    seed_user_message(db, &conversation_id, "go");

    let port = Arc::new(tool_call_then_answer(tool_name, args_json));
    let manager = Arc::new(RunManager::new(Arc::clone(db), as_port_arc(&port)));
    let run_id = manager.start(StartOptions {
        bot_id: bot_id.to_string(),
        conversation_id,
        model: "test/model".to_string(),
        messages: vec![ModelMessage::user("go")],
        trigger: Trigger::Chat,
        room: false,
    });
    common::drain(manager.subscribe(&run_id)).await;
    port
}

// `as_port` in `common` takes ownership; the tool tests need to keep their
// own `Arc<ScriptedPort>` afterward to read `.requests()`, so this clones
// the Arc into the trait object `RunManager::new` wants instead of moving it.
fn as_port_arc(port: &Arc<ScriptedPort>) -> Arc<dyn model::ModelPort> {
    Arc::clone(port) as Arc<dyn model::ModelPort>
}

/// One `memory_log` row, as raw SQL sees it - no store API exposes `scope`
/// or `project_id` on a row for a test to assert on otherwise.
struct MemoryRow {
    bot_id: String,
    kind: String,
    scope: String,
    project_id: Option<String>,
    expires_at: Option<String>,
}

/// The one `memory_log` row whose content is exactly `content`, if any.
/// Raw SQL, same escape hatch `build_prompt`'s tier queries use.
fn memory_row(db: &Arc<Mutex<Db>>, content: &str) -> Option<MemoryRow> {
    let db = db.lock().expect("db mutex poisoned");
    db.conn()
        .query_row(
            "SELECT bot_id, kind, scope, project_id, expires_at FROM memory_log WHERE content = ?1",
            rusqlite::params![content],
            |row| {
                Ok(MemoryRow {
                    bot_id: row.get(0)?,
                    kind: row.get(1)?,
                    scope: row.get(2)?,
                    project_id: row.get(3)?,
                    expires_at: row.get(4)?,
                })
            },
        )
        .optional()
        .expect("query memory_log")
}

fn memory_log_count(db: &Arc<Mutex<Db>>, bot_id: &str) -> i64 {
    let db = db.lock().expect("db mutex poisoned");
    db.conn()
        .query_row(
            "SELECT COUNT(*) FROM memory_log WHERE bot_id = ?1",
            rusqlite::params![bot_id],
            |row| row.get(0),
        )
        .expect("count memory_log")
}

fn system_text(messages: &[ModelMessage]) -> String {
    match &messages[0].content {
        model::MessageContent::Text(t) => t.clone(),
        model::MessageContent::Parts(_) => panic!("system message must be plain text"),
    }
}

#[tokio::test]
async fn note_tool_writes_an_own_scope_note_with_a_ttl() {
    let db = open_db();
    seed_bot(&db, "t", "T");

    run_tool(
        &db,
        "t",
        "note",
        r#"{"fact":"NOTE-MARKER status update","ttl":"1h"}"#,
    )
    .await;

    let row = memory_row(&db, "NOTE-MARKER status update").expect("note row must exist");
    assert_eq!(row.bot_id, "t");
    assert_eq!(row.kind, "note");
    assert_eq!(row.scope, "own");
    assert_eq!(row.project_id, None);
    assert!(row.expires_at.is_some(), "a note must carry an expiry");
}

#[tokio::test]
async fn remember_shared_tool_writes_a_shared_scope_entry() {
    let db = open_db();
    seed_bot(&db, "t", "T");

    run_tool(
        &db,
        "t",
        "remember_shared",
        r#"{"fact":"SHARED-MARKER everyone should know this"}"#,
    )
    .await;

    let row =
        memory_row(&db, "SHARED-MARKER everyone should know this").expect("shared row must exist");
    assert_eq!(row.bot_id, "t");
    assert_eq!(row.kind, "log");
    assert_eq!(row.scope, "shared");
    assert_eq!(row.project_id, None);
    assert_eq!(row.expires_at, None);
}

#[tokio::test]
async fn project_remember_tool_writes_a_project_scope_entry_for_a_member() {
    let db = open_db();
    seed_bot(&db, "t", "T");
    let project = {
        let locked = db.lock().unwrap();
        let project = store::create_project(&locked, "Zenith").unwrap();
        store::add_project_member(&locked, &project.id, "t").unwrap();
        project
    };

    run_tool(
        &db,
        "t",
        "project_remember",
        r#"{"project":"Zenith","fact":"PROJECT-MARKER the finish is a countout"}"#,
    )
    .await;

    let row =
        memory_row(&db, "PROJECT-MARKER the finish is a countout").expect("project row must exist");
    assert_eq!(row.bot_id, "t");
    assert_eq!(row.kind, "log");
    assert_eq!(row.scope, "project");
    assert_eq!(row.project_id, Some(project.id));
}

#[tokio::test]
async fn project_remember_tool_refuses_an_unknown_project_name_and_writes_nothing() {
    let db = open_db();
    seed_bot(&db, "t", "T");
    let before = memory_log_count(&db, "t");

    let port = run_tool(
        &db,
        "t",
        "project_remember",
        r#"{"project":"Nope","fact":"UNKNOWN-PROJECT-MARKER should not be written"}"#,
    )
    .await;

    let after = memory_log_count(&db, "t");
    assert_eq!(before, after, "an unknown project must write no row");

    let requests = port.requests();
    let second = &requests[1];
    let tool_result = second
        .messages
        .iter()
        .find(|m| m.role == "tool")
        .expect("a tool-result message");
    let text = match &tool_result.content {
        model::MessageContent::Text(t) => t.clone(),
        model::MessageContent::Parts(_) => panic!("expected text"),
    };
    assert!(
        text.contains("not a member of any project"),
        "expected a refusal naming the bot has no projects, got: {text}"
    );
}

#[tokio::test]
async fn search_memory_finds_a_shared_fact_another_bot_wrote() {
    let db = open_db();
    seed_bot(&db, "u", "U");
    seed_bot(&db, "t", "T");
    {
        let locked = db.lock().unwrap();
        store::remember_scoped(
            &locked,
            "u",
            "ZEBRA-MARKER the main event ends on a countout",
            Scope::Shared,
            None,
        )
        .unwrap();
    }

    let port = run_tool(&db, "t", "search_memory", r#"{"query":"zebra countout"}"#).await;

    let requests = port.requests();
    let second = &requests[1];
    let tool_result = second
        .messages
        .iter()
        .find(|m| m.role == "tool")
        .expect("a tool-result message");
    let text = match &tool_result.content {
        model::MessageContent::Text(t) => t.clone(),
        model::MessageContent::Parts(_) => panic!("expected text"),
    };
    assert!(
        text.contains("ZEBRA-MARKER"),
        "expected search_memory to find another bot's shared fact, got: {text}"
    );
}

#[tokio::test]
async fn expired_note_is_absent_from_the_next_turns_prompt() {
    let db = open_db();
    seed_bot(&db, "t", "T");
    {
        let locked = db.lock().unwrap();
        // ttl 0 -> `note()` sets `expires_at` to right now, already expired.
        store::note(&locked, "t", "EXPIRED-NOTE-MARKER", 0).unwrap();
        store::remember(&locked, "t", "LIVE-FACT-MARKER", "bot").unwrap();
    }

    let bot = {
        let locked = db.lock().unwrap();
        store::get_bot(&locked, "t").unwrap().unwrap()
    };
    let messages = server::prompt::build_prompt(&db.lock().unwrap(), &bot, &[]);

    let conversation_id = own_conversation(&db, "t");
    seed_user_message(&db, &conversation_id, "go");
    let port = Arc::new(ScriptedPort::new(vec![vec![
        ModelEvent::Delta {
            text: "ok".to_string(),
        },
        ModelEvent::Done {
            model: "test/model".to_string(),
            usage: None,
            finish_reason: None,
        },
    ]]));
    let manager = Arc::new(RunManager::new(Arc::clone(&db), as_port_arc(&port)));
    let run_id = manager.start(StartOptions {
        bot_id: "t".to_string(),
        conversation_id,
        model: "test/model".to_string(),
        messages,
        trigger: Trigger::Chat,
        room: false,
    });
    common::drain(manager.subscribe(&run_id)).await;

    let requests = port.requests();
    let system = system_text(&requests[0].messages);
    assert!(
        system.contains("LIVE-FACT-MARKER"),
        "a live fact must still be recalled"
    );
    assert!(
        !system.contains("EXPIRED-NOTE-MARKER"),
        "an expired note must not appear in the next turn's prompt"
    );
}

#[tokio::test]
async fn recall_headers_appear_in_precedence_order() {
    let db = open_db();
    seed_bot(&db, "t", "T");
    let project = {
        let locked = db.lock().unwrap();
        let project = store::create_project(&locked, "Zenith").unwrap();
        store::add_project_member(&locked, &project.id, "t").unwrap();
        store::remember(&locked, "t", "OWN-TIER-MARKER", "bot").unwrap();
        store::remember_scoped(
            &locked,
            "t",
            "PROJECT-TIER-MARKER",
            Scope::Project,
            Some(&project.id),
        )
        .unwrap();
        store::remember_scoped(&locked, "t", "SHARED-TIER-MARKER", Scope::Shared, None).unwrap();
        project
    };

    let bot = {
        let locked = db.lock().unwrap();
        store::get_bot(&locked, "t").unwrap().unwrap()
    };
    let messages = server::prompt::build_prompt(&db.lock().unwrap(), &bot, &[]);

    let conversation_id = own_conversation(&db, "t");
    seed_user_message(&db, &conversation_id, "go");
    let port = Arc::new(ScriptedPort::new(vec![vec![
        ModelEvent::Delta {
            text: "ok".to_string(),
        },
        ModelEvent::Done {
            model: "test/model".to_string(),
            usage: None,
            finish_reason: None,
        },
    ]]));
    let manager = Arc::new(RunManager::new(Arc::clone(&db), as_port_arc(&port)));
    let run_id = manager.start(StartOptions {
        bot_id: "t".to_string(),
        conversation_id,
        model: "test/model".to_string(),
        messages,
        trigger: Trigger::Chat,
        room: false,
    });
    common::drain(manager.subscribe(&run_id)).await;

    let requests = port.requests();
    let system = system_text(&requests[0].messages);

    let own_header = system.find("## What you know").expect("own tier header");
    let project_header = system
        .find(&format!("## Project: {}", project.name))
        .expect("project tier header");
    let shared_header = system.find("## Shared").expect("shared tier header");

    assert!(
        own_header < project_header,
        "own tier must precede project tier"
    );
    assert!(
        project_header < shared_header,
        "project tier must precede shared tier"
    );
    assert!(system.contains("OWN-TIER-MARKER"));
    assert!(system.contains("PROJECT-TIER-MARKER"));
    assert!(system.contains("SHARED-TIER-MARKER"));
}
