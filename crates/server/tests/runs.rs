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
    let mid = manager.working(&conversation_id).expect("working query");
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
        let seen = manager.working(&conversation_id).expect("working query");
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
        manager
            .working(&conversation_id)
            .expect("working query")
            .is_empty(),
        "expected nobody working once the run settled"
    );
}

// 5. `stop(run_id)` BEFORE the first model call (F6): `take_stop` catches it
//    before `run_turn` ever touches the model, so no text is ever produced.
//    Status failed, error "Stopped.", and NO assistant message - TS calls
//    `fail()` for exactly this case (`runs.ts:965-968`, `:1741-1752`) and
//    writes nothing at all. `POST /api/bots/arthur/messages` then
//    `POST /api/runs/:id/stop` immediately used to leave an empty assistant
//    bubble sitting in the thread; this is that scenario.
#[tokio::test]
async fn stop_before_first_step_fails_the_run_with_stopped_and_writes_no_message() {
    let db = open_db();
    seed_bot(&db, "arthur", "Arthur");
    let conversation_id = own_conversation(&db, "arthur");

    // Never actually reached: `stop` runs before the spawned task is ever
    // polled, since nothing between `start` and `stop` awaits on the
    // current-thread test runtime.
    let port = model::fake::text_port("should not be reached", "test/model");

    let manager = Arc::new(RunManager::new(Arc::clone(&db), as_port(port)));
    let run_id = manager.start(StartOptions {
        bot_id: "arthur".to_string(),
        conversation_id: conversation_id.clone(),
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

    let messages = {
        let db = db.lock().unwrap();
        store::list_messages(&db, &conversation_id).unwrap()
    };
    assert!(
        messages.iter().all(|m| m.role != "assistant"),
        "expected no assistant message for a run stopped before its first step, got {messages:?}"
    );
}

// 6. `stop(run_id)` BETWEEN steps (S1-05 acceptance 5, F20): step 1's tool
//    call runs to completion, but step 2 (the final answer, held behind its
//    own gate) never starts - `take_stop` catches it at the top of the next
//    iteration. Status failed, error "Stopped.", and (F6) still no
//    assistant message: `list_tasks` produced a tool call but no text, so
//    `state.text` is empty exactly as it is in test 5.
//
//    The version of this test that used to live here started `stop` before
//    the spawned task was ever polled, so - despite its name - it exercised
//    "stopped before the first model call" (test 5 above), not this. Moving
//    `take_stop` out of the loop body to a single pre-loop check would have
//    left it green regardless; only `rooms.rs`'s
//    `stopping_a_held_run_over_http_fails_it_with_stopped` covered the real
//    between-steps semantics. This ports that test's `GatedPort` technique
//    to drive `RunManager` directly.
#[tokio::test]
async fn stop_between_steps_fails_the_run_with_stopped_and_writes_no_message() {
    let db = open_db();
    seed_bot(&db, "arthur", "Arthur");
    let conversation_id = own_conversation(&db, "arthur");

    let (gate_tx, gate_rx) = tokio::sync::oneshot::channel();
    let (_held_tx, held_rx) = tokio::sync::oneshot::channel();
    let port = GatedPort {
        turn: Mutex::new(0),
        gate: Mutex::new(Some(gate_rx)),
        held: Mutex::new(Some(held_rx)),
    };

    let manager = Arc::new(RunManager::new(Arc::clone(&db), as_port(port)));
    let run_id = manager.start(StartOptions {
        bot_id: "arthur".to_string(),
        conversation_id: conversation_id.clone(),
        model: "test/model".to_string(),
        messages: vec![ModelMessage::user("hi")],
        trigger: Trigger::Chat,
        room: false,
    });

    // Give the spawned task a chance to reach step 1's model call and
    // suspend on the still-closed gate before we stop it - otherwise
    // `stop` could land before step 1's own `take_stop` check even runs,
    // which is test 5 above, not this.
    tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    manager.stop(&run_id);
    // Step 1 now runs: it asks for `list_tasks`, which completes (proving
    // `take_stop` is a per-iteration check, not a one-time guard) - step 2
    // is what the stop actually catches, and it is held behind `held_rx`,
    // never signaled here, so it must never be reached.
    let _ = gate_tx.send(());

    let events = drain(manager.subscribe(&run_id)).await;
    assert!(
        matches!(events.last(), Some(RunEvent::Error { message, .. }) if message == "Stopped."),
        "expected a Stopped. error event, got {events:?}"
    );

    let (status, error) = run_row(&db, &run_id);
    assert_eq!(status, "failed");
    assert_eq!(error.as_deref(), Some("Stopped."));

    let messages = {
        let db = db.lock().unwrap();
        store::list_messages(&db, &conversation_id).unwrap()
    };
    assert!(
        messages.iter().all(|m| m.role != "assistant"),
        "expected no assistant message for a run stopped between steps with no text, got {messages:?}"
    );
}
