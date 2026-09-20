//! S10-08: `hire_bot` parks for approval, creates on approve, refuses dupes.

mod common;

use std::sync::{Arc, Mutex};
use std::time::Duration;

use common::{ScriptedPort, as_port, own_conversation, seed_bot, seed_user_message};
use model::ladder::Trigger;
use model::{MessageContent, ModelEvent, ModelMessage, ToolCall};
use server::runs::{RunEvent, RunManager, StartOptions};
use store::Db;

fn open_db() -> Arc<Mutex<Db>> {
    let db = Db::open(":memory:").expect("open :memory: db");
    model::routing::set_routing_settings(&db, Some(false), None)
        .expect("disable routing classifier");
    server::judge::set_judge_enabled(&db, false).expect("disable judge");
    Arc::new(Mutex::new(db))
}

fn hire_then_answer(args: &str) -> ScriptedPort {
    ScriptedPort::new(vec![
        vec![ModelEvent::ToolCalls {
            calls: vec![ToolCall {
                id: "c1".to_string(),
                name: "hire_bot".to_string(),
                arguments: args.to_string(),
            }],
            usage: None,
        }],
        vec![
            ModelEvent::Delta {
                text: "done".to_string(),
            },
            ModelEvent::Done {
                model: "test/model".to_string(),
                usage: None,
                finish_reason: None,
            },
        ],
    ])
}

async fn drain_until_paused(
    mut rx: tokio::sync::mpsc::UnboundedReceiver<RunEvent>,
) -> Vec<RunEvent> {
    let mut seen = Vec::new();
    while let Some(event) = rx.recv().await {
        let stop = matches!(
            event,
            RunEvent::Done { .. } | RunEvent::Error { .. } | RunEvent::ApprovalNeeded { .. }
        );
        seen.push(event);
        if stop {
            break;
        }
    }
    seen
}

fn run_status(db: &Arc<Mutex<Db>>, run_id: &str) -> String {
    let db = db.lock().expect("db mutex poisoned");
    db.conn()
        .query_row(
            "SELECT status FROM runs WHERE id = ?1",
            rusqlite::params![run_id],
            |row| row.get(0),
        )
        .expect("read run row")
}

async fn wait_for_status(db: &Arc<Mutex<Db>>, run_id: &str, target: &str) {
    let mut status = run_status(db, run_id);
    for _ in 0..300 {
        if status == target {
            return;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
        status = run_status(db, run_id);
    }
    panic!("run {run_id} stayed at {status}, wanted {target}");
}

fn pending_approval(db: &Arc<Mutex<Db>>, run_id: &str) -> Option<String> {
    let db = db.lock().expect("db mutex poisoned");
    db.conn()
        .query_row(
            "SELECT id FROM approvals WHERE run_id = ?1",
            rusqlite::params![run_id],
            |row| row.get(0),
        )
        .ok()
}

fn bot_count_named(db: &Arc<Mutex<Db>>, name: &str) -> i64 {
    let db = db.lock().expect("db mutex poisoned");
    db.conn()
        .query_row(
            "SELECT COUNT(*) FROM bots WHERE lower(name) = lower(?1)",
            rusqlite::params![name],
            |row| row.get(0),
        )
        .expect("count bots")
}

fn tool_result_for_call(db: &Arc<Mutex<Db>>, run_id: &str, call_id: &str) -> Option<String> {
    let db = db.lock().expect("db mutex poisoned");
    let messages_json: String = db
        .conn()
        .query_row(
            "SELECT messages FROM runs WHERE id = ?1",
            rusqlite::params![run_id],
            |row| row.get(0),
        )
        .expect("read messages");
    let messages: Vec<ModelMessage> = serde_json::from_str(&messages_json).expect("parse messages");
    messages.iter().find_map(|m| {
        if m.role != "tool" {
            return None;
        }
        if m.tool_call_id.as_deref() != Some(call_id) {
            return None;
        }
        let MessageContent::Text(text) = &m.content else {
            return None;
        };
        Some(text.clone())
    })
}

#[tokio::test]
async fn hire_bot_parks_before_roster_changes() {
    let db = open_db();
    seed_bot(&db, "arthur", "Arthur");
    let conversation_id = own_conversation(&db, "arthur");
    seed_user_message(&db, &conversation_id, "hire Riley");

    let args = r#"{"name":"Riley","purpose":"Research","instructions":"Dig."}"#;
    let manager = Arc::new(RunManager::new(
        Arc::clone(&db),
        as_port(hire_then_answer(args)),
    ));
    let run_id = manager.start(StartOptions {
        bot_id: "arthur".to_string(),
        conversation_id,
        model: "test/model".to_string(),
        messages: vec![ModelMessage::user("hire Riley")],
        trigger: Trigger::Chat,
        room: false,
    });

    drain_until_paused(manager.subscribe(&run_id)).await;
    assert!(pending_approval(&db, &run_id).is_some());
    assert_eq!(bot_count_named(&db, "Riley"), 0);
}

#[tokio::test]
async fn approving_hire_bot_creates_roster_entry() {
    let db = open_db();
    seed_bot(&db, "arthur", "Arthur");
    let conversation_id = own_conversation(&db, "arthur");
    seed_user_message(&db, &conversation_id, "hire Riley");

    let args = r#"{"name":"Riley","purpose":"Research","instructions":"Dig into things."}"#;
    let manager = Arc::new(RunManager::new(
        Arc::clone(&db),
        as_port(hire_then_answer(args)),
    ));
    let run_id = manager.start(StartOptions {
        bot_id: "arthur".to_string(),
        conversation_id,
        model: "test/model".to_string(),
        messages: vec![ModelMessage::user("hire Riley")],
        trigger: Trigger::Chat,
        room: false,
    });

    drain_until_paused(manager.subscribe(&run_id)).await;
    let approval_id = pending_approval(&db, &run_id).expect("approval");
    assert!(manager.decide_approval(&approval_id, true, None).await);
    wait_for_status(&db, &run_id, "done").await;

    assert_eq!(bot_count_named(&db, "Riley"), 1);
    let result = tool_result_for_call(&db, &run_id, "c1").expect("tool result");
    assert!(result.contains("live on the roster"));
    assert!(result.contains("riley") || result.contains("Riley"));
}

#[tokio::test]
async fn duplicate_hire_name_refused_on_approve() {
    let db = open_db();
    seed_bot(&db, "arthur", "Arthur");
    let conversation_id = own_conversation(&db, "arthur");
    seed_user_message(&db, &conversation_id, "hire Arthur");

    let args = r#"{"name":"Arthur","purpose":"Dup","instructions":"..."}"#;
    let manager = Arc::new(RunManager::new(
        Arc::clone(&db),
        as_port(hire_then_answer(args)),
    ));
    let run_id = manager.start(StartOptions {
        bot_id: "arthur".to_string(),
        conversation_id,
        model: "test/model".to_string(),
        messages: vec![ModelMessage::user("hire Arthur")],
        trigger: Trigger::Chat,
        room: false,
    });

    drain_until_paused(manager.subscribe(&run_id)).await;
    let approval_id = pending_approval(&db, &run_id).expect("approval");
    assert!(manager.decide_approval(&approval_id, true, None).await);
    wait_for_status(&db, &run_id, "done").await;

    let result = tool_result_for_call(&db, &run_id, "c1").expect("tool result");
    assert!(result.to_lowercase().contains("already"));
    assert_eq!(bot_count_named(&db, "Arthur"), 1);
}
