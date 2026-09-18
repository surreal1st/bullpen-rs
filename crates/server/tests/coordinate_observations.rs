mod common;

use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use common::{own_conversation, seed_bot};
use model::ladder::Trigger;
use model::{EventStream, ModelPort, ModelRequest};
use server::runs::RunManager;
use server::sandbox;
use server::tools::RunExecutionContext;
use server::vm::{CapturedFrame, DockerRun};
use store::Db;
use store::vms::{DockerResult, VmConfig};

struct NeverCalledPort;

impl ModelPort for NeverCalledPort {
    fn stream(&self, _request: ModelRequest) -> EventStream {
        panic!("coordinate tool tests must not reach the model port");
    }
}

struct DesktopDocker {
    calls: Mutex<Vec<Vec<String>>>,
    exec_started: tokio::sync::Notify,
    release_exec: tokio::sync::Semaphore,
    hold_exec: bool,
}

impl Default for DesktopDocker {
    fn default() -> Self {
        Self {
            calls: Mutex::new(Vec::new()),
            exec_started: tokio::sync::Notify::new(),
            release_exec: tokio::sync::Semaphore::new(0),
            hold_exec: false,
        }
    }
}

impl DesktopDocker {
    fn running() -> Self {
        Self {
            release_exec: tokio::sync::Semaphore::new(1),
            ..Self::default()
        }
    }

    fn held() -> Self {
        Self {
            hold_exec: true,
            ..Self::default()
        }
    }

    fn exec_count(&self) -> usize {
        self.calls
            .lock()
            .unwrap()
            .iter()
            .filter(|call| call.first().map(String::as_str) == Some("exec"))
            .count()
    }
}

#[async_trait]
impl DockerRun for DesktopDocker {
    async fn call(&self, args: &[&str], _timeout_ms: u64) -> DockerResult {
        self.calls
            .lock()
            .unwrap()
            .push(args.iter().map(|arg| arg.to_string()).collect());
        match args.first() {
            Some(&"inspect") => DockerResult {
                ok: true,
                stdout: "running true".into(),
                stderr: String::new(),
            },
            Some(&"exec") => {
                self.exec_started.notify_one();
                if self.hold_exec {
                    self.release_exec
                        .acquire()
                        .await
                        .expect("release gate open")
                        .forget();
                }
                DockerResult {
                    ok: true,
                    stdout: "action complete".into(),
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

fn config() -> VmConfig {
    VmConfig {
        image: "test-image".into(),
        docker_host: "unix:///test.sock".into(),
        cdp_base: 9500,
        web_base: 6500,
        slots: 8,
        idle_ms: 1_800_000,
        init_dir: "/vm-init".into(),
        memory: "3g".into(),
        cpus: "1.5".into(),
        shm_size: "1g".into(),
        timezone: "America/New_York".into(),
        puid: "1004".into(),
        pgid: "1004".into(),
    }
}

fn setup(docker: Arc<DesktopDocker>) -> (Arc<RunManager>, Arc<Mutex<Db>>) {
    let db = Arc::new(Mutex::new(Db::open(":memory:").expect("open db")));
    seed_bot(&db, "arthur", "Arthur");
    let conversation_id = own_conversation(&db, "arthur");
    {
        let db = db.lock().unwrap();
        server::desk::ensure_desk_tables(&db).unwrap();
        db.conn()
            .execute(
                "INSERT INTO vms (bot_id, container, cdp_port, web_port, state, last_used_at)
                 VALUES ('arthur', 'bullpen-vm-arthur', 9501, 6501, 'running', '2026-09-18T12:00:00Z')",
                [],
            )
            .unwrap();
        db.conn()
            .execute(
                "INSERT INTO runs (id, bot_id, conversation_id, trigger, status, model, messages, text, created_at, updated_at)
                 VALUES ('run-1', 'arthur', ?1, 'chat', 'running', 'test/model', '[]', '', '2026-09-18T12:00:00Z', '2026-09-18T12:00:00Z')",
                [conversation_id],
            )
            .unwrap();
    }
    let docker_port: Arc<dyn DockerRun> = docker;
    let manager = Arc::new(RunManager::with_sandbox_and_vm(
        Arc::clone(&db),
        Arc::new(NeverCalledPort) as Arc<dyn ModelPort>,
        sandbox::default_sandbox(),
        docker_port,
        Arc::new(config()),
        true,
    ));
    (manager, db)
}

fn toolbox(manager: &Arc<RunManager>, context: RunExecutionContext) -> server::tools::ToolBox {
    manager.toolbox_for_context("arthur", Trigger::Chat, false, "test/model", None, context)
}

fn store_observation(manager: &Arc<RunManager>, id: &str, generation: u64) {
    let observation = manager
        .observation_admission()
        .try_begin_capture()
        .expect("capture reservation")
        .retain(
            CapturedFrame {
                png: vec![1, 2, 3],
                width: 200,
                height: 100,
            },
            "run-1",
            "arthur",
            id,
            "2026-09-18T12:00:00Z",
            generation,
        );
    manager.observation_registry().store(&observation);
}

#[tokio::test]
async fn coordinate_action_uses_current_owned_observation_once_and_native_bounds() {
    let docker = Arc::new(DesktopDocker::running());
    let (manager, _) = setup(Arc::clone(&docker));
    let observation_id = "00000000-0000-4000-8000-000000000001";
    store_observation(&manager, observation_id, 0);

    let args = format!(
        r#"{{"observation_id":"{observation_id}","actions":[{{"kind":"click","x":100.0,"y":99.0}}]}}"#
    );
    let result = toolbox(
        &manager,
        RunExecutionContext::ModelTurn {
            run_id: "run-1".into(),
        },
    )
    .run("desk_act", &args)
    .await
    .text;
    assert!(result.contains("action complete"), "{result}");
    assert_eq!(docker.exec_count(), 1);
    assert!(manager.observation_registry().metadata("run-1").is_none());
    assert_eq!(
        manager
            .desktop_state_registry()
            .for_bot("arthur")
            .lock()
            .await
            .generation(),
        1
    );

    let reused = toolbox(
        &manager,
        RunExecutionContext::ModelTurn {
            run_id: "run-1".into(),
        },
    )
    .run("desk_act", &args)
    .await
    .text;
    assert!(reused.contains("No current screen observation"), "{reused}");
    assert_eq!(docker.exec_count(), 1);

    let next_id = "00000000-0000-4000-8000-000000000002";
    store_observation(&manager, next_id, 1);
    let outside =
        format!(r#"{{"observation_id":"{next_id}","actions":[{{"kind":"move","x":200,"y":0}}]}}"#);
    let refused = toolbox(
        &manager,
        RunExecutionContext::ModelTurn {
            run_id: "run-1".into(),
        },
    )
    .run("desk_act", &outside)
    .await
    .text;
    assert!(refused.contains("inside the observed 200x100"), "{refused}");
    assert!(manager.observation_registry().metadata("run-1").is_none());
    assert_eq!(docker.exec_count(), 1);
}

#[tokio::test]
async fn coordinate_actions_fail_closed_without_a_model_run() {
    let docker = Arc::new(DesktopDocker::running());
    let (manager, _) = setup(Arc::clone(&docker));
    let args = r#"{"observation_id":"00000000-0000-4000-8000-000000000001","actions":[{"kind":"click","x":1,"y":1}]}"#;

    let unbound = toolbox(&manager, RunExecutionContext::Unbound)
        .run("desk_act", args)
        .await
        .text;
    assert!(unbound.contains("active persisted run"), "{unbound}");
    let direct = toolbox(&manager, RunExecutionContext::DirectRoutine)
        .run("desk_act", args)
        .await
        .text;
    assert!(direct.contains("direct tool routine"), "{direct}");
    let missing = toolbox(
        &manager,
        RunExecutionContext::ModelTurn {
            run_id: "missing-run".into(),
        },
    )
    .run("desk_act", args)
    .await
    .text;
    assert!(missing.contains("active persisted run"), "{missing}");
    assert_eq!(docker.exec_count(), 0);
}

#[tokio::test]
async fn cancelled_caller_does_not_release_desktop_lock_before_remote_action_finishes() {
    let docker = Arc::new(DesktopDocker::held());
    let (manager, _) = setup(Arc::clone(&docker));
    let started = docker.exec_started.notified();
    tokio::pin!(started);
    let box_ = toolbox(&manager, RunExecutionContext::Unbound);
    let caller = tokio::spawn(async move {
        box_.run("desk_act", r#"{"actions":[{"kind":"key","keys":"a"}]}"#)
            .await
    });
    tokio::time::timeout(std::time::Duration::from_secs(2), &mut started)
        .await
        .expect("remote action started");
    caller.abort();

    let state = manager.desktop_state_registry().for_bot("arthur");
    assert!(
        tokio::time::timeout(
            std::time::Duration::from_millis(50),
            state.clone().lock_owned()
        )
        .await
        .is_err(),
        "caller cancellation released the desktop lock while docker was still running"
    );
    docker.release_exec.add_permits(1);
    let guard = tokio::time::timeout(std::time::Duration::from_secs(2), state.lock_owned())
        .await
        .expect("worker released lock after remote action");
    assert_eq!(guard.generation(), 1);
}

#[tokio::test]
async fn queued_coordinate_action_rechecks_run_owner_and_status_after_lock_wait() {
    for (update, expected) in [
        (
            "UPDATE runs SET status = 'done' WHERE id = 'run-1'",
            "running run",
        ),
        (
            "UPDATE runs SET bot_id = 'other' WHERE id = 'run-1'",
            "another bot",
        ),
    ] {
        let docker = Arc::new(DesktopDocker::running());
        let (manager, db) = setup(Arc::clone(&docker));
        seed_bot(&db, "other", "Other");
        let id = "00000000-0000-4000-8000-000000000003";
        store_observation(&manager, id, 0);
        let desktop = manager.desktop_state_registry().for_bot("arthur");
        let held = desktop.lock().await;
        let box_ = toolbox(
            &manager,
            RunExecutionContext::ModelTurn {
                run_id: "run-1".into(),
            },
        );
        let args =
            format!(r#"{{"observation_id":"{id}","actions":[{{"kind":"click","x":1,"y":1}}]}}"#);
        let mut action = Box::pin(box_.run("desk_act", &args));
        assert!(futures::poll!(action.as_mut()).is_pending());
        assert_eq!(db.lock().unwrap().conn().execute(update, []).unwrap(), 1);
        drop(held);
        let result = tokio::time::timeout(std::time::Duration::from_secs(2), action)
            .await
            .expect("queued action settled")
            .text;
        assert!(result.contains(expected), "{result}");
        assert_eq!(
            docker.exec_count(),
            0,
            "queued action ran after ownership changed"
        );
    }
}

#[tokio::test]
async fn floating_point_coordinates_still_obey_native_bounds() {
    for point in [r#""x":200.0,"y":0.0"#, r#""x":0.0,"y":100.0"#] {
        let docker = Arc::new(DesktopDocker::running());
        let (manager, _) = setup(Arc::clone(&docker));
        let id = "00000000-0000-4000-8000-000000000004";
        store_observation(&manager, id, 0);
        let args = format!(r#"{{"observation_id":"{id}","actions":[{{"kind":"move",{point}}}]}}"#);
        let result = toolbox(
            &manager,
            RunExecutionContext::ModelTurn {
                run_id: "run-1".into(),
            },
        )
        .run("desk_act", &args)
        .await
        .text;
        assert!(result.contains("inside the observed 200x100"), "{result}");
        assert_eq!(docker.exec_count(), 0);
        assert!(manager.observation_registry().metadata("run-1").is_none());
    }
}
