//! Port of `projects/bullpen-night/src/client/events.ts`: one shared
//! `/api/events` stream multiplexed over every subscriber in the tab,
//! opened lazily on the first subscriber and closed once the last one
//! leaves.
//!
//! 🔴 Not called anywhere yet. S1-07b's room list and working bar are the
//! first real subscribers - its own ticket says "Wire the 'roster' and
//! 'working' kinds from `events.rs`" as ITS target, not this one's. This
//! module exists, compiles and is unit-tested (`parse_change`, below) so
//! that ticket can consume it without writing its own port; `#[allow(dead_code)]`
//! is why clippy stays quiet about a public API with no caller inside this
//! crate yet.
//!
//! 🔴 Deviation: the original's heartbeat watchdog closes and reopens the
//! stream after two missed 30s server pings (75s of silence), because a
//! dead proxy can leave `EventSource` looking "open" while producing
//! nothing - see the long comment on `fallback` in `events.ts`. This port
//! reconnects only on an actual `error` event (`gloo_net`'s
//! `EventSourceError::ConnectionError`), not on silence. Simpler, and
//! covers the ordinary case (server restart, network drop), but not the
//! specific "technically open, silently stuck" case the original was
//! written for. Worth adding if S1-07b's wiring hits that symptom.

#![allow(dead_code)]

use futures::StreamExt;
use gloo_net::eventsource::futures::EventSource;
use gloo_timers::future::TimeoutFuture;
use serde::Deserialize;
use std::cell::{Cell, RefCell};
use std::collections::HashMap;
use std::rc::Rc;

/// What changed on the server.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ChangeKind {
    Roster,
    Approvals,
    Questions,
    Working,
    // S3-05: server's `ChangeKind::Memory` (`crates/server/src/changes.rs`),
    // wire string "memory" - core/log/project/shared-log writes all touch
    // this so `memory_editor.rs`'s modal can refetch instead of polling.
    Memory,
}

impl ChangeKind {
    fn parse(kind: &str) -> Option<Self> {
        match kind {
            "roster" => Some(Self::Roster),
            "approvals" => Some(Self::Approvals),
            "questions" => Some(Self::Questions),
            "working" => Some(Self::Working),
            "memory" => Some(Self::Memory),
            _ => None,
        }
    }
}

#[derive(Deserialize)]
struct Frame {
    #[serde(rename = "type")]
    frame_type: Option<String>,
    kind: Option<String>,
}

/// Any frame at all proves the stream is alive; only a `{"type":"change",
/// "kind":...}` frame is acted on - ported from `events.ts:46-47`.
fn parse_change(raw: &str) -> Option<ChangeKind> {
    let frame: Frame = serde_json::from_str(raw).ok()?;
    if frame.frame_type.as_deref() != Some("change") {
        return None;
    }
    ChangeKind::parse(frame.kind.as_deref()?)
}

type Listener = Rc<dyn Fn(ChangeKind)>;

thread_local! {
    static LISTENERS: RefCell<HashMap<u64, Listener>> = RefCell::new(HashMap::new());
    static NEXT_ID: Cell<u64> = const { Cell::new(0) };
    static RUNNING: Cell<bool> = const { Cell::new(false) };
}

fn notify(kind: ChangeKind) {
    let listeners: Vec<_> = LISTENERS.with(|l| l.borrow().values().cloned().collect());
    for listener in listeners {
        listener(kind);
    }
}

/// A live subscription. Dropping it unsubscribes; the shared stream closes
/// once the last subscriber drops.
pub struct EventsHandle {
    id: u64,
}

impl Drop for EventsHandle {
    fn drop(&mut self) {
        LISTENERS.with(|l| {
            l.borrow_mut().remove(&self.id);
        });
        if LISTENERS.with(|l| l.borrow().is_empty()) {
            RUNNING.with(|r| r.set(false));
        }
    }
}

/// Subscribes to server-side changes over the shared `/api/events` stream.
pub fn subscribe_events(on_change: impl Fn(ChangeKind) + 'static) -> EventsHandle {
    let id = NEXT_ID.with(|n| {
        let v = n.get();
        n.set(v + 1);
        v
    });
    LISTENERS.with(|l| l.borrow_mut().insert(id, Rc::new(on_change)));
    if !RUNNING.with(|r| r.replace(true)) {
        wasm_bindgen_futures::spawn_local(run());
    }
    EventsHandle { id }
}

async fn run() {
    loop {
        if !RUNNING.with(Cell::get) {
            return;
        }
        if let Ok(mut source) = EventSource::new("/api/events")
            && let Ok(mut messages) = source.subscribe("message")
        {
            while let Some(frame) = messages.next().await {
                if !RUNNING.with(Cell::get) {
                    return;
                }
                let Ok((_, msg)) = frame else { break };
                if let Some(text) = msg.data().as_string()
                    && let Some(kind) = parse_change(&text)
                {
                    notify(kind);
                }
            }
        }
        if !RUNNING.with(Cell::get) {
            return;
        }
        // A dead stream (connect failure or an `error` event) is retried,
        // not given up on - ported from `events.ts`'s `reconnectTimer`.
        TimeoutFuture::new(3_000).await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_change_reads_a_recognised_kind() {
        assert_eq!(
            parse_change(r#"{"type":"change","kind":"roster"}"#),
            Some(ChangeKind::Roster)
        );
        assert_eq!(
            parse_change(r#"{"type":"change","kind":"working"}"#),
            Some(ChangeKind::Working)
        );
        assert_eq!(
            parse_change(r#"{"type":"change","kind":"memory"}"#),
            Some(ChangeKind::Memory)
        );
    }

    #[test]
    fn parse_change_ignores_anything_else() {
        assert_eq!(parse_change(r#"{"type":"ping"}"#), None);
        assert_eq!(parse_change(r#"{"type":"hello","kind":"roster"}"#), None);
        assert_eq!(parse_change(r#"{"type":"change","kind":"nonsense"}"#), None);
        assert_eq!(parse_change("not json"), None);
    }
}
