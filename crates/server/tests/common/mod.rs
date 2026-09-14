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
#![allow(dead_code)]

use std::collections::VecDeque;
use std::sync::{Arc, Mutex};

use model::{EventStream, ModelEvent, ModelPort, ModelRequest, ToolCall};

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
                    calls: vec![ToolCall { id: "c1".to_string(), name: "list_tasks".to_string(), arguments: "{}".to_string() }],
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
