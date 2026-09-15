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
use store::Db;
use store::vms::{DockerResult, VmConfig, VmRow};
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
