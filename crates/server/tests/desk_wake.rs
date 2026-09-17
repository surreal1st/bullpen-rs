//! S8b-01: a reaped VM's FIRST `browse` after `docker start` must not fail
//! just because Chromium has not finished booting inside the container yet.
//!
//! The defect, reproduced on meridian 2026-09-17 (not reasoned about):
//! `cdp_for_bot` (`desk.rs`) used to hand back a freshly built `HttpCdp` the
//! instant `ensure_vm_in`'s `docker start` succeeded. `docker start`
//! returning only means the CONTAINER is up, not that the webtop's
//! Chromium is answering `/json/version` yet - so the bot's very next tool
//! call hit a raw connection error a model cannot act on. This is now the
//! COMMON path: `start_vm_reaper` (`b010c30`) genuinely hibernates every
//! bot idle 30+ minutes, and per-bot desk routing (`854d1ff`) means every
//! bot's next `browse` genuinely reaches its own just-started machine.
//!
//! Two bites, both against the REAL transport (`HttpCdp`, real TCP, real
//! WebSocket frames) rather than a `Cdp` trait fake - `desk_routing.rs`'s
//! own header doc explains why that is the stronger observable here: no
//! seam is removed between the dispatch arm and the socket.
//!
//! (a) `wake_poll_absorbs_a_reaped_desks_slow_boot_into_a_successful_browse`:
//!     a listener that refuses every connection for slightly less than
//!     `desk::WAKE_POLL_BUDGET`, then starts answering the full CDP
//!     protocol - `desk::wait_for_ready` (called from `cdp_for_bot`) must
//!     absorb that gap so the bot's `browse` still succeeds and returns the
//!     real page.
//! (b) `wake_poll_times_out_with_an_actionable_message_and_never_hangs`:
//!     nothing EVER answers - `cdp_for_bot` must return the actionable
//!     "just woke up" message (`desk::WAKING_UP_REASON`), never the raw
//!     transport error, and must not block past `desk::WAKE_POLL_BUDGET`
//!     by more than a small margin.
//!
//! Both bites proved red in this ticket's Result by temporarily reverting
//! `desk.rs`'s `cdp_for_bot` to hand back `Arc::new(HttpCdp::new(config))`
//! directly (skipping `wait_for_ready` entirely) and re-running this file -
//! never via `git checkout`.

use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use futures::{SinkExt, StreamExt};
use model::ladder::Trigger;
use model::{EventStream, ModelPort, ModelRequest};
use server::runs::RunManager;
use server::sandbox;
use server::vm::DockerRun;
use store::Db;
use store::vms::{DockerResult, VmConfig};
use tokio::io::AsyncWriteExt;
use tokio::net::TcpListener;
use tokio_tungstenite::tungstenite::Message;

/// `browse` never calls the model.
struct NeverCalledPort;

impl ModelPort for NeverCalledPort {
    fn stream(&self, _request: ModelRequest) -> EventStream {
        panic!("browse must never reach the model port");
    }
}

/// Same shape as `desk_routing.rs`'s own `StoppedThenWakes`, duplicated
/// rather than shared - see `browse_tools.rs`'s own doc on why this
/// crate's test files duplicate small fakes instead of reaching into a
/// sibling file's private helpers.
struct StoppedThenWakes;

#[async_trait]
impl DockerRun for StoppedThenWakes {
    async fn call(&self, args: &[&str], _timeout_ms: u64) -> DockerResult {
        if args.first() == Some(&"inspect") {
            DockerResult {
                ok: true,
                stdout: "exited false".to_string(),
                stderr: String::new(),
            }
        } else {
            DockerResult {
                ok: true,
                stdout: String::new(),
                stderr: String::new(),
            }
        }
    }
}

fn test_config() -> VmConfig {
    VmConfig {
        image: "test-image:latest".to_string(),
        docker_host: "unix:///test.sock".to_string(),
        cdp_base: 9500,
        web_base: 6500,
        slots: 24,
        idle_ms: 30 * 60 * 1000,
        init_dir: "/vm-init".to_string(),
        memory: "3g".to_string(),
        cpus: "1.5".to_string(),
        shm_size: "1g".to_string(),
        timezone: "America/New_York".to_string(),
        puid: "1004".to_string(),
        pgid: "1004".to_string(),
    }
}

fn insert_stopped_vm(db: &Db, bot_id: &str, container: &str, cdp_port: i32, web_port: i32) {
    db.conn()
        .execute(
            "INSERT INTO vms (bot_id, container, cdp_port, web_port, state, last_used_at) \
             VALUES (?, ?, ?, ?, 'stopped', ?)",
            rusqlite::params![
                bot_id,
                container,
                cdp_port,
                web_port,
                "2026-09-01T00:00:00Z"
            ],
        )
        .expect("insert vm row");
}

/// Reserves a port by binding then immediately dropping the listener, so
/// the caller can hand it to a VM row BEFORE anything is actually
/// listening - simulating "the container is up but Chromium has not bound
/// its port yet" exactly, which a listener bound from the start cannot: a
/// bound-but-silent listener still completes the TCP handshake, where a
/// real not-yet-booted Chromium refuses the connection outright.
async fn reserve_port() -> u16 {
    let listener = TcpListener::bind("127.0.0.1:0")
        .await
        .expect("reserve port");
    listener.local_addr().expect("local_addr").port()
}

/// Runs a minimal CDP protocol responder against `port` FOREVER (until the
/// test process ends) once it binds. Answers `GET /json/version` over
/// plain HTTP, and any WebSocket JSON-RPC request with a canned reply keyed
/// off `method`/`params.expression` - just enough for `create_window` +
/// `Page.enable` + `Page.navigate` + `read_page`'s three `Runtime.evaluate`
/// reads to all succeed, matching `HttpCdp`'s own real wire shape (a FRESH
/// connection per call, per its own doc).
async fn serve_cdp_forever(
    listener: TcpListener,
    port: u16,
    url: String,
    title: String,
    text: String,
) {
    loop {
        let (mut stream, _) = match listener.accept().await {
            Ok(v) => v,
            Err(_) => return,
        };
        let mut buf = [0u8; 1024];
        let n = stream.peek(&mut buf).await.unwrap_or(0);
        let head = String::from_utf8_lossy(&buf[..n]).to_lowercase();

        if head.contains("upgrade: websocket") {
            let Ok(mut ws) = tokio_tungstenite::accept_async(stream).await else {
                continue;
            };
            let request = loop {
                match ws.next().await {
                    Some(Ok(Message::Text(text))) => break Some(text),
                    Some(Ok(_)) => continue,
                    _ => break None,
                }
            };
            let Some(request) = request else { continue };
            let parsed: serde_json::Value = serde_json::from_str(&request).unwrap_or_default();
            let id = parsed.get("id").and_then(|v| v.as_u64()).unwrap_or(1);
            let method = parsed.get("method").and_then(|v| v.as_str()).unwrap_or("");
            let expr = parsed
                .get("params")
                .and_then(|p| p.get("expression"))
                .and_then(|v| v.as_str())
                .unwrap_or("");
            let result = match method {
                "Target.createTarget" => serde_json::json!({ "targetId": "target-1" }),
                "Runtime.evaluate" => {
                    let value = if expr == "location.href" {
                        url.as_str()
                    } else if expr == "document.title" {
                        title.as_str()
                    } else {
                        text.as_str()
                    };
                    serde_json::json!({ "result": { "value": value } })
                }
                _ => serde_json::json!({}),
            };
            let reply = serde_json::json!({ "id": id, "result": result }).to_string();
            let _ = ws.send(Message::text(reply)).await;
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
}

/// 🔴 BITE (a): guard-present world. The port is reserved but genuinely
/// refuses every connection for `LATE_BIND_DELAY` (< `desk::WAKE_POLL_BUDGET`),
/// then starts serving. If `cdp_for_bot` did not poll at all (the
/// guard-removed mutation), `HttpCdp::create_window`'s very first
/// `/json/version` request would hit the still-refusing port immediately
/// and fail with a raw connection error - this test's own Result records
/// that literal red output.
#[tokio::test]
async fn wake_poll_absorbs_a_reaped_desks_slow_boot_into_a_successful_browse() {
    const LATE_BIND_DELAY: Duration = Duration::from_millis(4_000);
    assert!(
        LATE_BIND_DELAY < server::desk::WAKE_POLL_BUDGET,
        "the test's own delay must fit inside the production poll budget, or this proves nothing"
    );

    let port = reserve_port().await;

    tokio::spawn(async move {
        tokio::time::sleep(LATE_BIND_DELAY).await;
        let listener = TcpListener::bind(("127.0.0.1", port))
            .await
            .expect("rebind the reserved port once the desk 'boots'");
        serve_cdp_forever(
            listener,
            port,
            "https://example.com/".to_string(),
            "Example".to_string(),
            "the page finally loaded".to_string(),
        )
        .await;
    });

    let db = Db::open(":memory:").expect("open :memory: db");
    server::desk::ensure_desk_tables(&db).expect("ensure desk tables");
    insert_stopped_vm(
        &db,
        "arthur",
        "bullpen-vm-arthur",
        port as i32,
        port as i32 + 1,
    );
    let db = Arc::new(Mutex::new(db));

    let manager = Arc::new(RunManager::with_sandbox_and_vm(
        Arc::clone(&db),
        Arc::new(NeverCalledPort) as Arc<dyn ModelPort>,
        sandbox::default_sandbox(),
        Arc::new(StoppedThenWakes) as Arc<dyn DockerRun>,
        Arc::new(test_config()),
        true,
    ));

    let toolbox = manager.toolbox_for("arthur", Trigger::Chat, false, "test/model", None);
    let (result, _) = toolbox
        .run("browse", r#"{"url":"https://example.com/"}"#)
        .await;

    assert!(
        result.contains("Example") && result.contains("the page finally loaded"),
        "expected a SUCCESSFUL browse once the desk finished waking up, got: {result:?}"
    );
    assert!(
        !result.contains("did not answer") && !result.contains("woke up"),
        "must not read as a failure once the desk actually answered: {result:?}"
    );
}

/// 🔴 BITE (b): guard-present world, timeout branch. Nothing EVER answers
/// on this reserved port - `cdp_for_bot` must give up after
/// `desk::WAKE_POLL_BUDGET` and hand back the actionable message, not hang
/// and not surface the raw connection error. Guard-removed (no poll at
/// all): `HttpCdp::create_window` would fail almost instantly with a raw
/// `reqwest` error instead - still bounded, but the WRONG STRING, which is
/// this bite's actual target per the ticket ("the observable is which of
/// those two strings reaches the model", not merely "did it return").
#[tokio::test]
async fn wake_poll_times_out_with_an_actionable_message_and_never_hangs() {
    let port = reserve_port().await; // reserved, then deliberately left unbound forever

    let db = Db::open(":memory:").expect("open :memory: db");
    server::desk::ensure_desk_tables(&db).expect("ensure desk tables");
    insert_stopped_vm(
        &db,
        "arthur",
        "bullpen-vm-arthur",
        port as i32,
        port as i32 + 1,
    );
    let db = Arc::new(Mutex::new(db));

    let manager = Arc::new(RunManager::with_sandbox_and_vm(
        Arc::clone(&db),
        Arc::new(NeverCalledPort) as Arc<dyn ModelPort>,
        sandbox::default_sandbox(),
        Arc::new(StoppedThenWakes) as Arc<dyn DockerRun>,
        Arc::new(test_config()),
        true,
    ));

    let toolbox = manager.toolbox_for("arthur", Trigger::Chat, false, "test/model", None);

    let started = std::time::Instant::now();
    let (result, _) = tokio::time::timeout(
        server::desk::WAKE_POLL_BUDGET + Duration::from_secs(3),
        toolbox.run("browse", r#"{"url":"https://example.com/"}"#),
    )
    .await
    .expect(
        "browse must return within WAKE_POLL_BUDGET plus a small margin, never hang past it - \
if this panics, the poll is not bounded",
    );
    let elapsed = started.elapsed();

    assert_eq!(
        result,
        "The shared computer did not answer: This bot's shared computer just woke up and is \
still starting its browser. Try again in a few seconds."
    );
    assert!(
        elapsed < server::desk::WAKE_POLL_BUDGET + Duration::from_secs(2),
        "must give up close to WAKE_POLL_BUDGET ({:?}), not wait far past it: took {elapsed:?}",
        server::desk::WAKE_POLL_BUDGET
    );
}
