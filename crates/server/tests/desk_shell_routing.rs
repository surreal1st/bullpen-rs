//! S8b-02 bite (a): `desk_shell` execs into the CALLING bot's own container,
//! never a shared or another bot's - same class of bug S8a-02's
//! `tests/desk_routing.rs` exists for (`browse`/`read_page`), proven the
//! same way: two bots, two VM rows, two different containers, and the
//! observable is which container name the fake `DockerRun` actually saw on
//! the `docker exec` call - not the returned string, which for two
//! identically-scripted bots would read the same either way.
//!
//! Drives `server::runs::RunManager::toolbox_for(...).run("desk_shell", ...)`
//! directly - the exact dispatch arm this ticket added in `tools/mod.rs` -
//! against two bots with `state = 'running'` rows (no `docker start` in the
//! way; `desk_shell` needs no VM wake-poll the way `browse`'s `Cdp` does, so
//! this only needs one `docker inspect` per call before the real `exec`).
//!
//! Guard-present world: `tools/mod.rs`'s `"desk_shell"` arm resolves through
//! `desk::desk_config_for_bot(&db, ..., &bot_id)`, so bot A's call execs
//! into `bullpen-vm-bot-a` and bot B's into `bullpen-vm-bot-b`. Guard-removed
//! world (the same mutation that caught S8a-02): the dispatch arm ignores
//! `bot_id` and always resolves bot A's config - proved red in this
//! ticket's Result by temporarily hardcoding `"bot-a"` in place of `&bot_id`
//! in that call, then reverted.

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
        panic!("desk_shell must never reach the model port");
    }
}

/// Answers every `docker inspect` with "running true" (no wake needed) and
/// every `docker exec` with a scripted, container-tagged stdout so a test
/// can tell, from the RETURNED text alone, which container actually ran -
/// belt-and-suspenders on top of the `containers_seen` log below, which is
/// the log this ticket's bite actually turns on.
#[derive(Default)]
struct RunningVms {
    /// Every call's full argv, in order - `tests/desk_routing.rs`'s own
    /// `containers_seen` shape, widened to the whole argv because `exec`'s
    /// container is not its LAST argument the way `inspect`/`start`'s is
    /// (`bash -lc <command>` follows it) - see this crate's
    /// `tools::desk_shell::run_desk_shell` for the exact argv order.
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
                // Tag the container name into stdout so the test also has
                // a result-string check available, on top of the argv log.
                // Index 7 in `run_desk_shell`'s fixed argv: exec, -u, abc,
                // -w, /workspace, -e, HOME=/config, <container>, ...
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

/// 🔴 BITE (a) target.
#[tokio::test]
async fn desk_shell_execs_into_each_bots_own_container() {
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

    let toolbox_a = manager.toolbox_for("bot-a", Trigger::Chat, false, "test/model", None);
    let (result_a, _) = toolbox_a
        .run("desk_shell", r#"{"command":"echo hi"}"#)
        .await;

    let toolbox_b = manager.toolbox_for("bot-b", Trigger::Chat, false, "test/model", None);
    let (result_b, _) = toolbox_b
        .run("desk_shell", r#"{"command":"echo hi"}"#)
        .await;

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
    // container is argv[7] in `run_desk_shell`'s fixed
    // `exec -u abc -w /workspace -e HOME=/config <container> bash -lc
    // <command>` shape.
    let exec_calls = docker_fake.exec_calls();
    assert_eq!(exec_calls.len(), 2, "expected exactly one exec per bot");
    assert_eq!(exec_calls[0][7], "bullpen-vm-bot-a");
    assert_eq!(exec_calls[1][7], "bullpen-vm-bot-b");
}

/// `vm_enabled = false`: refused with the shared "did not answer" sentence
/// BEFORE `vm::desk_for_in` (and therefore docker) is ever touched - same
/// shape as `desk_routing.rs`'s identical test for `browse`.
#[tokio::test]
async fn desk_shell_refuses_before_touching_docker_when_vms_are_disabled() {
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
    let (result, _) = toolbox.run("desk_shell", r#"{"command":"echo hi"}"#).await;

    assert_eq!(
        result,
        "The shared computer did not answer: Per-bot machines are off here. There is nothing to \
run a command on."
    );
}
