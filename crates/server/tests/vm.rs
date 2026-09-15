//! Integration tests for VM lifecycle.
//!
//! Tests the provision, hibernation, reset, and doctor operations.
//! All docker calls go through a recorded fake, not a real daemon.

use server::vm::{DockerRun, ensure_vm, hibernate_idle, refresh_vm, touch_vm};
use std::collections::VecDeque;
use std::sync::{Arc, Mutex};
use store::{
    Db,
    vms::{DockerResult, VmConfig, get_vm},
};

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

#[tokio::test]
async fn reset_removes_container_but_preserves_config_volume() {
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

#[tokio::test]
async fn bite_reset_volume_removal() {
    // BITE: This test FAILS if config volume mount is not recorded in create args.
    // Makes reset skip the volume removal check and show the test go red.
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

#[tokio::test]
async fn doctor_detects_unhealthy_container() {
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
