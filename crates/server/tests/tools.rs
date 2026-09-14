//! S1-F-08 acceptance (F3, F9, F10): `message_bot`, `remember` and
//! `add_to_room` as tool calls a run actually makes. Drives `RunManager`
//! directly, never HTTP - same reason `tests/runs.rs` does: these are
//! single-run tool-loop behaviours, not routes, and S1-06's room-round
//! engine (`RoomEngine`) plays no part in any of them. F13's room-title
//! acceptance and F2's room-floor case live in `tests/rooms.rs` instead,
//! since those need the real round engine wired onto `build_app`.
//!
//! F13: before this file (and `tests/rooms.rs`'s two new cases), neither
//! `add_to_room` nor `message_bot` was referenced by any test in the
//! workspace - a regression in either was caught by nothing.

mod common;

use std::sync::{Arc, Mutex};

use common::{ScriptedPort, as_port, text_script};
use model::ladder::Trigger;
use model::{ModelEvent, ModelMessage, ModelUsage, ToolCall};
use serde_json::json;
use server::runs::{RunEvent, RunManager, StartOptions};
use store::Db;

fn open_db() -> Arc<Mutex<Db>> {
    // S2-04: routing defaults to enabled (S2-01); disable it so a
    // scripted test's first reply isn't consumed by the classifier call
    // instead of the turn it scripted it for.
    let db = Db::open(":memory:").expect("open :memory: db");
    model::routing::set_routing_settings(&db, Some(false), None)
        .expect("disable routing classifier for scripted-model tests");
    Arc::new(Mutex::new(db))
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

fn create_room(db: &Arc<Mutex<Db>>, title: &str, member_ids: &[&str]) {
    let db = db.lock().expect("db mutex poisoned");
    let ids: Vec<String> = member_ids.iter().map(|s| s.to_string()).collect();
    store::create_room(&db, title, &ids).expect("create_room");
}

/// The run row's own `cost_usd` - F3's whole point: whatever a delegated
/// `message_bot` call spent has to land here too, not just the run's own
/// model steps.
fn run_cost(db: &Arc<Mutex<Db>>, run_id: &str) -> f64 {
    let db = db.lock().expect("db mutex poisoned");
    db.conn()
        .query_row(
            "SELECT cost_usd FROM runs WHERE id = ?1",
            rusqlite::params![run_id],
            |row| row.get(0),
        )
        .expect("read run cost")
}

/// Every fact `remember` has written for one bot, oldest first - there is no
/// public store query for this table, so this reads it the same raw-SQL way
/// `tests/runs.rs`'s `run_row` reads the `runs` table.
fn memory_log_facts(db: &Arc<Mutex<Db>>, bot_id: &str) -> Vec<String> {
    let db = db.lock().expect("db mutex poisoned");
    let mut stmt = db
        .conn()
        .prepare(
            "SELECT content FROM memory_log WHERE bot_id = ?1 ORDER BY created_at ASC, rowid ASC",
        )
        .expect("prepare memory_log query");
    stmt.query_map(rusqlite::params![bot_id], |row| row.get(0))
        .expect("query memory_log")
        .collect::<Result<_, _>>()
        .expect("collect memory_log rows")
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

fn tool_result<'a>(events: &'a [RunEvent], tool: &str) -> Option<&'a str> {
    events.iter().find_map(|e| match e {
        RunEvent::ToolResult { name, result } if name == tool => Some(result.as_str()),
        _ => None,
    })
}

fn tool_results<'a>(events: &'a [RunEvent], tool: &str) -> Vec<&'a str> {
    events
        .iter()
        .filter_map(|e| match e {
            RunEvent::ToolResult { name, result } if name == tool => Some(result.as_str()),
            _ => None,
        })
        .collect()
}

fn tool_call(id: &str, name: &str, arguments: String) -> Vec<ModelEvent> {
    vec![ModelEvent::ToolCalls {
        calls: vec![ToolCall {
            id: id.to_string(),
            name: name.to_string(),
            arguments,
        }],
        usage: None,
    }]
}

// 1. F3: a `message_bot` call to a colleague that itself billed real money
//    folds that spend into the CALLER's own run row - not just the caller's
//    own model steps. Bite: drop the fold in `runs.rs`'s tool loop and this
//    reads 0.004 (the caller's two steps only), never the colleague's 0.02.
#[tokio::test]
async fn message_bots_delegated_spend_folds_into_the_callers_run_cost() {
    let db = open_db();
    seed_bot(&db, "arthur", "Arthur");
    seed_bot(&db, "jason", "Jason");
    let conversation_id = own_conversation(&db, "arthur");
    seed_user_message(&db, &conversation_id, "check the numbers with jason");

    let ask_jason = json!({"bot": "Jason", "question": "check the numbers"}).to_string();
    let port = ScriptedPort::new(vec![
        tool_call("c1", "message_bot", ask_jason), // arthur's step 1: asks Jason
        vec![
            ModelEvent::Delta {
                text: "Looks fine.".to_string(),
            },
            ModelEvent::Done {
                model: "test/model".to_string(),
                usage: Some(ModelUsage {
                    cost_usd: 0.02,
                    input_tokens: 300,
                    output_tokens: 40,
                    cached_tokens: 0,
                }),
                finish_reason: None,
            },
        ], // the delegated call TO Jason
        vec![
            ModelEvent::Delta {
                text: "ok".to_string(),
            },
            ModelEvent::Done {
                model: "test/model".to_string(),
                usage: Some(ModelUsage {
                    cost_usd: 0.004,
                    input_tokens: 60,
                    output_tokens: 10,
                    cached_tokens: 0,
                }),
                finish_reason: None,
            },
        ], // arthur's step 2: final answer
    ]);

    let manager = Arc::new(RunManager::new(Arc::clone(&db), as_port(port)));
    let run_id = manager.start(StartOptions {
        bot_id: "arthur".to_string(),
        conversation_id: conversation_id.clone(),
        model: "test/model".to_string(),
        messages: vec![ModelMessage::user("check the numbers with jason")],
        trigger: Trigger::Chat,
        room: false,
    });
    drain(manager.subscribe(&run_id)).await;

    let cost = run_cost(&db, &run_id);
    assert!(
        (cost - 0.024).abs() < 1e-9,
        "expected the delegated call's 0.02 folded into the caller's 0.004, got {cost}"
    );
}

// 2. F9: the tool's argument names are Bullpen's own (`bot`/`question`,
//    `app.ts:5028-5040`), not the pre-fix Rust names (`to`/`message`) - the
//    old shape no longer parses.
#[tokio::test]
async fn message_bot_only_accepts_bullpens_own_argument_names() {
    let db = open_db();
    seed_bot(&db, "arthur", "Arthur");
    seed_bot(&db, "jason", "Jason");
    let conversation_id = own_conversation(&db, "arthur");
    seed_user_message(&db, &conversation_id, "ask jason");

    let old_shape = json!({"to": "Jason", "message": "check the numbers"}).to_string();
    let port = ScriptedPort::new(vec![
        tool_call("c1", "message_bot", old_shape),
        text_script("noted"),
    ]);
    let manager = Arc::new(RunManager::new(Arc::clone(&db), as_port(port)));
    let run_id = manager.start(StartOptions {
        bot_id: "arthur".to_string(),
        conversation_id: conversation_id.clone(),
        model: "test/model".to_string(),
        messages: vec![ModelMessage::user("ask jason")],
        trigger: Trigger::Chat,
        room: false,
    });

    let events = drain(manager.subscribe(&run_id)).await;
    assert_eq!(
        tool_result(&events, "message_bot"),
        Some("Could not read `bot`/`question`."),
        "got {events:?}"
    );
}

// 3. F10: a bot targeting itself never reaches a second, paid model call.
#[tokio::test]
async fn message_bot_refuses_to_target_the_caller_itself() {
    let db = open_db();
    seed_bot(&db, "arthur", "Arthur");
    let conversation_id = own_conversation(&db, "arthur");
    seed_user_message(&db, &conversation_id, "talk to yourself");

    let self_call = json!({"bot": "Arthur", "question": "what do you think?"}).to_string();
    let port = Arc::new(ScriptedPort::new(vec![
        tool_call("c1", "message_bot", self_call),
        text_script("noted"),
    ]));
    let manager = Arc::new(RunManager::new(
        Arc::clone(&db),
        Arc::clone(&port) as Arc<dyn model::ModelPort>,
    ));
    let run_id = manager.start(StartOptions {
        bot_id: "arthur".to_string(),
        conversation_id: conversation_id.clone(),
        model: "test/model".to_string(),
        messages: vec![ModelMessage::user("talk to yourself")],
        trigger: Trigger::Chat,
        room: false,
    });

    let events = drain(manager.subscribe(&run_id)).await;
    assert_eq!(
        tool_result(&events, "message_bot"),
        Some("That is you. Answer it yourself.")
    );
    assert_eq!(
        port.requests().len(),
        2,
        "self-target must never reach a second, paid model call - got {:?}",
        port.requests()
    );
}

// 4. F10: an unresolvable name hands back the whole roster so the model can
//    retry, instead of a dead end (`app.ts:6076-6078`).
#[tokio::test]
async fn message_bot_hands_back_the_roster_when_the_name_misses() {
    let db = open_db();
    seed_bot(&db, "arthur", "Arthur");
    seed_bot(&db, "jason", "Jason");
    let conversation_id = own_conversation(&db, "arthur");
    seed_user_message(&db, &conversation_id, "ask nobody");

    let miss = json!({"bot": "Nobody", "question": "anything?"}).to_string();
    let port = ScriptedPort::new(vec![
        tool_call("c1", "message_bot", miss),
        text_script("noted"),
    ]);
    let manager = Arc::new(RunManager::new(Arc::clone(&db), as_port(port)));
    let run_id = manager.start(StartOptions {
        bot_id: "arthur".to_string(),
        conversation_id: conversation_id.clone(),
        model: "test/model".to_string(),
        messages: vec![ModelMessage::user("ask nobody")],
        trigger: Trigger::Chat,
        room: false,
    });

    let events = drain(manager.subscribe(&run_id)).await;
    let result = tool_result(&events, "message_bot").expect("expected a message_bot tool result");
    assert!(
        result.starts_with("There is no bot called that. The roster is:"),
        "got: {result}"
    );
    assert!(result.contains("Arthur"), "got: {result}");
    assert!(result.contains("Jason"), "got: {result}");
}

// 5. F9: `remember`'s argument is `fact` (`app.ts:7207-7217`), not the
//    pre-fix `content` - and what gets written is exactly what was asked.
#[tokio::test]
async fn remember_only_accepts_the_fact_argument() {
    let db = open_db();
    seed_bot(&db, "arthur", "Arthur");
    let conversation_id = own_conversation(&db, "arthur");
    seed_user_message(&db, &conversation_id, "remember this");

    let old_shape = json!({"content": "Josh prefers dark mode."}).to_string();
    let new_shape = json!({"fact": "Josh prefers dark mode."}).to_string();
    let port = ScriptedPort::new(vec![
        tool_call("c1", "remember", old_shape),
        tool_call("c2", "remember", new_shape),
        text_script("noted"),
    ]);
    let manager = Arc::new(RunManager::new(Arc::clone(&db), as_port(port)));
    let run_id = manager.start(StartOptions {
        bot_id: "arthur".to_string(),
        conversation_id: conversation_id.clone(),
        model: "test/model".to_string(),
        messages: vec![ModelMessage::user("remember this")],
        trigger: Trigger::Chat,
        room: false,
    });

    let events = drain(manager.subscribe(&run_id)).await;
    let results = tool_results(&events, "remember");
    assert_eq!(
        results,
        vec!["Could not read `fact`.", "Remembered."],
        "got {events:?}"
    );
    assert_eq!(
        memory_log_facts(&db, "arthur"),
        vec!["Josh prefers dark mode.".to_string()]
    );
}

// 6. F13: `add_to_room` past the six-bot cap refuses with the same message
//    `check_roster` already gives a route (`store::rooms::check_roster`).
#[tokio::test]
async fn add_to_room_refuses_past_the_six_cap() {
    let db = open_db();
    seed_bot(&db, "arthur", "Arthur");
    for i in 1..=5 {
        seed_bot(&db, &format!("b{i}"), &format!("Bot{i}"));
    }
    seed_bot(&db, "extra", "Extra");
    create_room(&db, "Bench", &["arthur", "b1", "b2", "b3", "b4", "b5"]);
    let conversation_id = own_conversation(&db, "arthur");
    seed_user_message(&db, &conversation_id, "add extra to bench");

    let add = json!({"room": "Bench", "bot": "Extra"}).to_string();
    let port = ScriptedPort::new(vec![
        tool_call("c1", "add_to_room", add),
        text_script("noted"),
    ]);
    let manager = Arc::new(RunManager::new(Arc::clone(&db), as_port(port)));
    let run_id = manager.start(StartOptions {
        bot_id: "arthur".to_string(),
        conversation_id: conversation_id.clone(),
        model: "test/model".to_string(),
        messages: vec![ModelMessage::user("add extra to bench")],
        trigger: Trigger::Chat,
        room: false,
    });

    let events = drain(manager.subscribe(&run_id)).await;
    assert_eq!(
        tool_result(&events, "add_to_room"),
        Some("A group chat can have at most six bots."),
        "got {events:?}"
    );
}

// 7. F13: `add_to_room` on a bot already in the room says so rather than
//    silently no-op-ing.
#[tokio::test]
async fn add_to_room_reports_an_existing_member() {
    let db = open_db();
    seed_bot(&db, "arthur", "Arthur");
    seed_bot(&db, "riley", "Riley");
    create_room(&db, "Pair", &["arthur", "riley"]);
    let conversation_id = own_conversation(&db, "arthur");
    seed_user_message(&db, &conversation_id, "add riley again");

    let add = json!({"room": "Pair", "bot": "Riley"}).to_string();
    let port = ScriptedPort::new(vec![
        tool_call("c1", "add_to_room", add),
        text_script("noted"),
    ]);
    let manager = Arc::new(RunManager::new(Arc::clone(&db), as_port(port)));
    let run_id = manager.start(StartOptions {
        bot_id: "arthur".to_string(),
        conversation_id: conversation_id.clone(),
        model: "test/model".to_string(),
        messages: vec![ModelMessage::user("add riley again")],
        trigger: Trigger::Chat,
        room: false,
    });

    let events = drain(manager.subscribe(&run_id)).await;
    assert_eq!(
        tool_result(&events, "add_to_room"),
        Some("Riley is already in \"Pair\"."),
        "got {events:?}"
    );
}
