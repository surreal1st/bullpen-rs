//! Shared test doubles for server integration tests. `ScriptedPort` replays
//! one scripted event list per call to `stream`, in order (the last script
//! repeats once calls outrun the list) - built for tests that need multiple
//! model turns to look different, which `model::fake::FakePort` cannot
//! express (it replays one fixed event list on EVERY call). `GatedPort`
//! holds two specific turns behind channels so a test can observe a run
//! mid-flight. Both log every request sent across the model boundary, same
//! as `FakePort::requests`.
//!
//! Ported out of S1-05's `tests/runs.rs` so S1-06's `tests/rooms.rs` can
//! reuse them rather than a second copy - this mirrors how the TS source
//! itself (`working.test.ts`, `rooms.test.ts`) uses an ad hoc `ModelPort`
//! with its own turn counter for exactly these cases.
//!
//! Not every item here is used by every test binary that pulls this module
//! in - `cargo test` compiles `tests/runs.rs` and `tests/rooms.rs` as
//! separate crates, and each only exercises the doubles its own scenarios
//! need.
//!
//! **Routing and the judge are ON by default** - every test-db helper that
//! calls `model::routing::set_routing_settings` disables routing to prevent
//! a scripted test's first reply from being consumed by the classifier instead
//! of the turn it scripted; the same helpers also disable the judge via
//! `server::judge::set_judge_enabled` for the same reason (the judge makes
//! an extra model call on risky tools).
#![allow(dead_code)]

use std::collections::VecDeque;
use std::sync::{Arc, Mutex};

use model::{EventStream, ModelEvent, ModelPort, ModelRequest, ToolCall};

/// S1-F-05: the `/api/*` gate now requires a session on every route except
/// the open list in `server::auth::OPEN_PATHS` - a test that used to hit a
/// protected route with no credentials at all needs BOTH a password set
/// (`is_configured`) and a valid session token, or it gets 503/401 instead
/// of whatever it used to see. Call this on a test's own `db` before
/// building the app, then attach the returned value as a `cookie` header on
/// every request the test makes.
pub fn seed_session(db: &store::Db) -> String {
    store::set_password(db, "test-password").expect("seed test password");
    let token = store::create_session(db).expect("create test session");
    format!("bullpen_session={token}")
}

/// Replays one scripted event list per call to `stream`, in order; the last
/// script repeats if `stream` is called more times than there are scripts.
pub struct ScriptedPort {
    scripts: Mutex<VecDeque<Vec<ModelEvent>>>,
    last: Vec<ModelEvent>,
    requests: Mutex<Vec<ModelRequest>>,
}

impl ScriptedPort {
    pub fn new(scripts: Vec<Vec<ModelEvent>>) -> Self {
        let last = scripts.last().cloned().unwrap_or_default();
        Self {
            scripts: Mutex::new(scripts.into()),
            last,
            requests: Mutex::new(Vec::new()),
        }
    }

    /// Every request the app sent across the model boundary, in order.
    pub fn requests(&self) -> Vec<ModelRequest> {
        self.requests
            .lock()
            .expect("scripted port request log poisoned")
            .clone()
    }
}

impl ModelPort for ScriptedPort {
    fn stream(&self, request: ModelRequest) -> EventStream {
        self.requests
            .lock()
            .expect("scripted port request log poisoned")
            .push(request);
        let mut scripts = self.scripts.lock().expect("scripts mutex poisoned");
        let events = scripts.pop_front().unwrap_or_else(|| self.last.clone());
        Box::pin(futures::stream::iter(events))
    }
}

/// A plain text-then-done script, the shape most `ScriptedPort` turns need.
pub fn text_script(text: &str) -> Vec<ModelEvent> {
    vec![
        ModelEvent::Delta {
            text: text.to_string(),
        },
        ModelEvent::Done {
            model: "test/model".to_string(),
            usage: None,
            finish_reason: None,
        },
    ]
}

/// Turn 1 waits on `gate`, then asks for `list_tasks`. Turn 2 waits on
/// `held`, then answers. Ports `heldPort`/the gated `port` from
/// `working.test.ts`, so a test can observe the run mid-tool-call.
pub struct GatedPort {
    pub turn: Mutex<usize>,
    pub gate: Mutex<Option<tokio::sync::oneshot::Receiver<()>>>,
    pub held: Mutex<Option<tokio::sync::oneshot::Receiver<()>>>,
}

impl ModelPort for GatedPort {
    fn stream(&self, _request: ModelRequest) -> EventStream {
        let mut turn = self.turn.lock().expect("turn mutex poisoned");
        *turn += 1;
        let n = *turn;
        drop(turn);

        if n == 1 {
            let rx = self.gate.lock().expect("gate mutex poisoned").take();
            Box::pin(async_stream::stream! {
                if let Some(rx) = rx { let _ = rx.await; }
                yield ModelEvent::ToolCalls {
                    calls: vec![ToolCall { id: "c1".to_string(), name: "search_memory".to_string(), arguments: "{\"query\":\"checklist\"}".to_string() }],
                    usage: None,
                };
            })
        } else {
            let rx = self.held.lock().expect("held mutex poisoned").take();
            Box::pin(async_stream::stream! {
                if let Some(rx) = rx { let _ = rx.await; }
                yield ModelEvent::Delta { text: "Nothing on it.".to_string() };
                yield ModelEvent::Done { model: "test/model".to_string(), usage: None, finish_reason: None };
            })
        }
    }
}

pub fn as_port(port: impl ModelPort + 'static) -> Arc<dyn ModelPort> {
    Arc::new(port)
}

/// Drains a run's events until `Done`/`Error`, returning everything seen.
pub async fn drain(
    mut rx: tokio::sync::mpsc::UnboundedReceiver<server::runs::RunEvent>,
) -> Vec<server::runs::RunEvent> {
    let mut seen = Vec::new();
    while let Some(event) = rx.recv().await {
        let done = matches!(
            event,
            server::runs::RunEvent::Done { .. } | server::runs::RunEvent::Error { .. }
        );
        seen.push(event);
        if done {
            break;
        }
    }
    seen
}

/// Seed a bot into the database.
pub fn seed_bot(db: &Arc<std::sync::Mutex<store::Db>>, id: &str, name: &str) {
    let db = db.lock().expect("db mutex poisoned");
    db.conn()
        .execute(
            "INSERT INTO bots (id, name, purpose, instructions, model, created_at) VALUES (?1, ?2, '', ?3, NULL, '2026-01-01T00:00:00Z')",
            rusqlite::params![id, name, format!("You are {name}.")],
        )
        .expect("seed bot");
}

/// Get or create the bot's own 1:1 conversation.
pub fn own_conversation(db: &Arc<std::sync::Mutex<store::Db>>, bot_id: &str) -> String {
    let db = db.lock().expect("db mutex poisoned");
    store::get_or_create_conversation(&db, bot_id).expect("get_or_create_conversation")
}

/// Append a user message to a conversation.
pub fn seed_user_message(db: &Arc<std::sync::Mutex<store::Db>>, conversation_id: &str, text: &str) {
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

/// Read the status and error of a run from the database.
pub fn run_row(db: &Arc<std::sync::Mutex<store::Db>>, run_id: &str) -> (String, Option<String>) {
    let db = db.lock().expect("db mutex poisoned");
    db.conn()
        .query_row(
            "SELECT status, error FROM runs WHERE id = ?1",
            rusqlite::params![run_id],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .expect("read run row")
}
