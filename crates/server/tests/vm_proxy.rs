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

use server::vm_proxy::{ProxyOutcome, RouteOutcome, accept_and_route};
use std::net::SocketAddr;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
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
