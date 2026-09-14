//! S1-05 acceptance: the run manager, the tool loop, and the first six
//! tools. Drives `RunManager` directly, never HTTP - routes are S1-06.
//!
//! Multi-turn scripts (a tool call, then a final answer) use a small
//! `ScriptedPort`/`GatedPort` defined below rather than
//! `model::fake::FakePort`: `FakePort` replays one fixed event list on
//! EVERY call, which is right for a single-call answer but cannot express
//! "the first model call asks for a tool, the second answers" - exactly
//! what a mid-run tool call needs. This mirrors how the TS source
//! (`test/working.test.ts`, `test/rooms.test.ts`) tests the same thing: an
//! ad hoc `ModelPort` object with its own turn counter, not the shared
//! fake-port helper.

mod common;

use std::sync::{Arc, Mutex};

use common::{GatedPort, ScriptedPort, as_port};
use model::ladder::Trigger;
use model::{ModelEvent, ModelMessage, ToolCall};
use server::changes::ChangeKind;
use server::runs::{RunEvent, RunManager, StartOptions};
use store::Db;

fn open_db() -> Arc<Mutex<Db>> {
    Arc::new(Mutex::new(Db::open(":memory:").expect("open :memory: db")))
}

fn seed_bot(db: &Arc<Mutex<Db>>, id: &str, name: &str) {
    let db = db.lock().expect("db mutex poisoned");
    db.conn()
        .execute(
            "INSERT INTO bots (id, name, purpose, instructions, model, created_at) VALUES (?1, ?2, '', ?3, NULL, '2026-01-01T00:00:00Z')",
            rusqlite::params![id, name, format!("You are {name}.")],
        )
        .expect("seed bot");
}

fn own_conversation(db: &Arc<Mutex<Db>>, bot_id: &str) -> String {
    let db = db.lock().expect("db mutex poisoned");
    store::get_or_create_conversation(&db, bot_id).expect("get_or_create_conversation")
}

fn seed_user_message(db: &Arc<Mutex<Db>>, conversation_id: &str, text: &str) {
    let db = db.lock().expect("db mutex poisoned");
    store::append_message(
        &db,
        conversation_id,
        "user",
        text,
        store::NewMessage::default(),
    )
    .expect("append user message");
}

fn run_row(db: &Arc<Mutex<Db>>, run_id: &str) -> (String, Option<String>) {
    let db = db.lock().expect("db mutex poisoned");
    db.conn()
        .query_row(
            "SELECT status, error FROM runs WHERE id = ?1",
            rusqlite::params![run_id],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .expect("read run row")
}

/// Drains a run's events until `Done`/`Error`, returning everything seen.
async fn drain(mut rx: tokio::sync::mpsc::UnboundedReceiver<RunEvent>) -> Vec<RunEvent> {
    let mut seen = Vec::new();
    while let Some(event) = rx.recv().await {
        let done = matches!(event, RunEvent::Done { .. } | RunEvent::Error { .. });
        seen.push(event);
        if done {
            break;
        }
    }
    seen
}

// 1. A text reply: run row goes running -> done, assistant message appended
//    with the text, events seen by a subscriber: delta(s) then done.
#[tokio::test]
async fn text_reply_settles_to_done_and_delivers_the_message() {
    let db = open_db();
    seed_bot(&db, "arthur", "Arthur");
    let conversation_id = own_conversation(&db, "arthur");
    seed_user_message(&db, &conversation_id, "have a look at this");

    let manager = Arc::new(RunManager::new(
        Arc::clone(&db),
        as_port(model::fake::text_port("Hello, Josh.", "test/model")),
    ));

    let run_id = manager.start(StartOptions {
        bot_id: "arthur".to_string(),
        conversation_id: conversation_id.clone(),
        model: "test/model".to_string(),
        messages: vec![ModelMessage::user("have a look at this")],
        trigger: Trigger::Chat,
        room: false,
    });

    let events = drain(manager.subscribe(&run_id)).await;
    assert!(
        events.iter().any(|e| matches!(e, RunEvent::Delta { .. })),
        "expected at least one delta event, got {events:?}"
    );
    assert!(
        matches!(events.last(), Some(RunEvent::Done { model, .. }) if model == "test/model"),
        "expected the run to end on a done event, got {events:?}"
    );

    let (status, error) = run_row(&db, &run_id);
    assert_eq!(status, "done");
    assert_eq!(error, None);

    let messages = {
        let db = db.lock().unwrap();
        store::list_messages(&db, &conversation_id).unwrap()
    };
    let last = messages.last().expect("at least one message");
    assert_eq!(last.role, "assistant");
    assert_eq!(last.content, "Hello, Josh.");
}

// 2. A `say` tool call mid-run posts to the bot's own thread and the run
//    continues to a final answer (two model turns).
#[tokio::test]
async fn say_tool_call_mid_run_posts_and_the_run_continues() {
    let db = open_db();
    seed_bot(&db, "arthur", "Arthur");
    let conversation_id = own_conversation(&db, "arthur");
    seed_user_message(&db, &conversation_id, "have a look at this");

    let port = ScriptedPort::new(vec![
        vec![ModelEvent::ToolCalls {
            calls: vec![ToolCall {
                id: "c1".to_string(),
                name: "say".to_string(),
                arguments: "{\"text\":\"Progress note.\"}".to_string(),
            }],
            usage: None,
        }],
        vec![
            ModelEvent::Delta {
                text: "Final answer.".to_string(),
            },
            ModelEvent::Done {
                model: "test/model".to_string(),
                usage: None,
                finish_reason: None,
            },
        ],
    ]);

    let manager = Arc::new(RunManager::new(Arc::clone(&db), as_port(port)));
    let run_id = manager.start(StartOptions {
        bot_id: "arthur".to_string(),
        conversation_id: conversation_id.clone(),
        model: "test/model".to_string(),
        messages: vec![ModelMessage::user("have a look at this")],
        trigger: Trigger::Chat,
        room: false,
    });

    let events = drain(manager.subscribe(&run_id)).await;
    assert!(
        events
            .iter()
            .any(|e| matches!(e, RunEvent::ToolCall { name, .. } if name == "say")),
        "expected a say tool_call event, got {events:?}"
    );
    assert!(matches!(events.last(), Some(RunEvent::Done { .. })));

    let (status, _) = run_row(&db, &run_id);
    assert_eq!(status, "done");

    let messages = {
        let db = db.lock().unwrap();
        store::list_messages(&db, &conversation_id).unwrap()
    };
    let texts: Vec<&str> = messages.iter().map(|m| m.content.as_str()).collect();
    assert!(
        texts.contains(&"Progress note."),
        "expected the say tool's post in the thread, got {texts:?}"
    );
    assert!(
        texts.contains(&"Final answer."),
        "expected the run to continue to a final answer, got {texts:?}"
    );
}

// 3. `create_room("Growth", [arthur, riley, jason])` from a run creates a
//    room the store lists, with `arthur` as owner; a 7th member -> tool
//    result contains "at most six" and no room is created.
#[tokio::test]
async fn create_room_tool_creates_a_room_with_the_caller_as_owner() {
    let db = open_db();
    seed_bot(&db, "arthur", "Arthur");
    seed_bot(&db, "riley", "Riley");
    seed_bot(&db, "jason", "Jason");
    let conversation_id = own_conversation(&db, "arthur");

    let args = serde_json::json!({ "title": "Growth", "member_ids": ["arthur", "riley", "jason"] })
        .to_string();
    let port = ScriptedPort::new(vec![
        vec![ModelEvent::ToolCalls {
            calls: vec![ToolCall {
                id: "c1".to_string(),
                name: "create_room".to_string(),
                arguments: args,
            }],
            usage: None,
        }],
        vec![
            ModelEvent::Delta {
                text: "Made the room.".to_string(),
            },
            ModelEvent::Done {
                model: "test/model".to_string(),
                usage: None,
                finish_reason: None,
            },
        ],
    ]);

    let manager = Arc::new(RunManager::new(Arc::clone(&db), as_port(port)));
    let run_id = manager.start(StartOptions {
        bot_id: "arthur".to_string(),
        conversation_id,
        model: "test/model".to_string(),
        messages: vec![ModelMessage::user("start a room with riley and jason")],
        trigger: Trigger::Chat,
        room: false,
    });
    drain(manager.subscribe(&run_id)).await;

    let rooms = {
        let db = db.lock().unwrap();
        store::list_rooms(&db).unwrap()
    };
    let growth = rooms
        .iter()
        .find(|r| r.title == "Growth")
        .expect("Growth room exists");
    assert_eq!(
        growth.member_ids.first().map(String::as_str),
        Some("arthur")
    );
    assert!(growth.member_ids.contains(&"riley".to_string()));
    assert!(growth.member_ids.contains(&"jason".to_string()));
}

#[tokio::test]
async fn create_room_tool_refuses_a_seventh_member() {
    let db = open_db();
    seed_bot(&db, "arthur", "Arthur");
    let conversation_id = own_conversation(&db, "arthur");

    let args = serde_json::json!({
        "title": "Too Big",
        "member_ids": ["b1", "b2", "b3", "b4", "b5", "b6", "b7"]
    })
    .to_string();
    let port = ScriptedPort::new(vec![
        vec![ModelEvent::ToolCalls {
            calls: vec![ToolCall {
                id: "c1".to_string(),
                name: "create_room".to_string(),
                arguments: args,
            }],
            usage: None,
        }],
        vec![
            ModelEvent::Delta {
                text: "Could not make the room.".to_string(),
            },
            ModelEvent::Done {
                model: "test/model".to_string(),
                usage: None,
                finish_reason: None,
            },
        ],
    ]);

    let manager = Arc::new(RunManager::new(Arc::clone(&db), as_port(port)));
    let run_id = manager.start(StartOptions {
        bot_id: "arthur".to_string(),
        conversation_id,
        model: "test/model".to_string(),
        messages: vec![ModelMessage::user("start a room with seven others")],
        trigger: Trigger::Chat,
        room: false,
    });
    let events = drain(manager.subscribe(&run_id)).await;

    let tool_result = events.iter().find_map(|e| match e {
        RunEvent::ToolResult { name, result } if name == "create_room" => Some(result.clone()),
        _ => None,
    });
    assert!(
        tool_result
            .as_deref()
            .is_some_and(|r| r.contains("at most six")),
        "expected the tool result to say 'at most six', got {tool_result:?}"
    );

    let rooms = {
        let db = db.lock().unwrap();
        store::list_rooms(&db).unwrap()
    };
    assert!(
        rooms.iter().all(|r| r.title != "Too Big"),
        "no room should have been created, got {rooms:?}"
    );
}

// 4. `working(cid)` twins `working.test.ts`: reports the bot while in
//    flight with "Thinking"; "Reading its checklist"-style phrasing after a
//    tool call (port `activity_line` from `src/shared/working.ts` into
//    `shared`); empty after settle; announces "working" on the change bus
//    when the LINE changes mid-run (the gated count, not `contains`).
#[tokio::test]
async fn working_reports_thinking_then_tool_phrasing_then_empties_and_gates_the_touch_count() {
    let db = open_db();
    seed_bot(&db, "arthur", "Arthur");
    let conversation_id = own_conversation(&db, "arthur");

    let (gate_tx, gate_rx) = tokio::sync::oneshot::channel();
    let (held_tx, held_rx) = tokio::sync::oneshot::channel();
    let port = GatedPort {
        turn: Mutex::new(0),
        gate: Mutex::new(Some(gate_rx)),
        held: Mutex::new(Some(held_rx)),
    };

    let manager = Arc::new(RunManager::new(Arc::clone(&db), as_port(port)));

    let kinds: Arc<Mutex<Vec<ChangeKind>>> = Arc::new(Mutex::new(Vec::new()));
    let kinds_clone = Arc::clone(&kinds);
    manager
        .changes
        .subscribe(move |kind| kinds_clone.lock().unwrap().push(kind));

    let run_id = manager.start(StartOptions {
        bot_id: "arthur".to_string(),
        conversation_id: conversation_id.clone(),
        model: "test/model".to_string(),
        messages: vec![ModelMessage::user("what is on your list")],
        trigger: Trigger::Chat,
        room: false,
    });

    // The row exists the instant `start` returns, before the spawned task
    // has run at all - the indicator must not wait a beat to appear.
    let mid = manager.working(&conversation_id);
    assert_eq!(mid.len(), 1);
    assert_eq!(mid[0].name, "Arthur");
    assert!(!mid[0].waiting);
    assert_eq!(mid[0].activity, "Thinking");

    let working_touches = |kinds: &Arc<Mutex<Vec<ChangeKind>>>| {
        kinds
            .lock()
            .unwrap()
            .iter()
            .filter(|k| **k == ChangeKind::Working)
            .count()
    };
    let at_start = working_touches(&kinds);

    // Let turn 1 through; it asks for `list_tasks` and turn 2 stays held, so
    // the run sits on the tool-call activity line until released below.
    let _ = gate_tx.send(());
    let mut phrased = false;
    for _ in 0..200 {
        let seen = manager.working(&conversation_id);
        if seen.first().map(|b| b.activity.as_str()) == Some("Reading its checklist") {
            phrased = true;
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(5)).await;
    }
    assert!(
        phrased,
        "expected the activity line to read 'Reading its checklist'"
    );

    let after_tool = working_touches(&kinds);
    assert!(
        after_tool > at_start,
        "expected the tool call to announce on the change bus (gated count), at_start={at_start} after_tool={after_tool}"
    );

    let _ = held_tx.send(());
    drain(manager.subscribe(&run_id)).await;

    assert!(
        manager.working(&conversation_id).is_empty(),
        "expected nobody working once the run settled"
    );
}

// 5. `stop(run_id)` between steps -> status failed, error "Stopped.".
#[tokio::test]
async fn stop_fails_the_run_with_stopped() {
    let db = open_db();
    seed_bot(&db, "arthur", "Arthur");
    let conversation_id = own_conversation(&db, "arthur");

    // Never actually reached if `stop` wins the race, as it always does on
    // the current-thread test runtime: `stop` runs before the spawned task
    // is ever polled, since nothing between `start` and `stop` awaits.
    let port = model::fake::text_port("should not be reached", "test/model");

    let manager = Arc::new(RunManager::new(Arc::clone(&db), as_port(port)));
    let run_id = manager.start(StartOptions {
        bot_id: "arthur".to_string(),
        conversation_id,
        model: "test/model".to_string(),
        messages: vec![ModelMessage::user("hi")],
        trigger: Trigger::Chat,
        room: false,
    });
    manager.stop(&run_id);

    let events = drain(manager.subscribe(&run_id)).await;
    assert!(
        matches!(events.last(), Some(RunEvent::Error { message, .. }) if message == "Stopped."),
        "expected a Stopped. error event, got {events:?}"
    );

    let (status, error) = run_row(&db, &run_id);
    assert_eq!(status, "failed");
    assert_eq!(error.as_deref(), Some("Stopped."));
}
