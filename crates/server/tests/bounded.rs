//! S1-F-04 bite checks: `RunManager`'s per-run bookkeeping (`bus`,
//! `backlog`, `stopping`) is bounded by the runs still live plus a short
//! grace window, not by every run the process has ever driven (B3, B5,
//! B13); `ChangeBus::subscribe_scoped` (B4) actually unregisters a dropped
//! listener instead of leaking one closure per connection for the life of
//! the process.
//!
//! Drives `RunManager` directly, same posture as `tests/runs.rs` (which
//! this file deliberately does not touch - two other builders are editing
//! elsewhere in this crate concurrently). `ChangeBus`'s test needs no
//! server plumbing at all.

mod common;

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use common::as_port;
use model::ModelMessage;
use model::ladder::Trigger;
use server::changes::{ChangeBus, ChangeKind};
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

/// Drains a run's events until `Done`/`Error`, discarding them.
async fn drain(mut rx: tokio::sync::mpsc::UnboundedReceiver<RunEvent>) {
    while let Some(event) = rx.recv().await {
        if matches!(event, RunEvent::Done { .. } | RunEvent::Error { .. }) {
            break;
        }
    }
}

/// Same as `drain`, but keeps what it saw - for asserting on the replay a
/// late subscriber gets.
async fn drain_collect(mut rx: tokio::sync::mpsc::UnboundedReceiver<RunEvent>) -> Vec<RunEvent> {
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

fn start_options(bot_id: &str, conversation_id: &str) -> StartOptions {
    StartOptions {
        bot_id: bot_id.to_string(),
        conversation_id: conversation_id.to_string(),
        model: "test/model".to_string(),
        messages: vec![ModelMessage::user("hi")],
        trigger: Trigger::Chat,
        room: false,
    }
}

// 1. Fifty runs settle, none of them subscribed-to again after their own
//    drain - `bus` and `stopping` hold no more entries than the runs still
//    live (here, zero), proving B5/B13's "grows by one entry per run for
//    the life of the process" is actually fixed, not merely slowed.
#[tokio::test]
async fn fifty_settled_runs_leave_no_bus_or_stopping_entries() {
    let db = open_db();
    seed_bot(&db, "arthur", "Arthur");
    let conversation_id = own_conversation(&db, "arthur");

    let manager = Arc::new(RunManager::new(
        Arc::clone(&db),
        as_port(model::fake::text_port("Hello.", "test/model")),
    ));

    for _ in 0..50 {
        let run_id = manager.start(start_options("arthur", &conversation_id));
        drain(manager.subscribe(&run_id)).await;
    }

    let (bus_len, _backlog_len, stopping_len) = manager.bookkeeping_sizes();
    assert_eq!(
        bus_len, 0,
        "settle should drop each run's bus entry immediately"
    );
    assert_eq!(
        stopping_len, 0,
        "settle should drop each run's stopping entry immediately"
    );
}

// 2. B3: a late subscriber inside the grace window still gets the backlog
//    replay; once the grace window passes, the backlog (and any `bus`
//    entry a late `subscribe` recreated) is gone. Uses
//    `RunManager::with_backlog_ttl` to shrink the grace window so this does
//    not have to sleep out the real 60s product value.
#[tokio::test]
async fn backlog_survives_the_grace_window_then_is_dropped() {
    let db = open_db();
    seed_bot(&db, "arthur", "Arthur");
    let conversation_id = own_conversation(&db, "arthur");

    let manager = Arc::new(RunManager::with_backlog_ttl(
        Arc::clone(&db),
        as_port(model::fake::text_port("Hello.", "test/model")),
        Duration::from_millis(150),
    ));

    let run_id = manager.start(start_options("arthur", &conversation_id));
    drain(manager.subscribe(&run_id)).await;

    // Still inside the grace window: a late subscriber gets the replay.
    let replay = drain_collect(manager.subscribe(&run_id)).await;
    assert!(
        replay.iter().any(|e| matches!(e, RunEvent::Done { .. })),
        "expected the backlog replay inside the grace window, got {replay:?}"
    );

    tokio::time::sleep(Duration::from_millis(500)).await;

    let (bus_len, backlog_len, _stopping_len) = manager.bookkeeping_sizes();
    assert_eq!(
        backlog_len, 0,
        "backlog should be dropped once the grace window passes"
    );
    assert_eq!(
        bus_len, 0,
        "a late subscribe's bus entry should not survive the grace window either"
    );
}

// 3. B13: a stop that lands after the run's one and only `take_stop` check
//    already passed - the run finishes "in the same instant" the finding
//    describes - does not leave its id in `stopping` forever. A gated port
//    signals the test the moment the run has entered its (only) model call,
//    i.e. just after that check, so the race is deterministic rather than
//    hoped-for.
struct EnterThenGatePort {
    entered: Mutex<Option<tokio::sync::oneshot::Sender<()>>>,
    gate: Mutex<Option<tokio::sync::oneshot::Receiver<()>>>,
}

impl model::ModelPort for EnterThenGatePort {
    fn stream(&self, _request: model::ModelRequest) -> model::EventStream {
        let entered = self.entered.lock().expect("entered mutex poisoned").take();
        let gate = self.gate.lock().expect("gate mutex poisoned").take();
        Box::pin(async_stream::stream! {
            if let Some(tx) = entered {
                let _ = tx.send(());
            }
            if let Some(rx) = gate {
                let _ = rx.await;
            }
            yield model::ModelEvent::Delta { text: "Hi.".to_string() };
            yield model::ModelEvent::Done {
                model: "test/model".to_string(),
                usage: None,
                finish_reason: None,
            };
        })
    }
}

#[tokio::test]
async fn stopping_a_run_that_finishes_in_the_same_instant_does_not_leak() {
    let db = open_db();
    seed_bot(&db, "arthur", "Arthur");
    let conversation_id = own_conversation(&db, "arthur");

    let (entered_tx, entered_rx) = tokio::sync::oneshot::channel();
    let (gate_tx, gate_rx) = tokio::sync::oneshot::channel();
    let port = EnterThenGatePort {
        entered: Mutex::new(Some(entered_tx)),
        gate: Mutex::new(Some(gate_rx)),
    };

    let manager = Arc::new(RunManager::new(Arc::clone(&db), as_port(port)));
    let run_id = manager.start(start_options("arthur", &conversation_id));

    // Wait until the spawned run task has already taken its one `take_stop`
    // check for this step (false - nobody has stopped it yet) and is now
    // blocked inside the model call. A stop landing from here on will never
    // be seen by another `take_stop` check, only by `settle`.
    entered_rx.await.expect("run entered the model call");
    manager.stop(&run_id);
    let _ = gate_tx.send(());

    drain(manager.subscribe(&run_id)).await;

    let (_bus_len, _backlog_len, stopping_len) = manager.bookkeeping_sizes();
    assert_eq!(
        stopping_len, 0,
        "a stop that lands after the last take_stop check should not linger in `stopping`"
    );
}

// 4. B4: `ChangeBus::subscribe_scoped` returns a guard whose `Drop`
//    unregisters - opening and dropping 100 "connections" (what
//    `GET /api/events` does on every page reload / SSE reconnect) leaves no
//    listeners behind to fire on the next touch.
#[test]
fn dropping_a_hundred_scoped_subscriptions_leaves_no_listeners() {
    let bus = ChangeBus::new();
    let fired = Arc::new(AtomicUsize::new(0));

    for _ in 0..100 {
        let fired = Arc::clone(&fired);
        let _subscription = bus.subscribe_scoped(move |_| {
            fired.fetch_add(1, Ordering::SeqCst);
        });
        // The guard drops here, at the end of the loop body's scope - same
        // as an SSE stream ending when a client reloads.
    }

    bus.touch(ChangeKind::Working);
    assert_eq!(
        fired.load(Ordering::SeqCst),
        0,
        "no dropped subscription should still be listening"
    );
}
