//! `GET /api/events`: the app-wide change feed. Port of
//! `src/server/app.ts:1114-1152`. A `hello` frame, then a `{"type":"change",
//! "kind":…}` frame per `ChangeKind` debounced 250 ms (so a run's several
//! touches a second do not each cost a write), plus a `{"type":"ping"}`
//! every 30 s to keep the connection alive through anything that would
//! otherwise time out an idle SSE stream.

use std::collections::HashMap;
use std::convert::Infallible;
use std::time::Duration;

use axum::Router;
use axum::extract::State;
use axum::response::sse::{Event, Sse};
use axum::routing::get;
use futures::Stream;
use serde_json::json;
use tokio::time::Instant;

use crate::AppState;
use crate::changes::ChangeKind;

const DEBOUNCE: Duration = Duration::from_millis(250);
const PING_EVERY: Duration = Duration::from_secs(30);

pub fn router() -> Router<AppState> {
    Router::new().route("/api/events", get(events))
}

async fn events(
    State(state): State<AppState>,
) -> Sse<impl Stream<Item = Result<Event, Infallible>>> {
    let (tx, mut change_rx) = tokio::sync::mpsc::unbounded_channel::<ChangeKind>();
    // B4: a guarded subscription rather than the permanent `subscribe` -
    // every reload / reconnect (the client retries every 3s) hits this
    // handler again, so an unguarded listener here would add one closure
    // per connection for the life of the process. `_subscription` is held
    // by the stream below and unregisters on `Drop`, when the connection
    // ends.
    let subscription = state.runs.changes.subscribe_scoped(move |kind| {
        // A write can lose the race against the client disconnecting - a
        // closed receiver just means nobody catches this touch, never a
        // panic.
        let _ = tx.send(kind);
    });

    let stream = async_stream::stream! {
        let _subscription = subscription;
        yield Ok(Event::default().data(json!({"type": "hello"}).to_string()));

        let mut pending: HashMap<ChangeKind, Instant> = HashMap::new();
        let mut ping = tokio::time::interval(PING_EVERY);
        // `tokio::time::interval` fires its first tick immediately; a JS
        // `setInterval` never does. Consuming that first tick here lines
        // the two up - the first ping lands 30s in, not at t=0.
        ping.tick().await;

        loop {
            let deadline = pending.values().min().copied();
            tokio::select! {
                received = change_rx.recv() => {
                    match received {
                        Some(kind) => {
                            pending.entry(kind).or_insert_with(|| Instant::now() + DEBOUNCE);
                        }
                        // The manager itself dropped - nothing left to report.
                        None => break,
                    }
                }
                _ = ping.tick() => {
                    yield Ok(Event::default().data(json!({"type": "ping"}).to_string()));
                }
                _ = sleep_until_or_forever(deadline), if deadline.is_some() => {
                    let now = Instant::now();
                    let ready: Vec<ChangeKind> = pending
                        .iter()
                        .filter(|&(_, &at)| at <= now)
                        .map(|(&kind, _)| kind)
                        .collect();
                    for kind in ready {
                        pending.remove(&kind);
                        yield Ok(Event::default().data(json!({"type": "change", "kind": kind.as_str()}).to_string()));
                    }
                }
            }
        }
    };

    Sse::new(stream)
}

/// `select!`'s arm expression is evaluated fresh every loop iteration, so
/// this recomputes the sleep against the CURRENT earliest deadline each
/// time round - never a stale one left over from a prior iteration.
async fn sleep_until_or_forever(deadline: Option<Instant>) {
    match deadline {
        Some(at) => tokio::time::sleep_until(at).await,
        None => std::future::pending::<()>().await,
    }
}
