mod common;

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use common::{own_conversation, seed_bot};
use model::ladder::Trigger;
use model::{EventStream, ModelEvent, ModelMessage, ModelPort, ModelRequest, ToolCall};
use server::observations::ObservationClock;
use server::runs::{RunEvent, RunManager, StartOptions};
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

fn store_observation(manager: &Arc<RunManager>, run_id: &str, id: &str, generation: u64) {
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
            run_id,
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
    store_observation(&manager, "run-1", observation_id, 0);

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
    store_observation(&manager, "run-1", next_id, 1);
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
        store_observation(&manager, "run-1", id, 0);
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
        store_observation(&manager, "run-1", id, 0);
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

struct ApprovalPort {
    turn: AtomicUsize,
    release_first: Arc<tokio::sync::Semaphore>,
    observation_id: String,
}

impl ModelPort for ApprovalPort {
    fn stream(&self, _request: ModelRequest) -> EventStream {
        if self.turn.fetch_add(1, Ordering::SeqCst) == 0 {
            let release = Arc::clone(&self.release_first);
            let observation_id = self.observation_id.clone();
            return Box::pin(async_stream::stream! {
                let permit = release.acquire().await.expect("approval model gate open");
                permit.forget();
                yield ModelEvent::ToolCalls {
                    calls: vec![ToolCall {
                        id: "coordinate-call".into(),
                        name: "desk_act".into(),
                        arguments: serde_json::json!({
                            "observation_id": observation_id,
                            "actions": [{"kind": "click", "x": 1, "y": 1}]
                        })
                        .to_string(),
                    }],
                    usage: None,
                };
            });
        }
        Box::pin(futures::stream::iter(vec![ModelEvent::Done {
            model: "provider/actual".into(),
            usage: None,
            finish_reason: None,
        }]))
    }
}

struct TestClock(Mutex<tokio::time::Instant>);

impl TestClock {
    fn new() -> Self {
        Self(Mutex::new(tokio::time::Instant::now()))
    }

    fn advance(&self, by: Duration) {
        let mut now = self.0.lock().unwrap();
        *now += by;
    }
}

impl ObservationClock for TestClock {
    fn now(&self) -> tokio::time::Instant {
        *self.0.lock().unwrap()
    }
}

fn vision_catalog() -> Arc<dyn model::Catalog> {
    let json = serde_json::json!([{
        "id": "test/model",
        "name": "test/model",
        "inPerM": 1.0,
        "outPerM": 1.0,
        "contextLength": 32000,
        "supportsTools": true,
        "supportsImages": true,
        "supportsReasoning": false,
        "providerCount": 1,
        "supportsCaching": false
    }]);
    Arc::new(model::FixtureCatalog::from_json(&json.to_string()).unwrap())
}

enum ApprovalStaleness {
    Fresh,
    Mutated,
    GenerationAdvancedAgain,
    Expired,
}

async fn coordinate_approval_case(staleness: ApprovalStaleness) {
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
        model::routing::set_routing_settings(&db, Some(false), None).unwrap();
        server::judge::set_judge_enabled(&db, false).unwrap();
        let mut permissions = server::permissions::get_permissions(&db, "arthur").unwrap();
        permissions.insert("desk_act".into(), server::permissions::Decision::Ask);
        server::permissions::set_permissions(&db, "arthur", &permissions).unwrap();
    }
    let observation_id = "00000000-0000-4000-8000-000000000077";
    let release_first = Arc::new(tokio::sync::Semaphore::new(0));
    let port = Arc::new(ApprovalPort {
        turn: AtomicUsize::new(0),
        release_first: Arc::clone(&release_first),
        observation_id: observation_id.into(),
    });
    let docker = Arc::new(DesktopDocker::running());
    let clock = Arc::new(TestClock::new());
    let observations = Arc::new(server::observations::ObservationRegistry::with_clock(
        Arc::clone(&clock) as Arc<dyn ObservationClock>,
    ));
    let desktop_states = Arc::new(server::observations::DesktopStateRegistry::new());
    let manager = Arc::new(RunManager::with_shared_desktop_state(
        Arc::clone(&db),
        port as Arc<dyn ModelPort>,
        sandbox::default_sandbox(),
        sandbox::default_job_sandbox(),
        Arc::clone(&docker) as Arc<dyn DockerRun>,
        Arc::new(config()),
        true,
        vision_catalog(),
        Arc::clone(&observations),
        Arc::clone(&desktop_states),
    ));
    let run_id = manager.start(StartOptions {
        bot_id: "arthur".into(),
        conversation_id,
        model: "test/model".into(),
        messages: vec![ModelMessage::user("click it")],
        trigger: Trigger::Chat,
        room: false,
    });
    store_observation(&manager, &run_id, observation_id, 0);
    let mut events = manager.subscribe(&run_id);
    release_first.add_permits(1);
    let approval_id = tokio::time::timeout(Duration::from_secs(2), async {
        loop {
            if let Some(RunEvent::ApprovalNeeded { approval_id, .. }) = events.recv().await {
                break approval_id;
            }
        }
    })
    .await
    .expect("coordinate approval requested");

    match staleness {
        ApprovalStaleness::Fresh => {}
        ApprovalStaleness::Mutated => {
            let desktop = desktop_states.for_bot("arthur");
            let mut state = desktop.lock().await;
            observations.record_desktop_mutation("arthur", &mut state);
        }
        ApprovalStaleness::GenerationAdvancedAgain => {
            let desktop = desktop_states.for_bot("arthur");
            desktop.lock().await.advance();
            assert!(
                observations.metadata(&run_id).is_some(),
                "isolate generation mismatch from token invalidation"
            );
        }
        ApprovalStaleness::Expired => clock.advance(Duration::from_secs(31)),
    }

    assert!(manager.decide_approval(&approval_id, true, None).await);
    let remaining = tokio::time::timeout(Duration::from_secs(2), common::drain(events))
        .await
        .expect("approved coordinate run settled");
    let stale = !matches!(staleness, ApprovalStaleness::Fresh);
    assert_eq!(docker.exec_count(), usize::from(!stale));
    if stale {
        assert!(remaining.iter().any(|event| matches!(
            event,
            RunEvent::ToolResult { result, .. } if result.contains("Capture again")
        )));
    }
    assert!(
        !manager.decide_approval(&approval_id, true, None).await,
        "an approval can be consumed only once"
    );
}

#[tokio::test]
async fn coordinate_approval_executes_once_while_fresh() {
    coordinate_approval_case(ApprovalStaleness::Fresh).await;
}

#[tokio::test]
async fn coordinate_approval_refuses_mutated_generation_changes_expiry_and_reuse() {
    coordinate_approval_case(ApprovalStaleness::Mutated).await;
    coordinate_approval_case(ApprovalStaleness::GenerationAdvancedAgain).await;
    coordinate_approval_case(ApprovalStaleness::Expired).await;
}

struct StoppedThenWakesDocker {
    calls: Mutex<Vec<Vec<String>>>,
}

impl StoppedThenWakesDocker {
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
impl DockerRun for StoppedThenWakesDocker {
    async fn call(&self, args: &[&str], _timeout_ms: u64) -> DockerResult {
        self.calls
            .lock()
            .unwrap()
            .push(args.iter().map(|arg| arg.to_string()).collect());
        match args.first() {
            Some(&"inspect") => DockerResult {
                ok: true,
                stdout: "exited false".into(),
                stderr: String::new(),
            },
            Some(&"start") => DockerResult {
                ok: true,
                stdout: String::new(),
                stderr: String::new(),
            },
            Some(&"exec") => DockerResult {
                ok: true,
                stdout: "coordinate action must not execute after wake".into(),
                stderr: String::new(),
            },
            _ => DockerResult {
                ok: true,
                stdout: String::new(),
                stderr: String::new(),
            },
        }
    }
}

#[tokio::test]
async fn coordinate_action_refuses_when_resolving_its_desk_wakes_the_vm() {
    let db = Arc::new(Mutex::new(Db::open(":memory:").expect("open db")));
    seed_bot(&db, "arthur", "Arthur");
    let conversation_id = own_conversation(&db, "arthur");
    {
        let db = db.lock().unwrap();
        server::desk::ensure_desk_tables(&db).unwrap();
        db.conn()
            .execute(
                "INSERT INTO vms (bot_id, container, cdp_port, web_port, state, last_used_at)
                 VALUES ('arthur', 'bullpen-vm-arthur', 9501, 6501, 'stopped', '2026-09-18T12:00:00Z')",
                [],
            )
            .unwrap();
        db.conn()
            .execute(
                "INSERT INTO runs (id, bot_id, conversation_id, trigger, status, model, messages, text, created_at, updated_at)
                 VALUES ('run-wake', 'arthur', ?1, 'chat', 'running', 'test/model', '[]', '', '2026-09-18T12:00:00Z', '2026-09-18T12:00:00Z')",
                [conversation_id],
            )
            .unwrap();
    }
    let docker = Arc::new(StoppedThenWakesDocker {
        calls: Mutex::new(Vec::new()),
    });
    let manager = Arc::new(RunManager::with_sandbox_and_vm(
        Arc::clone(&db),
        Arc::new(NeverCalledPort) as Arc<dyn ModelPort>,
        sandbox::default_sandbox(),
        Arc::clone(&docker) as Arc<dyn DockerRun>,
        Arc::new(config()),
        true,
    ));
    let observation_id = "00000000-0000-4000-8000-000000000088";
    store_observation(&manager, "run-wake", observation_id, 0);
    let args = format!(
        r#"{{"observation_id":"{observation_id}","actions":[{{"kind":"click","x":1,"y":1}}]}}"#
    );

    let result = toolbox(
        &manager,
        RunExecutionContext::ModelTurn {
            run_id: "run-wake".into(),
        },
    )
    .run("desk_act", &args)
    .await
    .text;

    assert!(
        result.contains("desktop changed") || result.contains("Capture again"),
        "a wake after coordinate validation must refuse and require a fresh capture: {result}"
    );
    assert_eq!(
        docker.exec_count(),
        0,
        "coordinates captured before a VM wake must never execute afterward"
    );
}

struct DonePort;

impl ModelPort for DonePort {
    fn stream(&self, _request: ModelRequest) -> EventStream {
        Box::pin(futures::stream::iter(vec![ModelEvent::Done {
            model: "provider/actual".into(),
            usage: None,
            finish_reason: None,
        }]))
    }
}

#[tokio::test]
async fn persisted_coordinate_approval_cannot_authorize_a_recapture_after_manager_restart() {
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
        model::routing::set_routing_settings(&db, Some(false), None).unwrap();
        server::judge::set_judge_enabled(&db, false).unwrap();
        let mut permissions = server::permissions::get_permissions(&db, "arthur").unwrap();
        permissions.insert("desk_act".into(), server::permissions::Decision::Ask);
        server::permissions::set_permissions(&db, "arthur", &permissions).unwrap();
    }

    let old_observation = "00000000-0000-4000-8000-000000000070";
    let release_first = Arc::new(tokio::sync::Semaphore::new(0));
    let first_manager = Arc::new(RunManager::with_shared_desktop_state(
        Arc::clone(&db),
        Arc::new(ApprovalPort {
            turn: AtomicUsize::new(0),
            release_first: Arc::clone(&release_first),
            observation_id: old_observation.into(),
        }),
        sandbox::default_sandbox(),
        sandbox::default_job_sandbox(),
        Arc::new(DesktopDocker::running()),
        Arc::new(config()),
        true,
        vision_catalog(),
        Arc::new(server::observations::ObservationRegistry::new()),
        Arc::new(server::observations::DesktopStateRegistry::new()),
    ));
    let run_id = first_manager.start(StartOptions {
        bot_id: "arthur".into(),
        conversation_id,
        model: "test/model".into(),
        messages: vec![ModelMessage::user("click it")],
        trigger: Trigger::Chat,
        room: false,
    });
    store_observation(&first_manager, &run_id, old_observation, 0);
    let mut first_events = first_manager.subscribe(&run_id);
    release_first.add_permits(1);
    let approval_id = tokio::time::timeout(Duration::from_secs(2), async {
        loop {
            if let Some(RunEvent::ApprovalNeeded { approval_id, .. }) = first_events.recv().await {
                break approval_id;
            }
        }
    })
    .await
    .expect("approval persisted before restart");
    drop(first_events);
    drop(first_manager);

    let docker = Arc::new(DesktopDocker::running());
    let restarted = Arc::new(RunManager::with_shared_desktop_state(
        Arc::clone(&db),
        Arc::new(DonePort),
        sandbox::default_sandbox(),
        sandbox::default_job_sandbox(),
        Arc::clone(&docker) as Arc<dyn DockerRun>,
        Arc::new(config()),
        true,
        vision_catalog(),
        Arc::new(server::observations::ObservationRegistry::new()),
        Arc::new(server::observations::DesktopStateRegistry::new()),
    ));
    let recaptured = "00000000-0000-4000-8000-000000000071";
    store_observation(&restarted, &run_id, recaptured, 0);
    let events = restarted.subscribe(&run_id);

    assert!(restarted.decide_approval(&approval_id, true, None).await);
    let seen = tokio::time::timeout(Duration::from_secs(2), common::drain(events))
        .await
        .expect("restarted manager settled old approval");
    assert_eq!(
        docker.exec_count(),
        0,
        "persisted approval for an old observation authorized a recaptured screen"
    );
    assert!(seen.iter().any(|event| matches!(
        event,
        RunEvent::ToolResult { result, .. } if result.contains("latest observation")
    )));
    assert!(
        restarted.observation_registry().metadata(&run_id).is_none(),
        "terminal run cleanup releases the unconsumed recapture"
    );
}
