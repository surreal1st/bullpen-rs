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
//!
//! S13a-01: `gloo_net::eventsource` and `gloo_timers` are browser-only
//! (`Cargo.toml` now gates both to wasm32), so `run()` below is two
//! implementations behind one `#[cfg]`: the wasm32 half is this module's
//! original `EventSource` loop, untouched; the native half drives the same
//! `/api/events` stream through `crate::transport::Request`'s byte-chunk
//! reader instead (`transport::native::NativeBody`, the same one
//! `api.rs::send_message` reads), splitting on `\n` and feeding each
//! `data: ...` line to the same `parse_change` both platforms share. 🔴
//! Unverified at runtime - `subscribe_events` still has no caller (see
//! above), so the native half has only been proven by `cargo check`, not by
//! a real desktop build.
//!
//! S13a-01b: `spawn_stream` (below) used to pick between
//! `wasm_bindgen_futures::spawn_local(run())` and `dioxus::prelude::spawn(run())`
//! by hand behind its own `#[cfg]`. The native half of that was exactly the
//! open question this doc used to end on - whether `dioxus::prelude::spawn`
//! sees a live scope every time `subscribe_events` does - and the answer
//! turned out to be "sometimes, and even when it does it is the wrong
//! scope" (`transport/mod.rs::spawn_task`'s doc has the full reasoning).
//! `spawn_stream` now just calls `crate::transport::spawn_task(run())`,
//! which resolves that question for every platform at once by not depending
//! on scope at all.

#![allow(dead_code)]

use serde::Deserialize;
use std::cell::{Cell, RefCell};
use std::collections::HashMap;
use std::rc::Rc;

#[cfg(target_arch = "wasm32")]
use futures::StreamExt;
#[cfg(target_arch = "wasm32")]
use gloo_net::eventsource::futures::EventSource;

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
        spawn_stream();
    }
    EventsHandle { id }
}

/// S13a-01b: portable across both platforms via `spawn_task` - see this
/// module's top doc and `transport/mod.rs::spawn_task`'s own doc for why
/// neither `wasm_bindgen_futures::spawn_local` directly nor
/// `dioxus::prelude::spawn` directly is the right call here on every
/// target.
fn spawn_stream() {
    crate::transport::spawn_task(run());
}

#[cfg(target_arch = "wasm32")]
async fn run() {
    use gloo_timers::future::TimeoutFuture;

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

/// Native's twin of the wasm `run()` above - see this module's top doc for
/// why it exists and its one open question. Same buffering `api.rs::feed`
/// uses for the run-message SSE stream, applied to `/api/events`'s change
/// frames instead.
#[cfg(not(target_arch = "wasm32"))]
async fn run() {
    loop {
        if !RUNNING.with(Cell::get) {
            return;
        }
        if let Ok(resp) = crate::transport::Request::get("/api/events").send().await {
            let mut stream = resp.into_body_stream();
            let mut buffer: Vec<u8> = Vec::new();
            loop {
                if !RUNNING.with(Cell::get) {
                    return;
                }
                let Ok(Some(chunk)) = stream.next_chunk().await else {
                    break;
                };
                buffer.extend_from_slice(&chunk);
                while let Some(pos) = buffer.iter().position(|&b| b == b'\n') {
                    let mut line: Vec<u8> = buffer.drain(..=pos).collect();
                    line.pop(); // the '\n'
                    if line.last() == Some(&b'\r') {
                        line.pop();
                    }
                    let line = String::from_utf8_lossy(&line);
                    let Some(payload) = line.strip_prefix("data:") else {
                        continue;
                    };
                    let payload = payload.trim();
                    if payload.is_empty() {
                        continue;
                    }
                    if let Some(kind) = parse_change(payload) {
                        notify(kind);
                    }
                }
            }
        }
        if !RUNNING.with(Cell::get) {
            return;
        }
        // Same reconnect posture as the wasm build's `TimeoutFuture::new`
        // above - a dead stream is retried, not given up on.
        tokio::time::sleep(std::time::Duration::from_millis(3_000)).await;
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
