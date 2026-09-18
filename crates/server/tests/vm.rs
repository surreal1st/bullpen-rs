//! Integration tests for VM lifecycle (S6-06) and the per-bot screen half
//! (S6-06b: `create_args`, `start_vm_reaper`, `vm_desk`, `desk_for`,
//! `is_png`, `thumbnail`/`clear_thumbnail_cache`, `viewer_target`,
//! `proxy_response_headers`, `build_upgrade_request`, `attach_vm_proxy`).
//!
//! All docker calls go through a recorded fake, not a real daemon - there
//! is no Docker and no browser on this workstation. Nothing here proves a
//! container, a screen capture or a socket actually worked; that is a
//! smoke test on meridian, not this file.
//!
//! Three tests below are RENAMED from S6-06 (`6f20b3a`): they were named
//! for `reset`/`doctor`, which do not exist in the TS source and were
//! never built. The bodies are unchanged; only the names now say what they
//! actually assert.

use axum::http::{HeaderMap, HeaderValue};
use server::desk::DeskConfig;
use server::vm::{
    CapturedFrame, DockerRun, FrameCapture, THUMB_CACHE_MAX_BYTES, THUMB_CACHE_MAX_ENTRIES,
    create_args, desk_for, ensure_vm, ensure_vm_in_owned, hibernate_idle,
    hibernate_idle_in_tracked, is_png, refresh_vm, start_vm_reaper, thumbnail,
    thumbnail_cache_stats, touch_vm, validate_frame, vm_desk,
};
use server::vm_proxy::{
    ProxyOutcome, attach_vm_proxy, build_upgrade_request, proxy_response_headers, viewer_target,
};
use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use store::{
    Db,
    vms::{DockerResult, VmConfig, VmRow, get_vm},
};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;

/// Fake docker runner for integration tests. Records every call.
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
        let mut responses = self.responses.lock().unwrap();
        responses.push_back(DockerResult {
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

        let mut responses = self.responses.lock().unwrap();
        responses.pop_front().unwrap_or(DockerResult {
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

#[tokio::test]
async fn provision_creates_container_when_missing() {
    let db = Db::open(":memory:").expect("open :memory: db");

    let docker = Arc::new(RecordingDockerRun::new());
    docker.push_response(true, "", "Error: no such container");
    docker.push_response(true, "", "");

    let cfg = test_config();

    let outcome = ensure_vm(&db, docker.clone(), "bot-1", "Bot One", &cfg)
        .await
        .expect("ensure_vm");

    assert!(outcome.ok);
    assert!(outcome.created);
    assert_eq!(outcome.vm.as_ref().unwrap().bot_id, "bot-1");
    assert_eq!(outcome.vm.as_ref().unwrap().state, "starting");

    let calls = docker.recorded_calls();
    assert_eq!(calls.len(), 2, "Should inspect then run");

    assert_eq!(calls[0][0], "inspect");
    assert_eq!(calls[1][0], "run");
    assert!(calls[1].iter().any(|arg| arg.contains("test-image:latest")));
}

#[tokio::test]
async fn provision_wakes_stopped_container() {
    let db = Db::open(":memory:").expect("open :memory: db");

    let docker = Arc::new(RecordingDockerRun::new());
    // Container exists but not running
    docker.push_response(true, "exited false", "");
    docker.push_response(true, "", "");

    let cfg = test_config();

    let outcome = ensure_vm(&db, docker.clone(), "bot-2", "Bot Two", &cfg)
        .await
        .expect("ensure_vm");

    assert!(outcome.ok);
    assert!(!outcome.created);
    assert_eq!(outcome.vm.as_ref().unwrap().state, "starting");

    let calls = docker.recorded_calls();
    assert_eq!(calls.len(), 2);
    assert_eq!(calls[0][0], "inspect");
    assert_eq!(calls[1][0], "start");
}

#[tokio::test]
async fn provision_reports_running_when_already_up() {
    let db = Db::open(":memory:").expect("open :memory: db");

    let docker = Arc::new(RecordingDockerRun::new());
    docker.push_response(true, "running true", "");

    let cfg = test_config();

    let outcome = ensure_vm(&db, docker.clone(), "bot-3", "Bot Three", &cfg)
        .await
        .expect("ensure_vm");

    assert!(outcome.ok);
    assert!(!outcome.created);
    assert_eq!(outcome.vm.as_ref().unwrap().state, "running");

    let calls = docker.recorded_calls();
    assert_eq!(calls.len(), 1);
    assert_eq!(calls[0][0], "inspect");
}

/// RENAMED from `reset_removes_container_but_preserves_config_volume` -
/// there is no `reset` function; this only asserts that (a) `create_args`
/// mounts the bot's config volume and (b) a manual `docker rm -f` this
/// test issues itself is recorded as a plain `rm`, never a `volume`
/// subcommand (nothing in this codebase issues one).
#[tokio::test]
async fn create_args_mounts_config_volume_and_manual_rm_leaves_it_alone() {
    let db = Db::open(":memory:").expect("open :memory: db");

    let docker = Arc::new(RecordingDockerRun::new());
    docker.push_response(true, "", "Error: no such container");
    docker.push_response(true, "", "");

    let cfg = test_config();

    let outcome = ensure_vm(&db, docker.clone(), "bot-4", "Bot Four", &cfg)
        .await
        .expect("ensure_vm");

    assert!(outcome.created);
    let container_name = outcome.vm.as_ref().unwrap().container.clone();

    // Verify the create call included the config volume
    let calls = docker.recorded_calls();
    let create_call = &calls[1];

    let volume_mentions: Vec<String> = create_call
        .iter()
        .filter(|arg| arg.contains("bullpen-vmcfg-"))
        .cloned()
        .collect();

    assert!(
        !volume_mentions.is_empty(),
        "Create args should mount config volume"
    );

    // Now simulate reset: remove container but NOT the volume
    docker.push_response(true, "", "");

    docker.call(&["rm", "-f", &container_name], 30_000).await;

    let reset_calls = docker.recorded_calls();
    let last_call = &reset_calls[reset_calls.len() - 1];

    assert_eq!(last_call[0], "rm");
    assert!(last_call.contains(&"-f".to_string()));
    assert!(last_call.contains(&container_name));

    // Verify we did NOT call volume rm
    for call in &reset_calls[2..] {
        if call[0] == "volume" {
            panic!("reset should NOT remove the config volume");
        }
    }
}

/// RENAMED from `bite_reset_volume_removal` - there is no `reset`
/// function and this is not one of S6-06b's two required bites (see
/// `viewer_target_rejects_a_decoded_traversal_into_a_neighbouring_bot`
/// and `proxy_response_headers_strips_transfer_encoding` below for those).
/// This asserts `create_args` mounts a config volume scoped to the bot id.
#[tokio::test]
async fn create_args_config_volume_mount_is_bot_scoped() {
    let db = Db::open(":memory:").expect("open :memory: db");

    let docker = Arc::new(RecordingDockerRun::new());
    docker.push_response(true, "", "Error: no such container");
    docker.push_response(true, "", "");

    let cfg = test_config();

    let outcome = ensure_vm(&db, docker.clone(), "bite-bot", "Bite", &cfg)
        .await
        .expect("ensure_vm");

    assert!(outcome.created);

    // Verify the create call included the config volume - this is the critical check
    // that proves reset must NOT remove the config volume
    let calls = docker.recorded_calls();
    let create_call = &calls[1];

    // Extract all mount points from the create args
    let mut mounts = Vec::new();
    for i in 0..create_call.len() {
        if create_call[i] == "-v" && i + 1 < create_call.len() {
            mounts.push(create_call[i + 1].clone());
        }
    }

    // The config volume MUST be mounted during creation
    let config_volume_mounted = mounts.iter().any(|m| m.contains("bullpen-vmcfg-"));
    assert!(
        config_volume_mounted,
        "Config volume must be mounted in create args. Mounts: {:?}",
        mounts
    );
}

/// RENAMED from `doctor_detects_unhealthy_container` - there is no
/// `doctor` function; this calls `ensure_vm` (to get a real container
/// name) and then just `parse_container_state`, asserting it correctly
/// reads a stopped container's `docker inspect` output. Nothing here
/// compares that reading against the row's OWN `state` column - a
/// "doctor" would.
#[tokio::test]
async fn parse_container_state_reports_stopped_after_inspect() {
    let db = Db::open(":memory:").expect("open :memory: db");

    let docker = Arc::new(RecordingDockerRun::new());
    // Create a container
    docker.push_response(true, "", "Error: no such container");
    docker.push_response(true, "", "");

    let cfg = test_config();

    ensure_vm(&db, docker.clone(), "bot-5", "Bot Five", &cfg)
        .await
        .expect("ensure_vm");

    // Check state that says it's stopped when row says running
    docker.push_response(true, "exited false", "");

    let row = get_vm(&db, "bot-5").expect("get_vm").expect("row");

    // Doctor check: container reports stopped but we think it's starting
    db.conn()
        .execute(
            "UPDATE vms SET state = ? WHERE bot_id = ?",
            rusqlite::params!["starting", "bot-5"],
        )
        .expect("update");

    let state = store::vms::parse_container_state(
        &docker
            .call(
                &[
                    "inspect",
                    "-f",
                    "{{.State.Status}} {{.State.Running}}",
                    &row.container,
                ],
                10_000,
            )
            .await,
    );

    // This should trigger the discrepancy: row says starting, docker says stopped
    assert!(state.exists);
    assert!(!state.running);

    let calls = docker.recorded_calls();
    let doctor_calls = &calls[2..];
    assert!(!doctor_calls.is_empty());
    assert_eq!(doctor_calls[0][0], "inspect");
}

#[tokio::test]
async fn hibernate_stops_idle_containers() {
    let db = Db::open(":memory:").expect("open :memory: db");

    let docker = Arc::new(RecordingDockerRun::new());

    // Create two containers
    docker.push_response(true, "", "Error: no such container");
    docker.push_response(true, "", "");
    docker.push_response(true, "", "Error: no such container");
    docker.push_response(true, "", "");

    let cfg = test_config();

    ensure_vm(&db, docker.clone(), "idle-bot", "Idle", &cfg)
        .await
        .expect("ensure_vm");

    ensure_vm(&db, docker.clone(), "active-bot", "Active", &cfg)
        .await
        .expect("ensure_vm");

    // Mark one as very old (idle)
    db.conn()
        .execute(
            "UPDATE vms SET last_used_at = '2020-01-01T00:00:00Z' WHERE bot_id = ?",
            rusqlite::params!["idle-bot"],
        )
        .expect("update");

    // Hibernate with large idle_ms should stop the idle one
    let mut short_idle_cfg = test_config();
    short_idle_cfg.idle_ms = 1_000_000; // 1 second - should catch the 2020 timestamp

    docker.push_response(true, "", "");

    let stopped = hibernate_idle(&db, docker.clone(), &short_idle_cfg)
        .await
        .expect("hibernate");

    assert!(stopped.contains(&"idle-bot".to_string()));
    assert!(!stopped.contains(&"active-bot".to_string()));

    let calls = docker.recorded_calls();
    let stop_calls: Vec<_> = calls.iter().filter(|call| call[0] == "stop").collect();

    assert!(!stop_calls.is_empty(), "Should have called docker stop");
}

#[tokio::test]
async fn refresh_updates_starting_to_running() {
    let db = Db::open(":memory:").expect("open :memory: db");

    let docker = Arc::new(RecordingDockerRun::new());
    docker.push_response(true, "", "Error: no such container");
    docker.push_response(true, "", "");

    let cfg = test_config();

    ensure_vm(&db, docker.clone(), "refresh-bot", "Refresh", &cfg)
        .await
        .expect("ensure_vm");

    let vm = get_vm(&db, "refresh-bot").expect("get_vm").expect("vm");
    assert_eq!(vm.state, "starting");

    // Refresh: check if it's running now
    docker.push_response(true, "running true", "");

    let refreshed = refresh_vm(&db, docker.clone(), "refresh-bot")
        .await
        .expect("refresh");

    assert!(refreshed.is_some());
    assert_eq!(refreshed.unwrap().state, "running");

    // Verify inspect was called
    let calls = docker.recorded_calls();
    assert!(calls.iter().any(|call| call[0] == "inspect"));
}

#[tokio::test]
async fn touch_updates_last_used() {
    let db = Db::open(":memory:").expect("open :memory: db");

    let docker = Arc::new(RecordingDockerRun::new());
    docker.push_response(true, "", "Error: no such container");
    docker.push_response(true, "", "");

    let cfg = test_config();

    ensure_vm(&db, docker.clone(), "touch-bot", "Touch", &cfg)
        .await
        .expect("ensure_vm");

    let vm_before = get_vm(&db, "touch-bot").expect("get_vm").expect("vm");

    // Touch should update last_used_at
    touch_vm(&db, "touch-bot").expect("touch");

    let vm_after = get_vm(&db, "touch-bot").expect("get_vm").expect("vm");

    assert!(vm_after.last_used_at >= vm_before.last_used_at);
}

/* ============================================================ S6-06b ============================================================ */

/// Writes a `vms` row directly, bypassing `ensure_vm`/docker, for tests
/// that only care about a row already existing.
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

/* ---------------------------------------------------------- create_args ---------------------------------------------------------- */

#[test]
fn create_args_mounts_work_and_init_volumes_with_expected_ports() {
    let row = VmRow {
        bot_id: "nozdormu".to_string(),
        container: "bullpen-vm-nozdormu".to_string(),
        cdp_port: 9301,
        web_port: 6201,
        state: "new".to_string(),
        last_used_at: "2026-09-15T00:00:00Z".to_string(),
    };
    let cfg = test_config();

    let args = create_args(&row, "Nozdormu", &cfg);

    assert!(args.iter().any(|a| a == "bullpen-vm-nozdormu"));
    assert!(args.iter().any(|a| a.contains("bullpen-vmcfg-nozdormu")));
    assert!(args.iter().any(|a| a == "127.0.0.1:9301:9223"));
    assert!(args.iter().any(|a| a == "127.0.0.1:6201:3000"));
    assert!(args.iter().any(|a| a == "TITLE=Nozdormu's screen"));
    assert_eq!(args.last().map(String::as_str), Some("test-image:latest"));
}

/* ------------------------------------------------------------ start_vm_reaper ------------------------------------------------------------ */

#[tokio::test]
async fn start_vm_reaper_stops_an_idle_vm_on_tick() {
    let db = Arc::new(Mutex::new(Db::open(":memory:").expect("open :memory: db")));
    let docker = Arc::new(RecordingDockerRun::new());
    docker.push_response(true, "", ""); // the reaper's own "stop"

    {
        let guard = db.lock().unwrap();
        insert_vm_row(
            &guard,
            &VmRow {
                bot_id: "idle-bot".to_string(),
                container: "bullpen-vm-idle-bot".to_string(),
                cdp_port: 9301,
                web_port: 6201,
                state: "running".to_string(),
                last_used_at: "2020-01-01T00:00:00Z".to_string(),
            },
        );
    }

    let mut cfg = test_config();
    cfg.idle_ms = 1_000; // anything older than 1s is idle

    let handle = start_vm_reaper(
        db.clone(),
        docker.clone(),
        Arc::new(cfg),
        Duration::from_millis(15),
        Arc::new(server::observations::DesktopStateRegistry::new()),
        Arc::new(server::observations::ObservationRegistry::new()),
    );

    // Real-time wait for at least one tick; this is timing-sensitive but
    // needs no docker/browser, only the fake and a short sleep.
    tokio::time::sleep(Duration::from_millis(150)).await;
    handle.abort();

    let calls = docker.recorded_calls();
    assert!(
        calls
            .iter()
            .any(|call| call[0] == "stop" && call.contains(&"bullpen-vm-idle-bot".to_string())),
        "reaper should have stopped the idle container, calls: {calls:?}"
    );

    let row = {
        let guard = db.lock().unwrap();
        get_vm(&guard, "idle-bot").expect("get_vm").expect("row")
    };
    assert_eq!(row.state, "stopped");
}

/* -------------------------------------------------------------- vm_desk / desk_for -------------------------------------------------------------- */

#[test]
fn vm_desk_points_at_the_bots_own_ports() {
    let row = VmRow {
        bot_id: "arthur".to_string(),
        container: "bullpen-vm-arthur".to_string(),
        cdp_port: 9305,
        web_port: 6205,
        state: "running".to_string(),
        last_used_at: "2026-09-15T00:00:00Z".to_string(),
    };
    let cfg = test_config();

    let desk = vm_desk(&row, &cfg);

    assert_eq!(desk.cdp, "http://127.0.0.1:9305");
    assert_eq!(desk.view, "http://127.0.0.1:6205");
    assert_eq!(desk.container, "bullpen-vm-arthur");
    assert_eq!(desk.docker_host, cfg.docker_host);
}

#[tokio::test]
async fn desk_for_returns_fallback_when_vms_disabled() {
    let db = Db::open(":memory:").expect("open :memory: db");
    let docker = Arc::new(RecordingDockerRun::new());
    let cfg = test_config();
    let fallback = DeskConfig {
        cdp: "http://127.0.0.1:9223".to_string(),
        view: "http://127.0.0.1:6101".to_string(),
        container: "bullpen-desk".to_string(),
        docker_host: cfg.docker_host.clone(),
    };

    let desk = desk_for(
        &db,
        docker.clone(),
        "bot-x",
        "Bot X",
        fallback.clone(),
        &cfg,
        false,
    )
    .await
    .expect("desk_for");

    assert_eq!(desk, fallback);
    assert!(
        docker.recorded_calls().is_empty(),
        "disabled VMs must never touch docker"
    );
}

#[tokio::test]
async fn desk_for_returns_the_bots_own_desk_when_enabled() {
    let db = Db::open(":memory:").expect("open :memory: db");
    let docker = Arc::new(RecordingDockerRun::new());
    docker.push_response(true, "running true", "");
    let cfg = test_config();
    let fallback = DeskConfig {
        cdp: "http://127.0.0.1:9223".to_string(),
        view: "http://127.0.0.1:6101".to_string(),
        container: "bullpen-desk".to_string(),
        docker_host: cfg.docker_host.clone(),
    };

    insert_vm_row(
        &db,
        &VmRow {
            bot_id: "bot-y".to_string(),
            container: "bullpen-vm-bot-y".to_string(),
            cdp_port: 9310,
            web_port: 6210,
            state: "running".to_string(),
            last_used_at: "2026-09-15T00:00:00Z".to_string(),
        },
    );

    let desk = desk_for(
        &db,
        docker.clone(),
        "bot-y",
        "Bot Y",
        fallback.clone(),
        &cfg,
        true,
    )
    .await
    .expect("desk_for");

    assert_ne!(desk, fallback);
    assert_eq!(desk.cdp, "http://127.0.0.1:9310");
    assert_eq!(desk.container, "bullpen-vm-bot-y");
}

/* ------------------------------------------------------------------ is_png ------------------------------------------------------------------ */

#[test]
fn is_png_accepts_real_signature_with_body() {
    let mut bytes = vec![0x89, 0x50, 0x4e, 0x47, 0x0d, 0x0a, 0x1a, 0x0a];
    bytes.extend_from_slice(&[0u8; 16]);
    assert!(is_png(&bytes));
}

#[test]
fn is_png_rejects_short_buffers_and_wrong_signature() {
    assert!(!is_png(&[]));
    assert!(!is_png(
        b"not a png at all, just an error message"[..8].as_ref()
    ));
    // Right signature, but truncated before the 16-byte floor.
    assert!(!is_png(&[0x89, 0x50, 0x4e, 0x47, 0x0d, 0x0a, 0x1a, 0x0a]));
}

#[test]
fn validate_frame_returns_original_bytes_and_native_dimensions() {
    let capture = FakeFrameCapture::new();
    let expected = capture.png.clone();
    let frame = validate_frame(expected.clone()).expect("valid PNG");
    assert_eq!(frame.png, expected);
    assert_eq!((frame.width, frame.height), (1, 1));
}

#[test]
fn validate_frame_rejects_signature_only_and_corrupt_png_data() {
    let capture = FakeFrameCapture::new();
    assert!(
        validate_frame(vec![
            0x89, 0x50, 0x4e, 0x47, 0x0d, 0x0a, 0x1a, 0x0a, 0, 0, 0, 0, 0, 0, 0, 0
        ])
        .is_none()
    );
    let mut corrupt = capture.png.clone();
    let last = corrupt.len() - 1;
    corrupt[last] ^= 1;
    assert!(validate_frame(corrupt).is_none());
}
fn encoded_png(width: u32, height: u32) -> Vec<u8> {
    let mut bytes = Vec::new();
    {
        let mut encoder = png::Encoder::new(&mut bytes, width, height);
        encoder.set_color(png::ColorType::Grayscale);
        encoder.set_depth(png::BitDepth::One);
        let mut writer = encoder.write_header().expect("write PNG header");
        let row_bytes = width.div_ceil(8) as usize;
        writer
            .write_image_data(&vec![0; row_bytes * height as usize])
            .expect("write PNG pixels");
    }
    bytes
}

#[test]
fn validate_frame_rejects_all_size_and_decode_boundaries() {
    // A valid image plus trailing bytes isolates the byte cap from PNG corruption.
    let mut oversized = encoded_png(1, 1);
    oversized.resize(server::vm::MAX_FRAME_PNG_BYTES + 1, 0);
    assert!(validate_frame(oversized).is_none());
    assert!(validate_frame(encoded_png(4_097, 1)).is_none());
    assert!(validate_frame(encoded_png(4_000, 2_001)).is_none());
    let mut truncated = encoded_png(1, 1);
    truncated.pop();
    assert!(validate_frame(truncated).is_none());
}
/* --------------------------------------------------------------- thumbnail --------------------------------------------------------------- */

/// Records every call it receives, same convention as `RecordingDockerRun`.
struct FakeFrameCapture {
    calls: Mutex<Vec<String>>,
    png: Vec<u8>,
}

impl FakeFrameCapture {
    fn new() -> Self {
        let mut png = Vec::new();
        {
            let mut encoder = png::Encoder::new(&mut png, 1, 1);
            encoder.set_color(png::ColorType::Rgba);
            encoder.set_depth(png::BitDepth::Eight);
            let mut writer = encoder.write_header().expect("write PNG header");
            writer
                .write_image_data(&[0, 0, 0, 255])
                .expect("write PNG pixel");
        }
        Self {
            calls: Mutex::new(Vec::new()),
            png,
        }
    }
}

#[async_trait::async_trait]
impl FrameCapture for FakeFrameCapture {
    async fn capture(&self, container: &str, _cfg: &VmConfig) -> Option<CapturedFrame> {
        self.calls.lock().unwrap().push(container.to_string());
        validate_frame(self.png.clone())
    }
}

#[tokio::test]
async fn thumbnail_reuses_a_cached_frame_inside_the_ttl() {
    server::vm::clear_thumbnail_cache();
    let cfg = test_config();
    let capture = FakeFrameCapture::new();

    let first = thumbnail("bullpen-vm-cached", &cfg, &capture, 1_000).await;
    let second = thumbnail("bullpen-vm-cached", &cfg, &capture, 1_500).await; // +500ms, inside 5s TTL

    assert!(first.is_some());
    assert_eq!(first, second);
    assert_eq!(
        capture.calls.lock().unwrap().len(),
        1,
        "a cache hit must not call capture again"
    );
}

#[tokio::test]
async fn thumbnail_recaptures_once_the_ttl_expires() {
    server::vm::clear_thumbnail_cache();
    let cfg = test_config();
    let capture = FakeFrameCapture::new();

    thumbnail("bullpen-vm-expiring", &cfg, &capture, 1_000).await;
    thumbnail("bullpen-vm-expiring", &cfg, &capture, 6_001).await; // +5001ms, past the 5s TTL

    assert_eq!(
        capture.calls.lock().unwrap().len(),
        2,
        "an expired entry must be recaptured"
    );
}

/// Records every call (by container) and returns a frame of a fixed byte
/// size the test chooses. THUMB-01's cap/eviction tests care about
/// `png.len()`, not a real decodable image, so this skips PNG encoding
/// entirely (unlike `FakeFrameCapture` above, which real callers of
/// `validate_frame` need a genuine PNG from).
struct SizedFrameCapture {
    calls: Mutex<Vec<String>>,
    frame_bytes: usize,
}

impl SizedFrameCapture {
    fn new(frame_bytes: usize) -> Self {
        Self {
            calls: Mutex::new(Vec::new()),
            frame_bytes,
        }
    }

    fn call_count_for(&self, container: &str) -> usize {
        self.calls
            .lock()
            .unwrap()
            .iter()
            .filter(|c| c.as_str() == container)
            .count()
    }
}

#[async_trait::async_trait]
impl FrameCapture for SizedFrameCapture {
    async fn capture(&self, container: &str, _cfg: &VmConfig) -> Option<CapturedFrame> {
        self.calls.lock().unwrap().push(container.to_string());
        Some(CapturedFrame {
            png: vec![0xAB; self.frame_bytes],
            width: 1,
            height: 1,
        })
    }
}

#[tokio::test]
async fn thumbnail_caps_entry_count_at_the_configured_bound() {
    server::vm::clear_thumbnail_cache();
    let cfg = test_config();
    // 1 KiB frames: far under the byte cap, so only the entry cap can bind.
    let capture = SizedFrameCapture::new(1_024);

    for i in 0..THUMB_CACHE_MAX_ENTRIES + 5 {
        let container = format!("bullpen-vm-count-{i}");
        thumbnail(&container, &cfg, &capture, 1_000).await;
    }

    let (entries, _bytes) = thumbnail_cache_stats();
    assert!(
        entries <= THUMB_CACHE_MAX_ENTRIES,
        "entry count {entries} exceeded the cap of {THUMB_CACHE_MAX_ENTRIES}"
    );
}

#[tokio::test]
async fn thumbnail_caps_aggregate_bytes_at_the_configured_bound() {
    server::vm::clear_thumbnail_cache();
    let cfg = test_config();
    // 8 frames at a quarter of the byte cap each guarantee the byte cap
    // trips (32 MiB net demand at 4x the cap) long before the 64-entry
    // count cap would ever matter.
    let frame_bytes = THUMB_CACHE_MAX_BYTES / 4;
    let capture = SizedFrameCapture::new(frame_bytes);

    for i in 0..8 {
        let container = format!("bullpen-vm-bytes-{i}");
        thumbnail(&container, &cfg, &capture, 1_000).await;
    }

    let (_entries, bytes) = thumbnail_cache_stats();
    assert!(
        bytes <= THUMB_CACHE_MAX_BYTES,
        "aggregate bytes {bytes} exceeded the cap of {THUMB_CACHE_MAX_BYTES}"
    );
}

#[tokio::test]
async fn thumbnail_evicts_the_oldest_entry_first() {
    server::vm::clear_thumbnail_cache();
    let cfg = test_config();
    let capture = SizedFrameCapture::new(1_024);

    // Fill to exactly the entry cap, each with a distinct `at` so "oldest"
    // is unambiguous: container 0 is the oldest, container cap-1 the
    // newest.
    for i in 0..THUMB_CACHE_MAX_ENTRIES {
        let container = format!("bullpen-vm-evict-{i}");
        thumbnail(&container, &cfg, &capture, 1_000 + i as i64).await;
    }
    assert_eq!(capture.call_count_for("bullpen-vm-evict-0"), 1);

    // One more distinct container must evict the oldest entry to fit.
    let evict_at = 1_000 + THUMB_CACHE_MAX_ENTRIES as i64;
    thumbnail("bullpen-vm-evict-new", &cfg, &capture, evict_at).await;

    // Still inside "bullpen-vm-evict-0"'s original TTL window (its `at` was
    // 1_000, TTL is 5_000ms) - if it were still cached this would be a
    // cache hit with zero new capture calls. It is not: it was evicted.
    thumbnail("bullpen-vm-evict-0", &cfg, &capture, evict_at + 1).await;
    assert_eq!(
        capture.call_count_for("bullpen-vm-evict-0"),
        2,
        "the oldest entry must have been evicted, not served stale"
    );
}

#[tokio::test]
async fn thumbnail_sweeps_expired_entries_on_insert() {
    server::vm::clear_thumbnail_cache();
    let cfg = test_config();
    let capture = SizedFrameCapture::new(1_024);

    thumbnail("bullpen-vm-sweep-old", &cfg, &capture, 1_000).await;
    let (entries_before, _bytes) = thumbnail_cache_stats();
    assert_eq!(entries_before, 1);

    // Past the 5s TTL relative to the first insert. Inserting a second,
    // different container must sweep the first out rather than let it
    // linger resident alongside the new one for the life of the process.
    thumbnail("bullpen-vm-sweep-new", &cfg, &capture, 6_001).await;

    let (entries_after, _bytes) = thumbnail_cache_stats();
    assert_eq!(
        entries_after, 1,
        "the expired entry must be swept, not left resident alongside the new one"
    );
}

/* -------------------------------------------------------------- viewer_target -------------------------------------------------------------- */

#[test]
fn viewer_target_parses_the_bot_id_and_rest_of_path() {
    let target = viewer_target("/api/bots/nozdormu/vm/view/websockets").expect("should match");
    assert_eq!(target.bot_id, "nozdormu");
    assert_eq!(target.rest, "websockets");
}

#[test]
fn viewer_target_allows_one_mount_prefix_segment() {
    // The lazy `(?:/[^/]+)??` outer group: an app mounted under one path
    // segment (e.g. a reverse proxy prefix) still matches.
    let target = viewer_target("/app/api/bots/nozdormu/vm/view").expect("should match");
    assert_eq!(target.bot_id, "nozdormu");
    assert_eq!(target.rest, "");
}

#[test]
fn viewer_target_returns_none_for_non_viewer_paths() {
    assert!(viewer_target("/api/bots/nozdormu/messages").is_none());
    assert!(viewer_target("/").is_none());
}

/// S6-06b bite (a). Two worlds:
/// - GUARD PRESENT (shipped `viewer_target`): a decoded bot id that
///   contains a path separator is REJECTED - `None`.
/// - GUARD REMOVED (the guard's `if` deleted, as TS's literal body does):
///   `%2e%2e%2fother-bot` decodes to `../other-bot` and is accepted as a
///   bot id, wired straight into `get_vm`'s exact lookup and, if anything
///   downstream ever compares this by prefix instead of equality, into a
///   neighbouring bot's screen.
///
/// Observable that differs: `viewer_target(...).is_none()`.
#[test]
fn viewer_target_rejects_a_decoded_traversal_into_a_neighbouring_bot() {
    let path = "/api/bots/%2e%2e%2fother-bot/vm/view";
    assert!(
        viewer_target(path).is_none(),
        "a decoded bot id containing '/' must never be accepted"
    );
}

/* ---------------------------------------------------------- proxy_response_headers ---------------------------------------------------------- */

#[test]
fn proxy_response_headers_keeps_ordinary_headers_and_forces_no_store() {
    let mut source = HeaderMap::new();
    source.insert("content-type", HeaderValue::from_static("image/png"));
    source.insert("etag", HeaderValue::from_static("\"abc\""));

    let out = proxy_response_headers(&source);

    assert_eq!(out.get("content-type").unwrap(), "image/png");
    assert_eq!(out.get("etag").unwrap(), "\"abc\"");
    assert_eq!(out.get("cache-control").unwrap(), "no-store");
}

/// S6-W-06 bite (a). Two worlds:
/// - GUARD PRESENT (shipped `proxy_response_headers`, `content-encoding` NOT
///   on `HOP_BY_HOP`): a `content-encoding: gzip` on the upstream response
///   survives into the proxied headers, still describing the (still
///   compressed - this crate's `reqwest` never decompresses) body a caller
///   receives.
/// - GUARD REMOVED (`content-encoding` put back on `HOP_BY_HOP`, the shape
///   this shipped in until S6-W-06): the header is stripped, leaving a
///   browser with compressed bytes and no label saying so - it renders them
///   as text/html mojibake instead of a desktop (found on the SHIPPED
///   server at `9bc393a` by opening the shot, not by any test that existed
///   then).
///
/// Observable that differs: `out.get("content-encoding")`.
#[test]
fn proxy_response_headers_keeps_content_encoding() {
    let mut source = HeaderMap::new();
    source.insert("content-type", HeaderValue::from_static("text/html"));
    source.insert("content-encoding", HeaderValue::from_static("gzip"));

    let out = proxy_response_headers(&source);

    assert_eq!(
        out.get("content-encoding").unwrap(),
        "gzip",
        "content-encoding is END-TO-END per RFC 9110 and must survive - without it a \
         browser (every browser sends accept-encoding: gzip) renders the still-compressed \
         body as text"
    );
}

/// S6-06b bite (b). Two worlds:
/// - GUARD PRESENT (shipped `proxy_response_headers`): every header in
///   `HOP_BY_HOP` (`connection`, `transfer-encoding`, ...) is stripped
///   before the response reaches a caller.
/// - GUARD REMOVED (the `HOP_BY_HOP` filter deleted): a hop-by-hop header
///   describing ONE connection's framing (`transfer-encoding`) passes
///   straight through to a client that has no business seeing it, per
///   RFC 9110.
///
/// Observable that differs: `out.get("transfer-encoding").is_none()`.
#[test]
fn proxy_response_headers_strips_transfer_encoding() {
    let mut source = HeaderMap::new();
    source.insert("transfer-encoding", HeaderValue::from_static("chunked"));
    source.insert("connection", HeaderValue::from_static("keep-alive"));

    let out = proxy_response_headers(&source);

    assert!(out.get("transfer-encoding").is_none());
    assert!(out.get("connection").is_none());
}

/* --------------------------------------------------------------- build_upgrade_request --------------------------------------------------------------- */

#[test]
fn build_upgrade_request_rewrites_host_and_drops_the_session() {
    let mut headers = HeaderMap::new();
    headers.insert("host", HeaderValue::from_static("bullpen.example.com"));
    headers.insert("cookie", HeaderValue::from_static("bullpen_session=secret"));
    headers.insert("authorization", HeaderValue::from_static("Bearer secret"));
    headers.insert("upgrade", HeaderValue::from_static("websocket"));
    headers.insert(
        "sec-websocket-key",
        HeaderValue::from_static("dGhlIHNhbXBsZSBub25jZQ=="),
    );

    let request = build_upgrade_request("/websockets", &headers, 6201);

    assert!(request.starts_with("GET /websockets HTTP/1.1\r\n"));
    assert!(request.contains("Host: 127.0.0.1:6201\r\n"));
    assert!(request.contains("sec-websocket-key: dGhlIHNhbXBsZSBub25jZQ==\r\n"));
    assert!(!request.contains("bullpen.example.com"));
    assert!(!request.to_lowercase().contains("cookie:"));
    assert!(!request.to_lowercase().contains("authorization:"));
    assert!(request.ends_with("\r\n\r\n"));
}

/* --------------------------------------------------------------- attach_vm_proxy --------------------------------------------------------------- */

#[tokio::test]
async fn attach_vm_proxy_ignores_a_non_viewer_path() {
    let db = Db::open(":memory:").expect("open :memory: db");
    let (client, _held) = tokio::io::duplex(64);

    let outcome =
        attach_vm_proxy(client, "/api/conversations/1", &HeaderMap::new(), &db, true).await;

    assert_eq!(outcome, ProxyOutcome::NotAViewerPath);
}

#[tokio::test]
async fn attach_vm_proxy_refuses_without_a_valid_session() {
    let db = Db::open(":memory:").expect("open :memory: db");
    let (client, mut held) = tokio::io::duplex(256);

    let outcome = attach_vm_proxy(
        client,
        "/api/bots/some-bot/vm/view",
        &HeaderMap::new(),
        &db,
        true,
    )
    .await;

    assert_eq!(outcome, ProxyOutcome::Unauthorized);

    let mut response = Vec::new();
    let _ = tokio::time::timeout(Duration::from_millis(200), held.read_to_end(&mut response)).await;
    let text = String::from_utf8_lossy(&response);
    assert!(text.starts_with("HTTP/1.1 401"));
}

#[tokio::test]
async fn attach_vm_proxy_reports_unknown_bot_when_auth_is_off() {
    let db = Db::open(":memory:").expect("open :memory: db");
    let (client, _held) = tokio::io::duplex(256);

    let outcome = attach_vm_proxy(
        client,
        "/api/bots/nobody-here/vm/view",
        &HeaderMap::new(),
        &db,
        false,
    )
    .await;

    assert_eq!(outcome, ProxyOutcome::UnknownBot);
}

#[tokio::test]
async fn attach_vm_proxy_relays_bytes_between_client_and_the_bots_upstream() {
    let db = Db::open(":memory:").expect("open :memory: db");

    // A real loopback listener stands in for the container's web desktop -
    // this proves the RELAY wiring (connect, send the handshake, pipe both
    // directions), not that a real container or browser is on the other
    // end of it.
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
    let web_port = listener.local_addr().expect("local_addr").port() as i32;

    insert_vm_row(
        &db,
        &VmRow {
            bot_id: "relay-bot".to_string(),
            container: "bullpen-vm-relay-bot".to_string(),
            cdp_port: 9399,
            web_port,
            state: "running".to_string(),
            last_used_at: "2026-09-15T00:00:00Z".to_string(),
        },
    );

    let upstream_task = tokio::spawn(async move {
        let (mut upstream, _addr) = listener.accept().await.expect("accept");
        let mut buf = [0u8; 4096];
        let n = upstream.read(&mut buf).await.expect("read handshake");
        let handshake = String::from_utf8_lossy(&buf[..n]).to_string();
        upstream
            .write_all(b"HTTP/1.1 101 Switching Protocols\r\nUpgrade: websocket\r\n\r\nHELLO")
            .await
            .expect("write response");
        handshake
    });

    let (client, mut held) = tokio::io::duplex(4096);

    let outcome = attach_vm_proxy(
        client,
        "/api/bots/relay-bot/vm/view/websockets",
        &HeaderMap::new(),
        &db,
        false,
    )
    .await;

    assert_eq!(outcome, ProxyOutcome::Proxying);

    let handshake = tokio::time::timeout(Duration::from_secs(2), upstream_task)
        .await
        .expect("upstream task did not finish")
        .expect("upstream task panicked");
    assert!(handshake.starts_with("GET /websockets HTTP/1.1\r\n"));
    assert!(handshake.contains("Host: 127.0.0.1"));

    // The upstream's own handshake response is relayed byte for byte, same
    // as TS's `upstream.pipe(socket)` - so the client sees the 101 line
    // FIRST, then the body, not just the body.
    let expected = b"HTTP/1.1 101 Switching Protocols\r\nUpgrade: websocket\r\n\r\nHELLO";
    let mut relayed = vec![0u8; expected.len()];
    tokio::time::timeout(Duration::from_secs(2), held.read_exact(&mut relayed))
        .await
        .expect("timed out waiting for relayed bytes")
        .expect("read relayed bytes");
    assert_eq!(relayed.as_slice(), expected.as_slice());
}

/* --------------------------------------------------- S8c-02: call_with_stdin */

/// NOT one of S8c-02's three required bites (those live in
/// `server::tools::desk_shell`'s own test module, against the `DockerRun`
/// trait level). Extra coverage for `RealDockerRun::call_with_stdin`
/// itself: `RealDockerRun`'s own doc (`vm.rs`) says every test drives a
/// `DockerRun`-level fake instead of this struct, because there is no
/// Docker on this workstation to prove a real `docker exec -i` against -
/// but `RealDockerRun` wraps a `sandbox::CommandRunner`, and
/// `sandbox::FakeRunner` (already `pub`, already used by
/// `DockerSandbox`'s own tests one layer up) is a `CommandRunner` fake that
/// spawns nothing real either. Constructing `RealDockerRun` over a
/// `FakeRunner` proves the Rust-level plumbing this ticket added - `docker`
/// prefixed onto the given argv, `stdin` threaded through as bytes rather
/// than folded into argv, the SAME `timeout_ms` `call` would get - without
/// claiming anything about a real child process's stdin actually closing.
/// That last part is `sandbox::TokioRunner::run`'s own job, and per this
/// ticket's own bite (c), the only thing that can prove THAT for real is
/// the meridian smoke test in S8c-03.
#[tokio::test]
async fn real_docker_run_call_with_stdin_threads_the_payload_through_as_bytes_not_argv() {
    let runner = Arc::new(server::sandbox::FakeRunner::new());
    runner.push_response("typed ok", "", 0);
    let docker = server::vm::RealDockerRun::new(runner.clone());

    let result = docker
        .call_with_stdin(
            &[
                "exec",
                "-i",
                "-u",
                "abc",
                "some-container",
                "bash",
                "-lc",
                "xdotool type --file -",
            ],
            "; rm -rf / `echo pwned`",
            45_000,
        )
        .await;

    assert!(result.ok);
    assert_eq!(result.stdout, "typed ok");

    let commands = runner.commands();
    assert_eq!(commands.len(), 1);
    assert_eq!(
        commands[0][0], "docker",
        "call_with_stdin must prefix docker, same as call"
    );
    assert!(
        !commands[0]
            .iter()
            .any(|a| a.contains("rm -rf") || a.contains("pwned")),
        "the payload must never appear in argv, got {:?}",
        commands[0]
    );

    let stdins = runner.stdins();
    assert_eq!(stdins.len(), 1);
    assert_eq!(
        stdins[0],
        "; rm -rf / `echo pwned`".as_bytes(),
        "the payload must reach CommandRunner::run's own stdin parameter, as bytes"
    );
}

#[test]
fn validate_frame_rejects_invalid_compression_even_with_valid_chunk_crcs() {
    // Valid signature, IHDR, chunk checksums and IEND; the IDAT zlib stream is invalid.
    // A header/chunk-only validator accepts this, so it isolates full pixel decoding.
    let png = vec![
        137, 80, 78, 71, 13, 10, 26, 10, 0, 0, 0, 13, 73, 72, 68, 82, 0, 0, 0, 1, 0, 0, 0, 1, 8, 6,
        0, 0, 0, 31, 21, 196, 137, 0, 0, 0, 4, 73, 68, 65, 84, 0, 0, 0, 0, 234, 35, 231, 7, 0, 0,
        0, 0, 73, 69, 78, 68, 174, 66, 96, 130,
    ];
    assert!(validate_frame(png).is_none());
}

#[test]
fn validate_frame_rejects_valid_compression_with_missing_pixels() {
    // Valid chunks and a valid empty zlib stream, but IHDR promises a full RGBA pixel.
    // Chunk-only validation cannot detect the missing decompressed scanline.
    let png = vec![
        137, 80, 78, 71, 13, 10, 26, 10, 0, 0, 0, 13, 73, 72, 68, 82, 0, 0, 0, 1, 0, 0, 0, 1, 8, 6,
        0, 0, 0, 31, 21, 196, 137, 0, 0, 0, 11, 73, 68, 65, 84, 120, 1, 1, 0, 0, 255, 255, 0, 0, 0,
        1, 137, 214, 174, 95, 0, 0, 0, 0, 73, 69, 78, 68, 174, 66, 96, 130,
    ];
    assert!(validate_frame(png).is_none());
}

fn store_vm_observation(
    observations: &server::observations::ObservationRegistry,
    admission: &Arc<server::observations::ObservationAdmission>,
    run_id: &str,
    bot_id: &str,
) {
    let observation = admission
        .try_begin_capture()
        .expect("observation admission")
        .retain(
            CapturedFrame {
                png: vec![1, 2, 3],
                width: 10,
                height: 10,
            },
            run_id,
            bot_id,
            "00000000-0000-4000-8000-000000000099",
            "2026-09-18T12:00:00Z",
            0,
        );
    observations.store(&observation);
}

#[tokio::test]
async fn failed_create_and_start_attempts_invalidate_before_docker_mutates() {
    for existing in [false, true] {
        let db = Arc::new(Mutex::new(Db::open(":memory:").unwrap()));
        if existing {
            db.lock()
                .unwrap()
                .conn()
                .execute(
                    "INSERT INTO vms (bot_id, container, cdp_port, web_port, state, last_used_at)
                     VALUES ('bot-1', 'bullpen-vm-bot-1', 9301, 6201, 'stopped', '2026-09-18T12:00:00Z')",
                    [],
                )
                .unwrap();
        }
        let docker = Arc::new(RecordingDockerRun::new());
        docker.push_response(
            true,
            if existing { "exited false" } else { "" },
            if existing { "" } else { "no such container" },
        );
        docker.push_response(false, "", "fixture mutation failed");
        let observations = Arc::new(server::observations::ObservationRegistry::new());
        let admission = Arc::new(server::observations::ObservationAdmission::new());
        let desktop_states = Arc::new(server::observations::DesktopStateRegistry::new());
        store_vm_observation(&observations, &admission, "run-stale", "bot-1");

        let (outcome, desktop) = ensure_vm_in_owned(
            Arc::clone(&db),
            docker.clone(),
            "bot-1".into(),
            "Bot One".into(),
            Arc::new(test_config()),
            desktop_states,
            Arc::clone(&observations),
        )
        .await
        .expect("tracked ensure returns an outcome");

        assert!(!outcome.ok);
        assert_eq!(desktop.generation(), 1);
        assert!(observations.metadata("run-stale").is_none());
        let calls = docker.recorded_calls();
        assert_eq!(calls.len(), 2);
        assert_eq!(calls[1][0], if existing { "start" } else { "run" });
    }
}

#[tokio::test]
async fn failed_hibernate_attempt_invalidates_before_docker_stop() {
    let db = Arc::new(Mutex::new(Db::open(":memory:").unwrap()));
    db.lock()
        .unwrap()
        .conn()
        .execute(
            "INSERT INTO vms (bot_id, container, cdp_port, web_port, state, last_used_at)
             VALUES ('bot-1', 'bullpen-vm-bot-1', 9301, 6201, 'running', '2020-01-01T00:00:00Z')",
            [],
        )
        .unwrap();
    let docker = Arc::new(RecordingDockerRun::new());
    docker.push_response(false, "", "fixture stop failed");
    let observations = Arc::new(server::observations::ObservationRegistry::new());
    let admission = Arc::new(server::observations::ObservationAdmission::new());
    let desktop_states = Arc::new(server::observations::DesktopStateRegistry::new());
    store_vm_observation(&observations, &admission, "run-stale", "bot-1");
    let mut cfg = test_config();
    cfg.idle_ms = 0;

    let stopped = hibernate_idle_in_tracked(
        Arc::clone(&db),
        docker.clone(),
        Arc::new(cfg),
        Arc::clone(&desktop_states),
        Arc::clone(&observations),
    )
    .await
    .expect("tracked hibernate");

    assert!(
        stopped.is_empty(),
        "failed docker stop is not reported as stopped"
    );
    assert!(observations.metadata("run-stale").is_none());
    assert_eq!(desktop_states.for_bot("bot-1").lock().await.generation(), 1);
    assert_eq!(docker.recorded_calls()[0][0], "stop");
}

struct MutationTimingDocker {
    observations: Arc<server::observations::ObservationRegistry>,
    inspect_stdout: &'static str,
    inspect_stderr: &'static str,
    expected_mutation: &'static str,
    checked_before_mutation: AtomicBool,
}

#[async_trait::async_trait]
impl DockerRun for MutationTimingDocker {
    async fn call(&self, args: &[&str], _timeout_ms: u64) -> DockerResult {
        match args.first().copied() {
            Some("inspect") => DockerResult {
                ok: true,
                stdout: self.inspect_stdout.into(),
                stderr: self.inspect_stderr.into(),
            },
            Some(command) if command == self.expected_mutation => {
                assert!(
                    self.observations.is_empty(),
                    "{command} reached Docker while a pre-mutation observation was still valid"
                );
                self.checked_before_mutation.store(true, Ordering::SeqCst);
                DockerResult {
                    ok: false,
                    stdout: String::new(),
                    stderr: "fixture mutation failure".into(),
                }
            }
            other => panic!("unexpected docker call: {other:?}"),
        }
    }
}

#[tokio::test]
async fn create_start_and_stop_invalidate_before_the_docker_mutation_call() {
    for (existing, inspect_stdout, inspect_stderr, expected) in [
        (false, "", "no such container", "run"),
        (true, "exited false", "", "start"),
    ] {
        let db = Arc::new(Mutex::new(Db::open(":memory:").unwrap()));
        if existing {
            db.lock()
                .unwrap()
                .conn()
                .execute(
                    "INSERT INTO vms (bot_id, container, cdp_port, web_port, state, last_used_at)
                     VALUES ('bot-1', 'bullpen-vm-bot-1', 9301, 6201, 'stopped', '2026-09-18T12:00:00Z')",
                    [],
                )
                .unwrap();
        }
        let observations = Arc::new(server::observations::ObservationRegistry::new());
        let admission = Arc::new(server::observations::ObservationAdmission::new());
        store_vm_observation(&observations, &admission, "run-timing", "bot-1");
        let desktop_states = Arc::new(server::observations::DesktopStateRegistry::new());
        let docker = Arc::new(MutationTimingDocker {
            observations: Arc::clone(&observations),
            inspect_stdout,
            inspect_stderr,
            expected_mutation: expected,
            checked_before_mutation: AtomicBool::new(false),
        });
        let _ = ensure_vm_in_owned(
            db,
            Arc::clone(&docker) as Arc<dyn DockerRun>,
            "bot-1".into(),
            "Bot One".into(),
            Arc::new(test_config()),
            desktop_states,
            observations,
        )
        .await
        .expect("tracked ensure returns failed outcome");
        assert!(docker.checked_before_mutation.load(Ordering::SeqCst));
    }

    let db = Arc::new(Mutex::new(Db::open(":memory:").unwrap()));
    db.lock()
        .unwrap()
        .conn()
        .execute(
            "INSERT INTO vms (bot_id, container, cdp_port, web_port, state, last_used_at)
             VALUES ('bot-1', 'bullpen-vm-bot-1', 9301, 6201, 'running', '2020-01-01T00:00:00Z')",
            [],
        )
        .unwrap();
    let observations = Arc::new(server::observations::ObservationRegistry::new());
    let admission = Arc::new(server::observations::ObservationAdmission::new());
    store_vm_observation(&observations, &admission, "run-stop-timing", "bot-1");
    let docker = Arc::new(MutationTimingDocker {
        observations: Arc::clone(&observations),
        inspect_stdout: "",
        inspect_stderr: "",
        expected_mutation: "stop",
        checked_before_mutation: AtomicBool::new(false),
    });
    let mut cfg = test_config();
    cfg.idle_ms = 0;
    hibernate_idle_in_tracked(
        db,
        Arc::clone(&docker) as Arc<dyn DockerRun>,
        Arc::new(cfg),
        Arc::new(server::observations::DesktopStateRegistry::new()),
        observations,
    )
    .await
    .expect("tracked hibernate");
    assert!(docker.checked_before_mutation.load(Ordering::SeqCst));
}
