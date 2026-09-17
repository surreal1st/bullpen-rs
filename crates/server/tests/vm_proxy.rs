//! Integration tests for the accept-and-route wiring (S6-07):
//! `server::vm_proxy::accept_and_route`, the piece `attach_vm_proxy`
//! (S6-06b) had no caller for - given a freshly accepted, unparsed
//! connection, decide whether it is a bot's screen and, if so, which
//! bot's.
//!
//! Two worlds, named before either bite below was written:
//! - GUARD PRESENT: `get_vm`'s exact-bot-id lookup decides the upstream
//!   `web_port`, and nothing dials anywhere until that lookup succeeds.
//! - GUARD REMOVED (bite a): the resolved upstream stops depending on
//!   which bot's path was actually requested (e.g. the wrong row, or a
//!   hardcoded port, gets used instead).
//! - GUARD REMOVED (bite b): an unresolved bot id still reaches a dial.
//!
//! Every assertion below is on a RECORDING listener's accept count (or, for
//! the routing bite, TWO of them - one per bot), never on the client's own
//! response: a proxy that dials the wrong container, or dials before
//! checking, can still answer the client with something that looks fine.
//! This is the exact shape S6-05's `egress_proxy.rs` used for the same
//! reason (its own header comment says so); `spawn_recording_listener`/
//! `wait_for_at_least_one_accept` below are copied from it, not
//! reinvented.
//!
//! No Docker and no browser on this workstation (S6's own constraint) -
//! every "container" here is a real local `TcpListener` standing in for
//! one, and nothing here claims a real container was reached. That is the
//! meridian smoke test, not this file.
//!
//! S6-F-02 (`.scratch/bullpen-rs/tickets/S6-F-tickets.md`, from S6-R F7)
//! adds a third world to the ones above, for the query half of a viewer
//! path rather than the bot-id half:
//! - GUARD PRESENT: a raw `\r`/`\n` anywhere in the request target is
//!   refused before any upstream connection is even attempted, and a
//!   percent-encoded `%0a`/`%0d` is forwarded byte for byte, inert, because
//!   nothing downstream of this module ever decodes it.
//! - GUARD REMOVED: a raw line break survives into
//!   `build_upgrade_request`'s concatenated request line and splits it into
//!   two, injecting an attacker-chosen header into the container's
//!   handshake.
//!
//! Proven the same way as the rest of this file: from the literal bytes a
//! RECORDING upstream listener receives, never from `accept_and_route`'s
//! returned outcome (an injected header still leaves the client-facing
//! response looking fine).

use server::vm_proxy::{ProxyOutcome, RouteOutcome, accept_and_route, attach_vm_proxy};
use std::net::SocketAddr;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use store::Db;
use store::vms::VmRow;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;

/// Binds a real local listener that does nothing but count accepts, and
/// hands back its address plus a live counter - the "was a socket ever
/// opened" oracle every test in this file relies on. Copied from
/// `egress_proxy.rs`'s helper of the same name (S6-05), not reinvented.
async fn spawn_recording_listener() -> (SocketAddr, Arc<AtomicUsize>) {
    let listener = TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind ephemeral listener");
    let addr = listener.local_addr().expect("local_addr");
    let accepts = Arc::new(AtomicUsize::new(0));
    let accepts_task = Arc::clone(&accepts);
    tokio::spawn(async move {
        while let Ok((socket, _)) = listener.accept().await {
            accepts_task.fetch_add(1, Ordering::SeqCst);
            drop(socket);
        }
    });
    (addr, accepts)
}

/// Polls `accepts` for up to `timeout` for it to become nonzero. Used only
/// where a bug would legitimately race the assertion (`attach_vm_proxy`
/// connects to the upstream from inside the same call `accept_and_route`
/// is still awaiting) - never to paper over a genuinely flaky proxy.
async fn wait_for_at_least_one_accept(accepts: &AtomicUsize, timeout: Duration) -> usize {
    let deadline = tokio::time::Instant::now() + timeout;
    loop {
        let n = accepts.load(Ordering::SeqCst);
        if n > 0 || tokio::time::Instant::now() >= deadline {
            return n;
        }
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
}

fn insert_vm_row(db: &Db, row: &VmRow) {
    db.conn()
        .execute(
            "INSERT INTO vms (bot_id, container, cdp_port, web_port, state, last_used_at)
             VALUES (?, ?, ?, ?, ?, ?)",
            rusqlite::params![
                &row.bot_id,
                &row.container,
                &row.cdp_port,
                &row.web_port,
                &row.state,
                &row.last_used_at,
            ],
        )
        .expect("insert vm row");
}

fn vm_row(bot_id: &str, web_port: i32) -> VmRow {
    VmRow {
        bot_id: bot_id.to_string(),
        container: format!("bullpen-vm-{bot_id}"),
        cdp_port: 9399,
        web_port,
        state: "running".to_string(),
        last_used_at: "2026-09-15T00:00:00Z".to_string(),
    }
}

/// A well-formed WebSocket upgrade request line + headers for `path`, the
/// same shape a real browser opening the viewer sends - `Upgrade` is what
/// `accept_and_route` uses to decide this connection is even a candidate
/// (see `vm_proxy.rs`'s own "accept and route" header doc for why a path
/// match alone is not enough).
fn upgrade_request(path: &str) -> Vec<u8> {
    format!(
        "GET {path} HTTP/1.1\r\n\
         Host: bullpen.example.com\r\n\
         Upgrade: websocket\r\n\
         Connection: Upgrade\r\n\
         Sec-WebSocket-Key: dGhlIHNhbXBsZSBub25jZQ==\r\n\
         \r\n"
    )
    .into_bytes()
}

/// Writes `raw` into one end of a fresh duplex and hands the other end to
/// `accept_and_route`.
async fn route(db: &Db, raw: &[u8], require_auth: bool) -> RouteOutcome<tokio::io::DuplexStream> {
    let (server_side, mut client_side) = tokio::io::duplex(4096);
    client_side.write_all(raw).await.expect("write request");
    accept_and_route(server_side, db, require_auth).await
}

/* --------------------------------------------------------------- bite (a): routing --------------------------------------------------------------- */

/// **The bite (a) target.** A request naming bot A must reach bot A's own
/// container and NEVER bot B's - proven by which of the two RECORDING
/// listeners actually got an accept, not by the response `accept_and_route`
/// hands back (a proxy that dials the wrong container can still return a
/// response that looks like success).
#[tokio::test]
async fn accept_and_route_reaches_only_the_named_bots_container_never_a_neighbours() {
    let db = Db::open(":memory:").expect("open :memory: db");

    let (addr_a, accepts_a) = spawn_recording_listener().await;
    let (addr_b, accepts_b) = spawn_recording_listener().await;
    insert_vm_row(&db, &vm_row("bot-a", addr_a.port() as i32));
    insert_vm_row(&db, &vm_row("bot-b", addr_b.port() as i32));

    let outcome = route(
        &db,
        &upgrade_request("/api/bots/bot-a/vm/view/websockets"),
        false,
    )
    .await;

    assert!(
        matches!(outcome, RouteOutcome::Viewer(ProxyOutcome::Proxying)),
        "expected the request for bot-a to proxy"
    );

    let a_count = wait_for_at_least_one_accept(&accepts_a, Duration::from_secs(2)).await;
    assert_eq!(
        a_count, 1,
        "bot-a's own container must receive exactly one accept for a request naming bot-a"
    );
    assert_eq!(
        accepts_b.load(Ordering::SeqCst),
        0,
        "bot-b's container must never see a connection meant for bot-a"
    );
}

/// The same boundary from the other bot's side, so the test above is not
/// silently passing just because bot-a happened to be resolved first.
#[tokio::test]
async fn accept_and_route_reaches_only_the_named_bots_container_reversed() {
    let db = Db::open(":memory:").expect("open :memory: db");

    let (addr_a, accepts_a) = spawn_recording_listener().await;
    let (addr_b, accepts_b) = spawn_recording_listener().await;
    insert_vm_row(&db, &vm_row("bot-a", addr_a.port() as i32));
    insert_vm_row(&db, &vm_row("bot-b", addr_b.port() as i32));

    let outcome = route(
        &db,
        &upgrade_request("/api/bots/bot-b/vm/view/websockets"),
        false,
    )
    .await;

    assert!(
        matches!(outcome, RouteOutcome::Viewer(ProxyOutcome::Proxying)),
        "expected the request for bot-b to proxy"
    );

    let b_count = wait_for_at_least_one_accept(&accepts_b, Duration::from_secs(2)).await;
    assert_eq!(
        b_count, 1,
        "bot-b's own container must receive exactly one accept for a request naming bot-b"
    );
    assert_eq!(
        accepts_a.load(Ordering::SeqCst),
        0,
        "bot-a's container must never see a connection meant for bot-b"
    );
}

/* --------------------------------------------------------------- bite (b): unknown bot --------------------------------------------------------------- */

/// **The bite (b) target.** An unknown bot id must be refused before any
/// upstream dial at all - proven the same way S6-05 proved a refused
/// CONNECT never opens a socket: a bystander recording listener's accept
/// count stays zero, not the response text.
#[tokio::test]
async fn accept_and_route_unknown_bot_id_never_dials_any_upstream() {
    let db = Db::open(":memory:").expect("open :memory: db");

    // No VM row for "ghost-bot" - but a row for an UNRELATED bot exists,
    // pointing at this listener, so a "fall back to any other row" bug (not
    // just a "dial some hardcoded address" bug) has something real to find.
    let (addr, accepts) = spawn_recording_listener().await;
    insert_vm_row(&db, &vm_row("decoy-bot", addr.port() as i32));

    let outcome = route(
        &db,
        &upgrade_request("/api/bots/ghost-bot/vm/view/websockets"),
        false,
    )
    .await;

    assert!(
        matches!(outcome, RouteOutcome::Viewer(ProxyOutcome::UnknownBot)),
        "expected an unknown bot id to be refused, got a route"
    );

    // A bounded wait, not an instant check: a bug that dials from a
    // detached task (the same shape `attach_vm_proxy`'s own relay uses for
    // the SUCCESS path) would otherwise race this assertion.
    tokio::time::sleep(Duration::from_millis(100)).await;
    assert_eq!(
        accepts.load(Ordering::SeqCst),
        0,
        "an unknown bot id must never cause any upstream dial"
    );
}

/* --------------------------------------------------------------- non-viewer / non-upgrade traffic --------------------------------------------------------------- */

/// The other half of "accept and route": ordinary traffic must come back
/// untouched, byte for byte, so whatever normally serves it (`serve`'s
/// hyper fallback) sees exactly what a real client sent.
#[tokio::test]
async fn accept_and_route_hands_back_non_upgrade_traffic_unchanged() {
    let db = Db::open(":memory:").expect("open :memory: db");
    let raw = b"GET /api/conversations HTTP/1.1\r\nHost: bullpen.example.com\r\n\r\n";

    let outcome = route(&db, raw, false).await;

    match outcome {
        RouteOutcome::PassThrough(mut prefixed) => {
            let mut replayed = vec![0u8; raw.len()];
            tokio::time::timeout(Duration::from_secs(2), prefixed.read_exact(&mut replayed))
                .await
                .expect("timed out reading replayed bytes")
                .expect("read replayed bytes");
            assert_eq!(replayed.as_slice(), raw.as_slice());
        }
        other => panic!("expected PassThrough for a non-upgrade request, got a route: {other:?}"),
    }
}

/// A GET to a viewer path WITHOUT an `Upgrade` header (loading the
/// desktop's own HTML shell, not opening its socket) must also pass
/// through - `viewer_target` matching a path is not, on its own, proof
/// that this connection is the one `attach_vm_proxy`'s raw-duplex relay is
/// for. See `vm_proxy.rs`'s "accept and route" header doc.
#[tokio::test]
async fn accept_and_route_leaves_a_plain_get_to_a_viewer_path_alone() {
    let db = Db::open(":memory:").expect("open :memory: db");
    let raw = b"GET /api/bots/bot-a/vm/view/ HTTP/1.1\r\nHost: bullpen.example.com\r\n\r\n";

    let outcome = route(&db, raw, false).await;

    assert!(
        matches!(outcome, RouteOutcome::PassThrough(_)),
        "a non-upgrade request to a viewer path must still pass through"
    );
}

/// S6-R **F1**: an accept error must not end the serve loop.
///
/// The first version of `serve` wrote `listener.accept().await?`, and
/// `main.rs` wraps the call in `.expect("serve")` - so one `ECONNABORTED`
/// from a client that vanished between the SYN and the accept killed the
/// whole server, and the systemd unit's `Restart=on-failure` with no
/// `StartLimit` override would park it in `failed` after five of those in
/// ten seconds.
///
/// Two worlds, and the observable that separates them: with the guard, a
/// connection made AFTER an accept error is still served; without it,
/// `serve` has already returned and nothing is listening. Forcing a real
/// `ECONNABORTED` out of the OS is not portable, which is why `serve` takes
/// an `Accepter` - this fake yields two errors of each class first, then
/// delegates to a real listener.
struct FlakyAccepter {
    errors: Vec<std::io::ErrorKind>,
    inner: tokio::net::TcpListener,
}

impl server::vm_proxy::Accepter for FlakyAccepter {
    async fn accept(&mut self) -> std::io::Result<(tokio::net::TcpStream, std::net::SocketAddr)> {
        if !self.errors.is_empty() {
            let kind = self.errors.remove(0);
            return Err(std::io::Error::new(kind, "injected by FlakyAccepter"));
        }
        self.inner.accept().await
    }
}

#[tokio::test]
async fn an_accept_error_does_not_end_the_serve_loop() {
    use tokio::io::AsyncWriteExt;

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind ephemeral port");
    let addr = listener.local_addr().expect("local addr");

    let flaky = FlakyAccepter {
        // One the loop retries immediately, one it logs and sleeps on.
        errors: vec![
            std::io::ErrorKind::ConnectionAborted,
            std::io::ErrorKind::PermissionDenied,
        ],
        inner: listener,
    };

    let app = axum::Router::new().route("/", axum::routing::get(|| async { "ok" }));
    let db = std::sync::Arc::new(std::sync::Mutex::new(
        store::Db::open(":memory:").expect("open :memory:"),
    ));

    let server =
        tokio::spawn(
            async move { server::vm_proxy::serve_with_accepter(flaky, app, db, false).await },
        );

    // The loop sleeps 1s on the non-connection error, so give it room.
    let mut stream = None;
    for _ in 0..40 {
        if let Ok(s) = tokio::net::TcpStream::connect(addr).await {
            stream = Some(s);
            break;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    let mut stream = stream.expect(
        "after two injected accept errors the loop must still be accepting - \
         with `accept().await?` it has already returned and nothing is listening",
    );

    stream
        .write_all(b"GET / HTTP/1.1\r\nHost: x\r\nConnection: close\r\n\r\n")
        .await
        .expect("write request");
    let mut response = Vec::new();
    tokio::time::timeout(
        Duration::from_secs(5),
        tokio::io::AsyncReadExt::read_to_end(&mut stream, &mut response),
    )
    .await
    .expect("timed out reading the response")
    .expect("read response");

    let text = String::from_utf8_lossy(&response);
    assert!(
        text.starts_with("HTTP/1.1 200"),
        "the connection after the injected accept errors must be served normally, got: {text:?}"
    );

    server.abort();
}

/* --------------------------------------------------------------- S6-F-02: header injection via the query half --------------------------------------------------------------- */

/// Binds a real local listener that accepts ONE connection and records
/// every byte written to it. `spawn_recording_listener` above only counts
/// accepts, which proves ROUTING but not FRAMING: an injected header still
/// leaves an accept count of exactly one. This is the oracle that reads the
/// literal bytes the container's own handshake would receive.
async fn spawn_capturing_listener() -> (SocketAddr, Arc<Mutex<Vec<u8>>>) {
    let listener = TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind ephemeral listener");
    let addr = listener.local_addr().expect("local_addr");
    let captured = Arc::new(Mutex::new(Vec::new()));
    let captured_task = Arc::clone(&captured);
    tokio::spawn(async move {
        if let Ok((mut socket, _)) = listener.accept().await {
            let mut buf = [0u8; 4096];
            // `build_upgrade_request`'s handshake is written in one
            // `write_all` call, so one read is enough to capture it whole.
            if let Ok(Ok(n)) =
                tokio::time::timeout(Duration::from_secs(2), socket.read(&mut buf)).await
            {
                captured_task.lock().unwrap().extend_from_slice(&buf[..n]);
            }
        }
    });
    (addr, captured)
}

/// **The S6-F-02 bite.** S6-R F7's literal example: a bare LF placed after
/// `?` survives `parse_request_head`'s naive line splitting (it splits on
/// the two-byte sequence `"\r\n"`, so a lone `\n` rides straight through),
/// clears `viewer_target`'s bot-id guard (the LF lands in the query, not
/// the path `viewer_target` matches against), and - without this ticket's
/// guard - would land verbatim in `build_upgrade_request`'s concatenated
/// request line, splitting it into two and injecting `X-Injected: y` as a
/// real header on the container's request. No space inside the injected
/// text: `parse_request_head` tokenises the request line on spaces, so a
/// space there would just truncate the parsed target short of the bug
/// rather than exercise it.
///
/// Proven from the upstream's own bytes, not the client-facing outcome: a
/// status code proves nothing here (an injected header still returns
/// normally to a client that never sees the container's side).
#[tokio::test]
async fn accept_and_route_refuses_a_bare_lf_in_the_query_before_touching_any_upstream() {
    let db = Db::open(":memory:").expect("open :memory: db");

    let (addr, captured) = spawn_capturing_listener().await;
    insert_vm_row(&db, &vm_row("bot-a", addr.port() as i32));

    let raw: &[u8] = b"GET /api/bots/bot-a/vm/view/?a=1\nX-Injected:y HTTP/1.1\r\n\
                        Host: bullpen.example.com\r\n\
                        Upgrade: websocket\r\n\
                        Connection: Upgrade\r\n\
                        Sec-WebSocket-Key: dGhlIHNhbXBsZSBub25jZQ==\r\n\
                        \r\n";

    let outcome = route(&db, raw, false).await;

    assert!(
        matches!(outcome, RouteOutcome::Unreadable),
        "a target carrying a raw LF must never parse into a routable request, got {outcome:?}"
    );

    // Bounded wait, not instant: a bug that still dials would otherwise
    // race this assertion, the same shape `accept_and_route_unknown_bot_id_\
    // never_dials_any_upstream` above uses.
    tokio::time::sleep(Duration::from_millis(150)).await;
    assert!(
        captured.lock().unwrap().is_empty(),
        "the container's socket must never receive a single byte for a target carrying a raw LF"
    );
}

/// The other half of the same bite: an INERT encoding of the same attack
/// (`%0a` - three ordinary printable ASCII bytes, `%`, `0`, `a`) must not
/// be refused, because nothing downstream of this module ever decodes a
/// viewer path's query - it is forwarded exactly as it arrived. Proven by
/// reading the literal handshake bytes the recording upstream received:
/// `%0a` must survive in the request line, and `X-Injected` must never
/// appear as a header line of its own.
#[tokio::test]
async fn accept_and_route_forwards_a_percent_encoded_0a_in_the_query_as_inert_literal_text() {
    let db = Db::open(":memory:").expect("open :memory: db");

    let (addr, captured) = spawn_capturing_listener().await;
    insert_vm_row(&db, &vm_row("bot-a", addr.port() as i32));

    let outcome = route(
        &db,
        &upgrade_request("/api/bots/bot-a/vm/view/websockets?a=1%0aX-Injected:y"),
        false,
    )
    .await;

    assert!(
        matches!(outcome, RouteOutcome::Viewer(ProxyOutcome::Proxying)),
        "a %0a is inert ASCII text, not a control character - it must proxy normally, got {outcome:?}"
    );

    let deadline = tokio::time::Instant::now() + Duration::from_secs(2);
    let bytes = loop {
        let snapshot = captured.lock().unwrap().clone();
        if !snapshot.is_empty() || tokio::time::Instant::now() >= deadline {
            break snapshot;
        }
        tokio::time::sleep(Duration::from_millis(5)).await;
    };
    assert!(
        !bytes.is_empty(),
        "expected the recording upstream to receive the handshake"
    );

    let text = String::from_utf8_lossy(&bytes);
    let head_end = text
        .find("\r\n\r\n")
        .expect("a full handshake head must have arrived");
    let head_lines: Vec<&str> = text[..head_end].split("\r\n").collect();

    let request_line = head_lines[0];
    assert!(
        request_line.to_ascii_lowercase().contains("%0a"),
        "the percent-encoded form must survive untouched in the handshake's request line, got: {request_line:?}"
    );

    let injected_as_its_own_header = head_lines[1..]
        .iter()
        .any(|line| line.to_ascii_lowercase().starts_with("x-injected"));
    assert!(
        !injected_as_its_own_header,
        "an inert %0a must never become a real header line in the handshake, got: {head_lines:?}"
    );
}

/// `attach_vm_proxy` (S6-06b) is the second place a raw target can enter
/// this module - a caller that already has a socket and a `raw_target`
/// string, bypassing `parse_request_head` entirely. The same guard must
/// hold there too, refused before `resolve_viewer_target`/`get_vm` ever
/// runs (a `CR` this time, not the `LF` used above, so both control bytes
/// - not just the one in S6-R's literal example - are proven refused).
#[tokio::test]
async fn attach_vm_proxy_refuses_a_raw_target_with_an_embedded_cr_before_resolving_anything() {
    let db = Db::open(":memory:").expect("open :memory: db");
    let (server_side, mut client_side) = tokio::io::duplex(4096);
    let headers = axum::http::HeaderMap::new();

    let outcome = attach_vm_proxy(
        server_side,
        "/api/bots/bot-a/vm/view/websockets?a=1\rX-Injected:y",
        &headers,
        &db,
        false,
    )
    .await;

    assert!(
        matches!(outcome, ProxyOutcome::MalformedTarget),
        "a raw CR in the target must be refused before any resolution, got {outcome:?}"
    );

    let mut response = Vec::new();
    tokio::time::timeout(
        Duration::from_secs(2),
        client_side.read_to_end(&mut response),
    )
    .await
    .expect("timed out reading refusal")
    .expect("read refusal");
    let text = String::from_utf8_lossy(&response);
    assert!(
        text.starts_with("HTTP/1.1 400"),
        "expected a 400 refusal, got: {text:?}"
    );
}

/* --------------------------------------------------------------- S8b-05 (F9): require_auth actually gates --------------------------------------------------------------- */

/// **F9 bite, guard-present.** `main.rs:153` passes `require_auth = true`
/// for the live proxy, but until this test every call in this file passed
/// `false` - the one value that ever reaches production was the one value
/// never exercised (`DEFERRED.md:93`). Proven the same way bite (b) above
/// proves an unknown bot never dials upstream: a bystander recording
/// listener's accept count stays zero, never the response text, so a bug
/// that dials before checking auth (but still answers the client with
/// something that looks like a refusal) has something real to catch it.
#[tokio::test]
async fn accept_and_route_with_require_auth_true_refuses_an_unauthenticated_request_before_resolving_anything()
 {
    let db = Db::open(":memory:").expect("open :memory: db");
    let (addr, accepts) = spawn_recording_listener().await;
    insert_vm_row(&db, &vm_row("bot-a", addr.port() as i32));

    let outcome = route(
        &db,
        &upgrade_request("/api/bots/bot-a/vm/view/websockets"),
        true,
    )
    .await;

    assert!(
        matches!(outcome, RouteOutcome::Viewer(ProxyOutcome::Unauthorized)),
        "expected an unauthenticated request to be refused when require_auth is true, got {outcome:?}"
    );

    // A bounded wait, not an instant check - same reasoning as bite (b)'s
    // own comment: a bug that dials from a detached task would otherwise
    // race this assertion.
    tokio::time::sleep(Duration::from_millis(100)).await;
    assert_eq!(
        accepts.load(Ordering::SeqCst),
        0,
        "an unauthenticated request must never cause any upstream dial when require_auth is true"
    );
}

/// **F9, the other half.** The fix is "refuse an absent/invalid session",
/// not "refuse everything under require_auth" - a request carrying a real
/// session token (`store::auth::create_session`, the same row
/// `session_valid` reads) must still reach the bot's own container.
#[tokio::test]
async fn accept_and_route_with_require_auth_true_and_a_valid_session_still_proxies() {
    let db = Db::open(":memory:").expect("open :memory: db");
    let token = store::auth::create_session(&db).expect("create session");
    let (addr, accepts) = spawn_recording_listener().await;
    insert_vm_row(&db, &vm_row("bot-a", addr.port() as i32));

    let raw = format!(
        "GET /api/bots/bot-a/vm/view/websockets HTTP/1.1\r\n\
         Host: bullpen.example.com\r\n\
         Authorization: Bearer {token}\r\n\
         Upgrade: websocket\r\n\
         Connection: Upgrade\r\n\
         Sec-WebSocket-Key: dGhlIHNhbXBsZSBub25jZQ==\r\n\
         \r\n"
    )
    .into_bytes();

    let outcome = route(&db, &raw, true).await;

    assert!(
        matches!(outcome, RouteOutcome::Viewer(ProxyOutcome::Proxying)),
        "expected a request with a valid session to proxy when require_auth is true, got {outcome:?}"
    );

    let count = wait_for_at_least_one_accept(&accepts, Duration::from_secs(2)).await;
    assert_eq!(
        count, 1,
        "a valid session under require_auth = true must still reach the bot's own container"
    );
}
