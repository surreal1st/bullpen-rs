//! S8a-02 bite: `browse`/`read_page` resolve their `Cdp` at the CALLING
//! bot's own VM (`desk::cdp_for_bot` -> `vm::desk_for_in` -> `vm::vm_desk`),
//! never a single desk shared by every bot on the roster.
//!
//! Drives `server::runs::RunManager::toolbox_for(...).run("browse", ...)`
//! directly - the exact dispatch arm this ticket rewrote in
//! `tools/mod.rs` - for two bots holding two different VM rows, both
//! `state = 'stopped'` (the reaper-hibernated common case this ticket's own
//! instructions call out, `b010c30`), against a real `HttpCdp` with nothing
//! listening on either resolved port.
//!
//! **The observable that tells the two worlds apart:** nothing is
//! listening on either port, so `HttpCdp::create_window`'s first request
//! fails, and `reqwest::Error`'s own `Display` names the exact host:port it
//! tried. Bot A's result and bot B's result therefore differ by PORT ALONE:
//! same tool, same args, two different machines. This is a stronger
//! observable than a fake `Cdp` would give: it is the REAL transport,
//! reaching the REAL resolved endpoint, with no seam removed between the
//! dispatch arm and the socket.
//!
//! Guard-present world: this file's own dispatch arm
//! (`crate::desk::cdp_for_bot`, `tools/mod.rs`) reaches two DIFFERENT
//! ports, one per bot. Guard-removed world: the arm reverted to the
//! deleted `desk::build_cdp()`, ONE shared, env-gated desk for every bot,
//! so both results collapse to the SAME string. Restoring `cdp_for_bot` is
//! what tells them apart again - see this ticket's Result for the literal
//! red/green.

use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use model::ladder::Trigger;
use model::{EventStream, ModelPort, ModelRequest};
use server::runs::RunManager;
use server::sandbox;
use server::vm::DockerRun;
use store::Db;
use store::vms::{DockerResult, VmConfig};

/// `browse`/`read_page` never call the model - this run manager's `port`
/// exists only because `RunManager::with_sandbox_and_vm` requires one.
struct NeverCalledPort;

impl ModelPort for NeverCalledPort {
    fn stream(&self, _request: ModelRequest) -> EventStream {
        panic!("browse must never reach the model port");
    }
}

/// Answers every `docker inspect` with "exited false" (exists, not
/// running - the reaper's own `hibernate_idle`/`start_vm_reaper` leave a
/// row in exactly this shape, `b010c30`) and every `docker start` with ok -
/// `ensure_vm_in`'s wake-a-stopped-machine branch, never the
/// create-a-new-one branch. Records nothing: which port a call reaches is
/// already provable from the row's own `cdp_port`, so a call log would add
/// nothing this test checks.
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
        cdp_base: 9400,
        web_base: 6400,
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

#[tokio::test]
async fn browse_reaches_two_different_bots_own_machines() {
    let db = Db::open(":memory:").expect("open :memory: db");
    server::desk::ensure_desk_tables(&db).expect("ensure desk tables");
    insert_stopped_vm(&db, "bot-a", "bullpen-vm-bot-a", 9401, 6401);
    insert_stopped_vm(&db, "bot-b", "bullpen-vm-bot-b", 9402, 6402);
    let db = Arc::new(Mutex::new(db));

    let docker: Arc<dyn DockerRun> = Arc::new(StoppedThenWakes);
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
        .run("browse", r#"{"url":"https://example.com/"}"#)
        .await;

    let toolbox_b = manager.toolbox_for("bot-b", Trigger::Chat, false, "test/model", None);
    let (result_b, _) = toolbox_b
        .run("browse", r#"{"url":"https://example.com/"}"#)
        .await;

    assert_ne!(
        result_a, result_b,
        "two bots' own machines must not collide: {result_a:?} / {result_b:?}"
    );
    assert!(
        result_a.contains("9401"),
        "expected bot-a's OWN port 9401 in its result, got {result_a:?}"
    );
    assert!(
        result_b.contains("9402"),
        "expected bot-b's OWN port 9402 in its result, got {result_b:?}"
    );
    assert!(
        !result_a.contains("9402") && !result_b.contains("9401"),
        "a bot's result must never name the OTHER bot's port: {result_a:?} / {result_b:?}"
    );
}

/// `vm_enabled = false` (Decision 1): refused with `UnavailableCdp` BEFORE
/// `vm::desk_for_in` is ever reached - the fake docker records nothing.
#[tokio::test]
async fn browse_refuses_before_touching_docker_when_vms_are_disabled() {
    let db = Db::open(":memory:").expect("open :memory: db");
    server::desk::ensure_desk_tables(&db).expect("ensure desk tables");
    insert_stopped_vm(&db, "bot-a", "bullpen-vm-bot-a", 9401, 6401);
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
        .run("browse", r#"{"url":"https://example.com/"}"#)
        .await;

    // `UnavailableCdp::create_window` returns the reason as an `Err`, which
    // `run_browse` wraps the same way it wraps every other `Cdp` failure
    // (`browse_tools.rs`'s own `browse_turns_a_cdp_failure_into_a_sentence`
    // - "The shared computer did not answer: {err}").
    assert_eq!(
        result,
        "The shared computer did not answer: Per-bot machines are off here. There is nothing to \
browse with."
    );
}
