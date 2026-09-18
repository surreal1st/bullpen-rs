//! S8c-03 bite (d): `desk_act` execs into the CALLING bot's own container,
//! never a shared or another bot's - same shape as
//! `tests/desk_shell_routing.rs`'s bite (a), which this file's fake and
//! test structure are deliberately copied from rather than reinvented, and
//! the same class of bug `tests/desk_routing.rs` (S8a-02, `browse`/
//! `read_page`) exists for. Proved the same way: two bots, two VM rows,
//! two different containers, and the observable is which container name
//! the fake `DockerRun` actually saw on the `docker exec` call.
//!
//! Drives `server::runs::RunManager::toolbox_for(...).run("desk_act", ...)`
//! directly - the exact dispatch arm S8c-03 added in `tools/mod.rs` -
//! against two bots with `state = 'running'` rows, the same way
//! `desk_shell_routing.rs` does.

use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use model::ladder::Trigger;
use model::{EventStream, ModelPort, ModelRequest};
use server::runs::RunManager;
use server::sandbox;
use server::vm::DockerRun;
use store::Db;
use store::vms::{DockerResult, VmConfig};

struct NeverCalledPort;

impl ModelPort for NeverCalledPort {
    fn stream(&self, _request: ModelRequest) -> EventStream {
        panic!("desk_act must never reach the model port");
    }
}

/// Answers every `docker inspect` with "running true" (no wake needed) and
/// every `docker exec` with a scripted, container-tagged stdout - same
/// shape as `desk_shell_routing.rs`'s own `RunningVms`.
#[derive(Default)]
struct RunningVms {
    /// Every call's full argv, in order. `exec`'s container is argv[7] -
    /// same fixed shape `desk_shell::desk_shell_result` builds
    /// (`exec, -u, abc, -w, /workspace, -e, HOME=/config, <container>, ...`),
    /// which `desk_act`'s `key` action runs through unchanged.
    calls: Mutex<Vec<Vec<String>>>,
}

impl RunningVms {
    fn exec_calls(&self) -> Vec<Vec<String>> {
        self.calls
            .lock()
            .unwrap()
            .iter()
            .filter(|argv| argv.first().map(String::as_str) == Some("exec"))
            .cloned()
            .collect()
    }
}

#[async_trait]
impl DockerRun for RunningVms {
    async fn call(&self, args: &[&str], _timeout_ms: u64) -> DockerResult {
        let owned: Vec<String> = args.iter().map(|s| s.to_string()).collect();
        self.calls.lock().unwrap().push(owned);

        match args.first() {
            Some(&"inspect") => DockerResult {
                ok: true,
                stdout: "running true".to_string(),
                stderr: String::new(),
            },
            Some(&"exec") => {
                let container = args.get(7).copied().unwrap_or("?");
                DockerResult {
                    ok: true,
                    stdout: format!("ran on {container}\n"),
                    stderr: String::new(),
                }
            }
            _ => DockerResult {
                ok: true,
                stdout: String::new(),
                stderr: String::new(),
            },
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

fn insert_running_vm(db: &Db, bot_id: &str, container: &str, cdp_port: i32, web_port: i32) {
    db.conn()
        .execute(
            "INSERT INTO vms (bot_id, container, cdp_port, web_port, state, last_used_at) \
             VALUES (?, ?, ?, ?, 'running', ?)",
            rusqlite::params![
                bot_id,
                container,
                cdp_port,
                web_port,
                "2026-09-17T00:00:00Z"
            ],
        )
        .expect("insert vm row");
}

/// 🔴 BITE (d) target.
#[tokio::test]
async fn desk_act_execs_into_each_bots_own_container() {
    let db = Db::open(":memory:").expect("open :memory: db");
    server::desk::ensure_desk_tables(&db).expect("ensure desk tables");
    insert_running_vm(&db, "bot-a", "bullpen-vm-bot-a", 9501, 6501);
    insert_running_vm(&db, "bot-b", "bullpen-vm-bot-b", 9502, 6502);
    let db = Arc::new(Mutex::new(db));

    let docker_fake = Arc::new(RunningVms::default());
    let docker: Arc<dyn DockerRun> = Arc::clone(&docker_fake) as Arc<dyn DockerRun>;
    let manager = Arc::new(RunManager::with_sandbox_and_vm(
        Arc::clone(&db),
        Arc::new(NeverCalledPort) as Arc<dyn ModelPort>,
        sandbox::default_sandbox(),
        docker,
        Arc::new(test_config()),
        true,
    ));

    let one_key_action = r#"{"actions":[{"kind":"key","keys":"a"}]}"#;

    let toolbox_a = manager.toolbox_for("bot-a", Trigger::Chat, false, "test/model", None);
    let (result_a, _) = toolbox_a.run("desk_act", one_key_action).await;

    let toolbox_b = manager.toolbox_for("bot-b", Trigger::Chat, false, "test/model", None);
    let (result_b, _) = toolbox_b.run("desk_act", one_key_action).await;

    assert!(
        result_a.contains("ran on bullpen-vm-bot-a"),
        "bot-a's result must show its own container: {result_a:?}"
    );
    assert!(
        result_b.contains("ran on bullpen-vm-bot-b"),
        "bot-b's result must show its own container: {result_b:?}"
    );

    // The routing proof this file exists for: the `exec` call's own argv,
    // not just the returned string, named each bot's OWN container -
    // container is argv[7], same fixed shape `desk_shell_routing.rs` checks.
    let exec_calls = docker_fake.exec_calls();
    assert_eq!(exec_calls.len(), 2, "expected exactly one exec per bot");
    assert_eq!(exec_calls[0][7], "bullpen-vm-bot-a");
    assert_eq!(exec_calls[1][7], "bullpen-vm-bot-b");
}

/// `vm_enabled = false`: refused with the shared "did not answer" sentence
/// BEFORE `vm::desk_for_in` (and therefore docker) is ever touched - same
/// shape as `desk_shell_routing.rs`'s identical test.
#[tokio::test]
async fn desk_act_refuses_before_touching_docker_when_vms_are_disabled() {
    let db = Db::open(":memory:").expect("open :memory: db");
    server::desk::ensure_desk_tables(&db).expect("ensure desk tables");
    insert_running_vm(&db, "bot-a", "bullpen-vm-bot-a", 9501, 6501);
    let db = Arc::new(Mutex::new(db));

    struct PanicsIfCalled;
    #[async_trait]
    impl DockerRun for PanicsIfCalled {
        async fn call(&self, _args: &[&str], _timeout_ms: u64) -> DockerResult {
            panic!("vm_enabled=false must never reach docker");
        }
    }

    let manager = Arc::new(RunManager::with_sandbox_and_vm(
        Arc::clone(&db),
        Arc::new(NeverCalledPort) as Arc<dyn ModelPort>,
        sandbox::default_sandbox(),
        Arc::new(PanicsIfCalled),
        Arc::new(test_config()),
        false,
    ));

    let toolbox = manager.toolbox_for("bot-a", Trigger::Chat, false, "test/model", None);
    let (result, _) = toolbox
        .run("desk_act", r#"{"actions":[{"kind":"key","keys":"a"}]}"#)
        .await;

    assert_eq!(
        result,
        "The shared computer did not answer: Per-bot machines are off here. There is nothing to \
run a command on."
    );
}
