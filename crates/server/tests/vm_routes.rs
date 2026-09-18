//! S6-W-01: proves the VM routes (`crates/server/src/routes/vms.rs`) are
//! actually reachable from a running server - the S6 review's whole
//! complaint was that nothing in `vm.rs` had a route at all. Every test
//! here drives the routes over real HTTP (`tower::ServiceExt::oneshot`,
//! same posture `tests/hooks_routes.rs` already uses), through a recording
//! fake `vm::DockerRun` - there is no Docker on this workstation, so
//! nothing here may claim a container actually started; the meridian smoke
//! test (S6-W-04) is the only proof of that.

mod common;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use common::seed_session;
use serde_json::{Value, json};
use server::vm::DockerRun;
use std::collections::VecDeque;
use std::sync::{Arc, Mutex};
use std::time::Duration;
use store::Db;
use store::vms::{DockerResult, VmConfig, VmRow};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;
use tower::ServiceExt;

/// Fake docker runner. Records every call it receives - the RECORDED
/// ARGUMENT LIST is what every assertion in this file checks, never a
/// return value, per this ticket's own constraint (no Docker here).
struct RecordingDockerRun {
    calls: Arc<Mutex<Vec<Vec<String>>>>,
    responses: Arc<Mutex<VecDeque<DockerResult>>>,
}

impl RecordingDockerRun {
    fn new() -> Self {
        Self {
            calls: Arc::new(Mutex::new(Vec::new())),
            responses: Arc::new(Mutex::new(VecDeque::new())),
        }
    }

    fn push_response(&self, ok: bool, stdout: &str, stderr: &str) {
        self.responses.lock().unwrap().push_back(DockerResult {
            ok,
            stdout: stdout.to_string(),
            stderr: stderr.to_string(),
        });
    }

    fn recorded_calls(&self) -> Vec<Vec<String>> {
        self.calls.lock().unwrap().clone()
    }
}

#[async_trait::async_trait]
impl DockerRun for RecordingDockerRun {
    async fn call(&self, args: &[&str], _timeout_ms: u64) -> DockerResult {
        self.calls
            .lock()
            .unwrap()
            .push(args.iter().map(|s| s.to_string()).collect());
        self.responses
            .lock()
            .unwrap()
            .pop_front()
            .unwrap_or(DockerResult {
                ok: true,
                stdout: String::new(),
                stderr: String::new(),
            })
    }
}

fn test_config() -> VmConfig {
    VmConfig {
        image: "test-image:latest".to_string(),
        docker_host: "unix:///test.sock".to_string(),
        cdp_base: 9300,
        web_base: 6200,
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

fn seed_bot(db: &Db, id: &str, name: &str) {
    db.conn()
        .execute(
            "INSERT INTO bots (id, name, purpose, instructions, model, created_at) VALUES (?1, ?2, '', ?3, NULL, '2026-01-01T00:00:00Z')",
            rusqlite::params![id, name, format!("You are {name}.")],
        )
        .expect("seed bot");
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

async fn send(req: Request<Body>, router: axum::Router) -> (StatusCode, Value) {
    let resp = router.oneshot(req).await.expect("oneshot");
    let status = resp.status();
    let bytes = axum::body::to_bytes(resp.into_body(), 8 * 1024 * 1024)
        .await
        .expect("collect body");
    let body = if bytes.is_empty() {
        Value::Null
    } else {
        serde_json::from_slice(&bytes).unwrap_or_else(|e| {
            panic!(
                "response body is JSON: {e}; status={status}; raw={:?}",
                String::from_utf8_lossy(&bytes)
            )
        })
    };
    (status, body)
}

fn get(uri: &str, cookie: &str) -> Request<Body> {
    Request::builder()
        .method("GET")
        .uri(uri)
        .header("Cookie", cookie)
        .body(Body::empty())
        .expect("build GET request")
}

fn post(uri: &str, cookie: &str) -> Request<Body> {
    Request::builder()
        .method("POST")
        .uri(uri)
        .header("Cookie", cookie)
        .body(Body::empty())
        .expect("build POST request")
}

/* --------------------------------------------------------- ensure / status --------------------------------------------------------- */

#[tokio::test]
async fn ensure_provisions_a_new_vm_and_returns_its_view() {
    let db = Db::open(":memory:").expect("open :memory: db");
    let session = seed_session(&db);
    seed_bot(&db, "arthur", "Arthur");

    let docker = Arc::new(RecordingDockerRun::new());
    docker.push_response(true, "", "Error: no such container");
    docker.push_response(true, "", "");
    let docker_dyn: Arc<dyn DockerRun> = docker.clone();

    let state = server::AppState::with_vm(db, docker_dyn, test_config(), true);
    let router = server::build_app(state);

    let (status, body) = send(post("/api/bots/arthur/vm/ensure", &session), router).await;
    assert_eq!(status, StatusCode::OK, "body: {body:?}");
    assert_eq!(body["available"], true);
    assert_eq!(body["state"], "starting");
    assert!(
        body["viewPath"]
            .as_str()
            .unwrap()
            .starts_with("/api/bots/arthur/vm/view/")
    );

    let calls = docker.recorded_calls();
    assert_eq!(calls[0][0], "inspect");
    assert_eq!(calls[1][0], "run");
}

#[tokio::test]
async fn ensure_returns_404_for_a_bot_that_does_not_exist() {
    let db = Db::open(":memory:").expect("open :memory: db");
    let session = seed_session(&db);

    let docker: Arc<dyn DockerRun> = Arc::new(RecordingDockerRun::new());
    let state = server::AppState::with_vm(db, docker, test_config(), true);
    let router = server::build_app(state);

    let (status, body) = send(post("/api/bots/nobody/vm/ensure", &session), router).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert_eq!(body["error"], "no such bot");
}

#[tokio::test]
async fn status_route_settles_a_starting_row_without_provisioning() {
    let db = Db::open(":memory:").expect("open :memory: db");
    let session = seed_session(&db);
    seed_bot(&db, "arthur", "Arthur");
    insert_vm_row(
        &db,
        &VmRow {
            bot_id: "arthur".to_string(),
            container: "bullpen-vm-arthur".to_string(),
            cdp_port: 9301,
            web_port: 6201,
            state: "starting".to_string(),
            last_used_at: "2026-09-15T00:00:00Z".to_string(),
        },
    );

    let docker = Arc::new(RecordingDockerRun::new());
    docker.push_response(true, "running true", "");
    let docker_dyn: Arc<dyn DockerRun> = docker.clone();

    let state = server::AppState::with_vm(db, docker_dyn, test_config(), true);
    let router = server::build_app(state);

    let (status, body) = send(get("/api/bots/arthur/vm", &session), router).await;
    assert_eq!(status, StatusCode::OK, "body: {body:?}");
    assert_eq!(body["state"], "running");

    let calls = docker.recorded_calls();
    assert_eq!(calls.len(), 1, "a settle read costs exactly one inspect");
    assert_eq!(calls[0][0], "inspect");
}

/* ------------------------------------------------------------------- list -------------------------------------------------------------------- */

#[tokio::test]
async fn list_reports_every_bots_machine() {
    let db = Db::open(":memory:").expect("open :memory: db");
    let session = seed_session(&db);
    seed_bot(&db, "arthur", "Arthur");
    insert_vm_row(
        &db,
        &VmRow {
            bot_id: "arthur".to_string(),
            container: "bullpen-vm-arthur".to_string(),
            cdp_port: 9301,
            web_port: 6201,
            state: "running".to_string(),
            last_used_at: "2026-09-15T00:00:00Z".to_string(),
        },
    );

    let docker: Arc<dyn DockerRun> = Arc::new(RecordingDockerRun::new());
    let state = server::AppState::with_vm(db, docker, test_config(), true);
    let router = server::build_app(state);

    let (status, body) = send(get("/api/vms", &session), router).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["enabled"], true);
    let vms = body["vms"].as_array().expect("vms array");
    assert_eq!(vms.len(), 1);
    assert_eq!(vms[0]["botId"], "arthur");
    assert_eq!(vms[0]["state"], "running");
}

/* --------------------------------------------------------------- hibernate --------------------------------------------------------------- */

#[tokio::test]
async fn hibernate_route_stops_an_idle_vm() {
    let db = Db::open(":memory:").expect("open :memory: db");
    let session = seed_session(&db);
    seed_bot(&db, "idle-bot", "Idle");
    insert_vm_row(
        &db,
        &VmRow {
            bot_id: "idle-bot".to_string(),
            container: "bullpen-vm-idle-bot".to_string(),
            cdp_port: 9301,
            web_port: 6201,
            state: "running".to_string(),
            last_used_at: "2020-01-01T00:00:00Z".to_string(),
        },
    );

    let docker = Arc::new(RecordingDockerRun::new());
    docker.push_response(true, "", "");
    let docker_dyn: Arc<dyn DockerRun> = docker.clone();

    let mut cfg = test_config();
    cfg.idle_ms = 1_000;
    let state = server::AppState::with_vm(db, docker_dyn, cfg, true);
    let router = server::build_app(state);

    let (status, body) = send(post("/api/vms/hibernate", &session), router).await;
    assert_eq!(status, StatusCode::OK, "body: {body:?}");
    let stopped = body["stopped"].as_array().expect("stopped array");
    assert!(stopped.iter().any(|v| v == "idle-bot"));

    let calls = docker.recorded_calls();
    assert!(calls.iter().any(|c| c[0] == "stop"));
}

/* --------------------------------------------------------- auth gate ------------------------------------------------------------------- */

#[tokio::test]
async fn every_vm_route_requires_a_session() {
    let db = Db::open(":memory:").expect("open :memory: db");
    // seed_session sets a password (is_configured = true) but this request
    // presents no cookie at all, so it is UNAUTHENTICATED, not "unconfigured".
    let _password_set = seed_session(&db);
    seed_bot(&db, "arthur", "Arthur");

    let docker: Arc<dyn DockerRun> = Arc::new(RecordingDockerRun::new());
    let state = server::AppState::with_vm(db, docker, test_config(), true);
    let router = server::build_app(state);

    let req = Request::builder()
        .method("GET")
        .uri("/api/bots/arthur/vm")
        .body(Body::empty())
        .expect("build request with no cookie");
    let resp = router.oneshot(req).await.expect("oneshot");
    assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
}

/* ============================================================ BITE (a) ============================================================ */
//
// Two worlds:
// - GUARD PRESENT (shipped `get_thumbnail`/`view_proxy`): a bot with no
//   `vms` row gets a plain 404 - `state.vm_enabled` is true, docker is
//   reachable, and NOTHING is provisioned just because someone asked to
//   look at (or load the desktop page for) a machine that does not exist
//   yet. Matches TS `vm-routes.ts`'s own contract: only `POST .../vm/ensure`
//   ever creates one.
// - GUARD REMOVED (a route that called `ensure_vm_in`/`vm_desk` instead of
//   a bare `get_vm` before answering): the SAME request would silently boot
//   a container, and the RecordingDockerRun would show an `inspect`/`run`
//   pair instead of nothing.
//
// Observable that differs: `docker.recorded_calls()` - empty under the
// shipped guard, non-empty the moment a read-only route starts a machine.

#[tokio::test]
async fn thumbnail_404s_for_a_bot_with_no_vm_row_and_never_provisions() {
    let db = Db::open(":memory:").expect("open :memory: db");
    let session = seed_session(&db);
    seed_bot(&db, "arthur", "Arthur"); // bot exists; no `vms` row for it

    let docker = Arc::new(RecordingDockerRun::new());
    let docker_dyn: Arc<dyn DockerRun> = docker.clone();
    let state = server::AppState::with_vm(db, docker_dyn, test_config(), true);
    let router = server::build_app(state);

    let (status, body) = send(get("/api/bots/arthur/vm/thumbnail.png", &session), router).await;
    assert_eq!(status, StatusCode::NOT_FOUND, "body: {body:?}");
    assert_eq!(body["error"], "no machine");
    assert!(
        docker.recorded_calls().is_empty(),
        "a bot with no VM row must never provision one just to look at it: {:?}",
        docker.recorded_calls()
    );
}

#[tokio::test]
async fn view_proxy_404s_for_a_bot_with_no_vm_row_and_never_provisions() {
    let db = Db::open(":memory:").expect("open :memory: db");
    let session = seed_session(&db);
    seed_bot(&db, "arthur", "Arthur");

    let docker = Arc::new(RecordingDockerRun::new());
    let docker_dyn: Arc<dyn DockerRun> = docker.clone();
    let state = server::AppState::with_vm(db, docker_dyn, test_config(), true);
    let router = server::build_app(state);

    let (status, body) = send(get("/api/bots/arthur/vm/view/websockets", &session), router).await;
    assert_eq!(status, StatusCode::NOT_FOUND, "body: {body:?}");
    assert_eq!(body["error"], "no machine");
    assert!(
        docker.recorded_calls().is_empty(),
        "opening a bot's screen must never provision a machine that does not exist: {:?}",
        docker.recorded_calls()
    );
}

/* ============================================================ BITE (b) ============================================================ */
//
// Two worlds:
// - GUARD PRESENT (shipped routes): every route checks `state.vm_enabled`
//   BEFORE touching `vm::ensure_vm_in`/`refresh_vm_in`/`hibernate_idle_in`,
//   and `vm.rs`'s own `default_docker_run` gives a disabled server
//   `DisabledDockerRun` in production anyway - so `BULLPEN_VM` off means
//   NOTHING here ever reaches the `RecordingDockerRun` at all, and every
//   route still answers with a coherent status (never a 500, never a
//   silent 200 pretending a machine exists).
// - GUARD REMOVED (any one route's `if !state.vm_enabled { ... }` branch
//   deleted): that ONE route would call through to `ensure_vm_in`/
//   `refresh_vm_in`/`hibernate_idle_in` regardless, and the fake docker
//   (scripted with a response) would record a call - it never panics on an
//   unscripted call the way `sandbox::FakeRunner` does, so a silently
//   working route would not even crash a test that forgot to assert this;
//   only checking the recorded calls catches it.
//
// Observable that differs: `docker.recorded_calls()` after every route below
// has been hit - empty under the shipped guard, non-empty the moment ANY
// one of them forgets its own check.
#[tokio::test]
async fn bullpen_vm_off_leaves_every_route_refusing_and_never_touches_docker() {
    let db = Db::open(":memory:").expect("open :memory: db");
    let session = seed_session(&db);
    seed_bot(&db, "arthur", "Arthur");
    // Deliberately no `vms` row - the disabled-server state a real deploy
    // with BULLPEN_VM unset would be in for every bot.

    // Scripted generously: if any route's `enabled` guard were missing,
    // these responses let the call SUCCEED instead of merely erroring, so a
    // "silently half-works" route would not accidentally look broken for
    // some other reason - only the recorded-calls assertion below would
    // catch it.
    let docker = Arc::new(RecordingDockerRun::new());
    for _ in 0..4 {
        docker.push_response(true, "running true", "");
    }
    let docker_dyn: Arc<dyn DockerRun> = docker.clone();

    let state = server::AppState::with_vm(db, docker_dyn, test_config(), false);
    let router = server::build_app(state);

    let (status, body) = send(get("/api/vms", &session), router.clone()).await;
    assert_eq!(status, StatusCode::OK, "list: {body:?}");
    assert_eq!(body["enabled"], false);
    assert_eq!(body["vms"].as_array().unwrap().len(), 0);

    let (status, body) = send(get("/api/bots/arthur/vm", &session), router.clone()).await;
    assert_eq!(status, StatusCode::OK, "status: {body:?}");
    assert_eq!(body["available"], false);

    let (status, body) = send(post("/api/bots/arthur/vm/ensure", &session), router.clone()).await;
    assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE, "ensure: {body:?}");
    assert_eq!(body["available"], false);

    let (status, body) = send(post("/api/vms/hibernate", &session), router.clone()).await;
    assert_eq!(
        status,
        StatusCode::SERVICE_UNAVAILABLE,
        "hibernate: {body:?}"
    );

    let (status, body) = send(
        get("/api/bots/arthur/vm/thumbnail.png", &session),
        router.clone(),
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND, "thumbnail: {body:?}");

    let (status, body) = send(get("/api/bots/arthur/vm/view/websockets", &session), router).await;
    assert_eq!(status, StatusCode::NOT_FOUND, "view: {body:?}");

    assert!(
        docker.recorded_calls().is_empty(),
        "BULLPEN_VM off must leave every route refusing, never half-working: {:?}",
        docker.recorded_calls()
    );
}

/// A narrower companion to the bite above: proves the JSON responses read as
/// coherent "off" states, not as a machine that happens to exist - the
/// difference between "refusing" and "half-working" is as much about what
/// the client is TOLD as what docker was asked to do.
#[tokio::test]
async fn disabled_status_view_never_claims_a_machine_exists() {
    let db = Db::open(":memory:").expect("open :memory: db");
    let session = seed_session(&db);
    seed_bot(&db, "arthur", "Arthur");

    let docker: Arc<dyn DockerRun> = Arc::new(RecordingDockerRun::new());
    let state = server::AppState::with_vm(db, docker, test_config(), false);
    let router = server::build_app(state);

    let (status, body) = send(get("/api/bots/arthur/vm", &session), router).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(
        body,
        json!({
            "available": false,
            "state": "unavailable",
            "container": null,
            "cdpPort": null,
            "webPort": null,
            "lastUsedAt": null,
            "detail": "Per-bot machines are off on this server. Set BULLPEN_VM=on where they are wanted.",
            "viewPath": null,
        })
    );
}

/// 🔴 The `viewPath` this API hands out must actually ROUTE.
///
/// Found on the shipped server, not by a test: axum's `{*rest}` wildcard
/// does not match an empty remainder, so `/api/bots/{id}/vm/view/` - the
/// exact string `ensure` returns as `viewPath`, and where `/vm/view`
/// redirects - fell through to a 404, while `/vm/view/index.html` routed
/// fine. Opening a bot's screen was a redirect into a dead end.
///
/// Two worlds: with the trailing-slash route registered, the advertised
/// path reaches the proxy handler (which then fails on its own terms -
/// the fake docker has no container to reach - and NOT with 404 NOT_FOUND).
/// Without it, the path is not routed at all. The observable that separates
/// them is the status code being anything other than 404.
#[tokio::test]
async fn the_view_path_this_api_advertises_is_actually_routed() {
    let db = Db::open(":memory:").expect("open :memory: db");
    let session = seed_session(&db);
    seed_bot(&db, "arthur", "Arthur");

    let docker = Arc::new(RecordingDockerRun::new());
    docker.push_response(true, "", "Error: no such container");
    docker.push_response(true, "", "");
    let docker_dyn: Arc<dyn DockerRun> = docker.clone();
    let state = server::AppState::with_vm(db, docker_dyn, test_config(), true);
    let router = server::build_app(state);

    let (status, body) = send(post("/api/bots/arthur/vm/ensure", &session), router.clone()).await;
    assert_eq!(status, StatusCode::OK, "body: {body:?}");
    let advertised = body["viewPath"].as_str().expect("viewPath").to_string();

    let req = Request::builder()
        .method("GET")
        .uri(&advertised)
        .header("cookie", &session)
        .body(Body::empty())
        .expect("build request");
    let response = router.oneshot(req).await.expect("send");

    assert_ne!(
        response.status(),
        StatusCode::NOT_FOUND,
        "the viewPath this API hands out ({advertised}) must be routed - it 404'd, which is \
         what opening a bot's screen did on the shipped server"
    );
}

/* ============================================================ S6-W-06 ============================================================ */
//
// The bite the header-level unit test alone cannot cover (`tests/vm.rs`'s
// `proxy_response_headers_keeps_content_encoding`, which drives
// `proxy_response_headers` directly): this ticket's own text says that test
// "still passes if some later code strips the header somewhere else on the
// path" - `view_proxy` (`routes/vms.rs`) is that "somewhere else". It is
// the only place that actually assembles the response a browser receives,
// and it is reachable only through the real route, not by calling
// `proxy_response_headers` in isolation.
//
// Two worlds:
// - GUARD PRESENT (shipped `view_proxy` + `proxy_response_headers` with
//   `content-encoding` off `HOP_BY_HOP`): a client behind this route
//   receives the upstream's bytes UNCHANGED and the `content-encoding:
//   gzip` header describing them.
// - GUARD REMOVED (`content-encoding` put back on `HOP_BY_HOP` - the shape
//   this shipped in at `9bc393a` - or any other code on the path stripping
//   it before the response leaves `view_proxy`): the same compressed bytes
//   reach the client with no header saying so - the exact shape a browser
//   (every browser sends `accept-encoding: gzip`) rendered as mojibake on
//   the shipped server, found by opening the shot, not by any test.
//
// Observable that differs: the proxied response's `content-encoding`
// header, and that the body bytes are exactly the upstream's.
#[tokio::test]
async fn view_proxy_preserves_gzip_bytes_and_their_content_encoding_header() {
    let db = Db::open(":memory:").expect("open :memory: db");
    let session = seed_session(&db);
    seed_bot(&db, "dora", "Dora");

    // Ground truth captured on meridian (this ticket's own report) for
    // `accept-encoding: gzip`: 200, `content-type: text/html`, first bytes
    // `1f8b0800...` (a real gzip member), no `content-encoding` header on
    // the broken shape. This is that same gzip member.
    let gzip_body: &[u8] = &[
        0x1f, 0x8b, 0x08, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x03, 0xcb, 0x48, 0xcd, 0xc9, 0xc9,
        0x07, 0x00, 0x86, 0xa6, 0x10, 0x36, 0x05, 0x00, 0x00, 0x00,
    ];

    // A real loopback listener stands in for the container's web desktop -
    // there is no Docker and no browser on this workstation (this file's
    // own constraint), so this proves the PROXY wiring, not that a real
    // container answered.
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
    let web_port = listener.local_addr().expect("local_addr").port() as i32;

    let upstream_task = tokio::spawn({
        let gzip_body = gzip_body.to_vec();
        async move {
            let (mut upstream, _addr) = listener.accept().await.expect("accept");
            let mut buf = [0u8; 4096];
            // Drain the request; this fake only needs to know one arrived.
            let _ = upstream.read(&mut buf).await.expect("read request");
            let head = format!(
                "HTTP/1.1 200 OK\r\ncontent-type: text/html\r\ncontent-encoding: gzip\r\ncontent-length: {}\r\nconnection: close\r\n\r\n",
                gzip_body.len()
            );
            upstream
                .write_all(head.as_bytes())
                .await
                .expect("write response head");
            upstream
                .write_all(&gzip_body)
                .await
                .expect("write response body");
            upstream.shutdown().await.expect("shutdown");
        }
    });

    insert_vm_row(
        &db,
        &VmRow {
            bot_id: "dora".to_string(),
            container: "bullpen-vm-dora".to_string(),
            cdp_port: 9301,
            web_port,
            state: "running".to_string(),
            last_used_at: "2026-09-15T00:00:00Z".to_string(),
        },
    );

    let docker: Arc<dyn DockerRun> = Arc::new(RecordingDockerRun::new());
    let state = server::AppState::with_vm(db, docker, test_config(), true);
    let router = server::build_app(state);

    // The exact path the ticket's ground truth was captured against, and
    // every browser sends `accept-encoding: gzip` - the bug reproduces
    // exactly when a real client would send this.
    let req = Request::builder()
        .method("GET")
        .uri("/api/bots/dora/vm/view/")
        .header("cookie", &session)
        .header("accept-encoding", "gzip")
        .body(Body::empty())
        .expect("build request");
    let response = router.oneshot(req).await.expect("send");

    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(
        response
            .headers()
            .get("content-encoding")
            .map(|v| v.to_str().unwrap()),
        Some("gzip"),
        "the client must be told the body is gzip-compressed, or a browser renders it as text"
    );
    let body = axum::body::to_bytes(response.into_body(), 8 * 1024 * 1024)
        .await
        .expect("collect body");
    assert_eq!(
        body.as_ref(),
        gzip_body,
        "the proxy must relay the upstream's bytes unchanged - this crate's reqwest client \
         never decompresses (no `gzip` feature), so anything other than the exact bytes means \
         something on the path mangled the body"
    );

    tokio::time::timeout(Duration::from_secs(2), upstream_task)
        .await
        .expect("upstream task did not finish")
        .expect("upstream task panicked");
}

/* ============================================================ S8c-01 ============================================================ */
//
// `GET /api/bots/:id/desk` - is the bot's OWN browser actually answering,
// as opposed to `GET /api/bots/:id/vm` (above) which only reports the
// CONTAINER's state. Ticket: `.scratch/bullpen-rs/tickets/S8c-01-desk-status.md`.
//
// Two required bites, both reusing THIS file's `RecordingDockerRun` per the
// ticket's own instruction not to build a second fake:
//
// (a) A status check never starts a machine. Guard-present: a `stopped`
//     row's `GET .../desk` answers without touching docker at all - see
//     `desk_status_never_wakes_a_stopped_machine_to_check_it` below.
//     Guard-removed: the handler resolves the desk the way a tool does
//     (`desk_for_in`/`ensure_vm_in`) and `RecordingDockerRun` would show a
//     `start`/`run`/`create` call. Observable: `docker.recorded_calls()`.
//
// (b) A running container with a dead browser reads as NOT ok.
//     Guard-present: `desk_status_reports_not_ok_when_the_container_is_running_but_the_browser_is_dead`
//     below - a `running` row whose probe gets a real HTTP 502 answers
//     `ok: false` with the status wording. Guard-removed: the handler
//     reports container state instead of the probe's answer (e.g. `ok:
//     true` whenever the row is `running`, regardless of what the browser
//     said). Observable: the `ok` field and `detail` string.

/// A one-shot HTTP listener that answers `GET /json/version` with exactly
/// `status_line`/`body`, then closes - stands in for the bot's own
/// Chromium. Duplicated from `tests/desk.rs`'s own `spawn_version_listener`
/// rather than shared: each integration test file in this crate compiles
/// as its own binary, and this ticket owns no shared `tests/common`
/// addition for a two-call helper.
async fn spawn_desk_probe_listener(status_line: &'static str, body: &'static str) -> i32 {
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
    let port = listener.local_addr().expect("local_addr").port() as i32;

    tokio::spawn(async move {
        let (mut stream, _addr) = listener.accept().await.expect("accept");
        let mut buf = [0u8; 1024];
        let _ = stream.read(&mut buf).await;
        let head = format!(
            "HTTP/1.1 {status_line}\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n",
            body.len()
        );
        stream.write_all(head.as_bytes()).await.expect("write head");
        stream.write_all(body.as_bytes()).await.expect("write body");
        let _ = stream.shutdown().await;
    });

    port
}

#[tokio::test]
async fn desk_status_route_404s_for_a_bot_that_does_not_exist() {
    let db = Db::open(":memory:").expect("open :memory: db");
    let session = seed_session(&db);

    let docker: Arc<dyn DockerRun> = Arc::new(RecordingDockerRun::new());
    let state = server::AppState::with_vm(db, docker, test_config(), true);
    let router = server::build_app(state);

    let (status, body) = send(get("/api/bots/nobody/desk", &session), router).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert_eq!(body["error"], "no such bot");
}

#[tokio::test]
async fn desk_status_route_reports_off_when_vm_support_is_disabled() {
    let db = Db::open(":memory:").expect("open :memory: db");
    let session = seed_session(&db);
    seed_bot(&db, "arthur", "Arthur");

    let docker = Arc::new(RecordingDockerRun::new());
    let docker_dyn: Arc<dyn DockerRun> = docker.clone();
    let state = server::AppState::with_vm(db, docker_dyn, test_config(), false);
    let router = server::build_app(state);

    let (status, body) = send(get("/api/bots/arthur/desk", &session), router).await;
    assert_eq!(status, StatusCode::OK, "body: {body:?}");
    assert_eq!(body["ok"], false);
    assert!(
        docker.recorded_calls().is_empty(),
        "VM support off must never touch docker: {:?}",
        docker.recorded_calls()
    );
}

/// 🔴 BITE (a) target - see this section's header for the two worlds.
#[tokio::test]
async fn desk_status_never_wakes_a_stopped_machine_to_check_it() {
    let db = Db::open(":memory:").expect("open :memory: db");
    let session = seed_session(&db);
    seed_bot(&db, "arthur", "Arthur");
    insert_vm_row(
        &db,
        &VmRow {
            bot_id: "arthur".to_string(),
            container: "bullpen-vm-arthur".to_string(),
            cdp_port: 9301,
            web_port: 6201,
            state: "stopped".to_string(),
            last_used_at: "2026-09-15T00:00:00Z".to_string(),
        },
    );

    let docker = Arc::new(RecordingDockerRun::new());
    let docker_dyn: Arc<dyn DockerRun> = docker.clone();
    let state = server::AppState::with_vm(db, docker_dyn, test_config(), true);
    let router = server::build_app(state);

    let (status, body) = send(get("/api/bots/arthur/desk", &session), router).await;
    assert_eq!(status, StatusCode::OK, "body: {body:?}");
    assert_eq!(body["ok"], false);
    assert_eq!(
        body["detail"],
        "This bot's machine is not running, so there is no browser to check."
    );
    assert!(
        docker.recorded_calls().is_empty(),
        "a status check on a stopped machine must never touch docker, let alone start it: {:?}",
        docker.recorded_calls()
    );
}

/// 🔴 BITE (b) target - see this section's header for the two worlds. A
/// real HTTP 502 stands in for "the browser answered, and answered badly" -
/// the alternative the ticket itself offers ("Unreachable (or a 502)"),
/// chosen because it is deterministic and fast (no timeout to wait out),
/// unlike a genuinely refused connection.
#[tokio::test]
async fn desk_status_reports_not_ok_when_the_container_is_running_but_the_browser_is_dead() {
    let db = Db::open(":memory:").expect("open :memory: db");
    let session = seed_session(&db);
    seed_bot(&db, "arthur", "Arthur");

    let cdp_port = spawn_desk_probe_listener("502 Bad Gateway", "bad gateway").await;
    insert_vm_row(
        &db,
        &VmRow {
            bot_id: "arthur".to_string(),
            container: "bullpen-vm-arthur".to_string(),
            cdp_port,
            web_port: 6201,
            state: "running".to_string(),
            last_used_at: "2026-09-15T00:00:00Z".to_string(),
        },
    );

    let docker = Arc::new(RecordingDockerRun::new());
    let docker_dyn: Arc<dyn DockerRun> = docker.clone();
    let state = server::AppState::with_vm(db, docker_dyn, test_config(), true);
    let router = server::build_app(state);

    let (status, body) = send(get("/api/bots/arthur/desk", &session), router).await;
    assert_eq!(status, StatusCode::OK, "body: {body:?}");
    assert_eq!(body["ok"], false, "body: {body:?}");
    assert_eq!(body["detail"], "The desk browser answered 502.");
    // A `running` row costs no docker call either (`refresh_vm_in` only
    // ever calls docker to settle a `starting` row) - the container's own
    // state was never in question here, only its browser.
    assert!(docker.recorded_calls().is_empty());
}

/// The companion happy path: a `running` row whose browser genuinely
/// answers reads as `ok: true` with its name - proves the route's SUCCESS
/// path end to end (real HTTP through `RealCdpVersion`), not just the two
/// bites above.
#[tokio::test]
async fn desk_status_route_answers_ok_when_the_bots_browser_is_up() {
    let db = Db::open(":memory:").expect("open :memory: db");
    let session = seed_session(&db);
    seed_bot(&db, "arthur", "Arthur");

    let cdp_port = spawn_desk_probe_listener("200 OK", r#"{"Browser":"Chrome/128.0.0.0"}"#).await;
    insert_vm_row(
        &db,
        &VmRow {
            bot_id: "arthur".to_string(),
            container: "bullpen-vm-arthur".to_string(),
            cdp_port,
            web_port: 6201,
            state: "running".to_string(),
            last_used_at: "2026-09-15T00:00:00Z".to_string(),
        },
    );

    let docker: Arc<dyn DockerRun> = Arc::new(RecordingDockerRun::new());
    let state = server::AppState::with_vm(db, docker, test_config(), true);
    let router = server::build_app(state);

    let (status, body) = send(get("/api/bots/arthur/desk", &session), router).await;
    assert_eq!(status, StatusCode::OK, "body: {body:?}");
    assert_eq!(body["ok"], true, "body: {body:?}");
    assert_eq!(body["detail"], "Chrome/128.0.0.0 on this bot's machine.");
}
