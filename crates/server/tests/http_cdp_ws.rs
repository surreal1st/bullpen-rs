//! S6-W-05: proves `HttpCdp`'s WebSocket half (`create_window`/`call`)
//! against a LOCAL WebSocket listener - never Chromium, never a real
//! DevTools endpoint. Nothing here proves a real page loads; it proves the
//! CLIENT's behaviour: a DevTools-shaped error surfaces to the caller
//! instead of being swallowed or hung on (bite a), and a socket that
//! accepts and then goes silent cannot wedge a run forever because
//! `CALL_TIMEOUT` bounds the wait (bite b). The meridian smoke test
//! (S6-SMOKE) is the only thing that can prove a real browser answers.
//!
//! Every listener here binds `127.0.0.1:0` and reads the port back from
//! `TcpListener::local_addr()` - never a hardcoded port, several of which
//! (4380/4390/4370/4371/4360) are live on this machine already.

use server::desk::{Cdp, DeskConfig, HttpCdp};
use std::time::Duration;
use tokio::net::TcpListener;
use tokio_tungstenite::tungstenite::Message;

fn config_for(port: u16) -> DeskConfig {
    DeskConfig {
        cdp: format!("http://127.0.0.1:{port}"),
        view: "http://127.0.0.1:0".to_string(),
        container: "test-desk".to_string(),
        docker_host: "unix:///dev/null".to_string(),
    }
}

/// Short enough that a listener which never accepts, or accepts and stays
/// silent, cannot make this suite slow - but long enough that a real local
/// round trip (accept, one message exchange, all on loopback) never trips
/// it by accident.
const TEST_TIMEOUT: Duration = Duration::from_millis(500);

/* --------------------------------------------------------------- bite (a) */

/// A listener that performs the WebSocket handshake, waits for the one
/// request `HttpCdp::call` sends, and replies with a DevTools-shaped ERROR
/// message (`{"id": <same id>, "error": {"message": ...}}`) - the same
/// shape a real Chromium answers a bad `Runtime.evaluate` with (a syntax
/// error in the expression, a detached target, etc).
async fn spawn_error_listener(error_message: &'static str) -> u16 {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();

    tokio::spawn(async move {
        let (stream, _) = listener.accept().await.unwrap();
        let mut ws = tokio_tungstenite::accept_async(stream).await.unwrap();
        // Wait for HttpCdp's request so the reply's `id` genuinely echoes
        // what was asked, matching real DevTools rather than guessing 1.
        let request = loop {
            match futures::StreamExt::next(&mut ws).await {
                Some(Ok(Message::Text(text))) => break text,
                Some(Ok(_)) => continue,
                _ => return,
            }
        };
        let id = serde_json::from_str::<serde_json::Value>(&request)
            .ok()
            .and_then(|v| v.get("id").and_then(|i| i.as_u64()).map(|i| i as i64))
            .unwrap_or(1);
        let reply = serde_json::json!({
            "id": id,
            "error": { "message": error_message }
        })
        .to_string();
        let _ = futures::SinkExt::send(&mut ws, Message::text(reply)).await;
    });

    port
}

/// 🔴 BITE (a): the failure must surface to the CALLER as an `Err` carrying
/// the real DevTools error text - not be swallowed into an `Ok`, and not
/// hang. Proven wrong on purpose in this ticket's Results by commenting out
/// the `Some(e) => Err(...)` arm in `desk.rs`'s `once()` (making every
/// reply succeed regardless of an `error` field) and re-running this test:
/// it goes red because the assertion below expects `Err`, not `Ok`.
#[tokio::test]
async fn call_surfaces_a_devtools_error_instead_of_hanging_or_swallowing_it() {
    let port = spawn_error_listener("Runtime.evaluate: Uncaught SyntaxError").await;
    let cdp = HttpCdp::with_timeouts(config_for(port), TEST_TIMEOUT, TEST_TIMEOUT);

    let result = tokio::time::timeout(
        Duration::from_secs(2),
        cdp.call("target-1", "Runtime.evaluate", serde_json::json!({})),
    )
    .await
    .expect("call must return well within the test's own outer timeout, not hang");

    let err = result.expect_err("a DevTools error reply must come back as Err, not Ok");
    assert_eq!(
        err, "Runtime.evaluate: Uncaught SyntaxError",
        "the caller must see the real DevTools error message, not a generic one"
    );
}

/// Same shape, through `create_window`'s `Target.createTarget` round trip
/// instead of `call`'s per-target one - `create_window` ALSO fetches
/// `/json/version` first, so this listener answers HTTP too, over the
/// SAME `127.0.0.1:0` port (`HttpCdp` uses the identical `config.cdp` base
/// for both the HTTP and WebSocket sides, exactly like real Chromium's
/// DevTools port does).
#[tokio::test]
async fn create_window_surfaces_a_devtools_error_instead_of_hanging_or_swallowing_it() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();

    let server = tokio::spawn(async move {
        loop {
            let (mut stream, _) = listener.accept().await.unwrap();
            // Peek whether this connection is a plain HTTP GET (for
            // /json/version) or a WebSocket upgrade, by trying the HTTP
            // response first on a short-lived probe read.
            use tokio::io::AsyncWriteExt;
            let mut buf = [0u8; 1024];
            let n = stream.peek(&mut buf).await.unwrap_or(0);
            let head = String::from_utf8_lossy(&buf[..n]);
            if head.contains("Upgrade: websocket") || head.contains("upgrade: websocket") {
                let mut ws = tokio_tungstenite::accept_async(stream).await.unwrap();
                let request = loop {
                    match futures::StreamExt::next(&mut ws).await {
                        Some(Ok(Message::Text(text))) => break text,
                        Some(Ok(_)) => continue,
                        _ => return,
                    }
                };
                let id = serde_json::from_str::<serde_json::Value>(&request)
                    .ok()
                    .and_then(|v| v.get("id").and_then(|i| i.as_u64()).map(|i| i as i64))
                    .unwrap_or(1);
                let reply = serde_json::json!({
                    "id": id,
                    "error": { "message": "Target.createTarget: no such window" }
                })
                .to_string();
                let _ = futures::SinkExt::send(&mut ws, Message::text(reply)).await;
                return;
            } else {
                // GET /json/version - answer with a webSocketDebuggerUrl
                // pointing back at THIS SAME listener, exactly as real
                // Chromium's /json/version does (the browser-scoped socket
                // lives on the same port as everything else).
                let body = serde_json::json!({
                    "webSocketDebuggerUrl": format!("ws://127.0.0.1:{port}/devtools/browser/x")
                })
                .to_string();
                let response = format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                    body.len(),
                    body
                );
                let _ = stream.write_all(response.as_bytes()).await;
                let _ = stream.shutdown().await;
            }
        }
    });

    let cdp = HttpCdp::with_timeouts(config_for(port), TEST_TIMEOUT, TEST_TIMEOUT);
    let result = tokio::time::timeout(Duration::from_secs(2), cdp.create_window("about:blank"))
        .await
        .expect("create_window must return well within the test's own outer timeout, not hang");

    let err = result.expect_err("a DevTools error reply must come back as Err, not Ok");
    assert_eq!(err, "Target.createTarget: no such window");

    server.abort();
}

/* --------------------------------------------------------------- bite (b) */

/// Accepts the TCP connection and performs the WebSocket handshake, then
/// never reads and never replies - a wedged DevTools socket, the exact
/// shape this ticket's bite (b) exists to prove cannot hang a run forever.
/// The connection is kept open (not dropped) for the listener task's own
/// lifetime so this is a genuine silence, not a connection reset that
/// `HttpCdp` might be surfacing as a DIFFERENT kind of error by accident.
async fn spawn_silent_listener() -> u16 {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();

    tokio::spawn(async move {
        let (stream, _) = listener.accept().await.unwrap();
        let _ws = tokio_tungstenite::accept_async(stream).await.unwrap();
        // Hold the connection open and say nothing.
        tokio::time::sleep(Duration::from_secs(30)).await;
    });

    port
}

/// 🔴 BITE (b): `CALL_TIMEOUT` (30s in production; overridden here to
/// `TEST_TIMEOUT` via `HttpCdp::with_timeouts` so this test does not
/// itself take 30 real seconds) must turn a silent socket into a timed-out
/// `Err`, not a hang. Proven wrong on purpose in this ticket's Results by
/// mutating `desk.rs`'s `once()` to await the round trip directly instead
/// of wrapping it in `tokio::time::timeout(call_timeout, round_trip)`, and
/// re-running: the same test then hangs past the test's own 2s outer
/// guard, which is the observable "guard removed" red this bite requires.
#[tokio::test]
async fn call_times_out_instead_of_hanging_forever_on_a_silent_socket() {
    let port = spawn_silent_listener().await;
    let cdp = HttpCdp::with_timeouts(config_for(port), TEST_TIMEOUT, TEST_TIMEOUT);

    let started = std::time::Instant::now();
    let result = tokio::time::timeout(
        Duration::from_secs(2),
        cdp.call("target-1", "Runtime.evaluate", serde_json::json!({})),
    )
    .await
    .expect(
        "call must return within the test's own 2s outer guard - if this panics, the 30s \
production CALL_TIMEOUT was NOT what stopped it, which is exactly the wedge this bite exists \
to catch",
    );

    let err = result.expect_err("a silent socket must produce a timed-out Err, not Ok");
    assert!(
        err.contains("timed out"),
        "expected a timeout error, got: {err}"
    );
    assert!(
        started.elapsed() < Duration::from_secs(2),
        "must resolve near TEST_TIMEOUT ({TEST_TIMEOUT:?}), not wait out the full silence"
    );
}

/// Same wedge, through `create_window`'s `Target.createTarget` round trip.
/// `create_window`'s own initial WebSocket connect has no timeout of its
/// own (faithful to TS - see `desk.rs`'s doc on `SOCKET_CONNECT_TIMEOUT`),
/// so this listener must answer `/json/version` over HTTP first (same
/// pattern as the error-surfacing test above) and only go silent on the
/// WebSocket - proving `call_timeout` alone, not a connect timeout, is
/// what bounds the wedge here.
#[tokio::test]
async fn create_window_times_out_instead_of_hanging_forever_on_a_silent_socket() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();

    tokio::spawn(async move {
        loop {
            let (mut stream, _) = listener.accept().await.unwrap();
            use tokio::io::AsyncWriteExt;
            let mut buf = [0u8; 1024];
            let n = stream.peek(&mut buf).await.unwrap_or(0);
            let head = String::from_utf8_lossy(&buf[..n]);
            if head.contains("Upgrade: websocket") || head.contains("upgrade: websocket") {
                let _ws = tokio_tungstenite::accept_async(stream).await.unwrap();
                tokio::time::sleep(Duration::from_secs(30)).await;
                return;
            } else {
                let body = serde_json::json!({
                    "webSocketDebuggerUrl": format!("ws://127.0.0.1:{port}/devtools/browser/x")
                })
                .to_string();
                let response = format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                    body.len(),
                    body
                );
                let _ = stream.write_all(response.as_bytes()).await;
                let _ = stream.shutdown().await;
            }
        }
    });

    let cdp = HttpCdp::with_timeouts(config_for(port), TEST_TIMEOUT, TEST_TIMEOUT);
    let result = tokio::time::timeout(Duration::from_secs(2), cdp.create_window("about:blank"))
        .await
        .expect("create_window must return within the test's own 2s outer guard, not hang");

    let err = result.expect_err("a silent socket must produce a timed-out Err, not Ok");
    assert!(
        err.contains("timed out"),
        "expected a timeout error, got: {err}"
    );
}
