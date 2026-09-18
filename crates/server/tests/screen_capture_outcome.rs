mod common;

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use common::{ScriptedPort, as_port, own_conversation, seed_bot};
use model::ladder::Trigger;
use model::{EventStream, ModelEvent, ModelMessage, ModelPort, ModelRequest, ToolCall};
use server::observations::{
    CaptureWorkerLease, CapturedObservationFrame, CounterClaim, RunImageCounters,
    read_run_image_counters,
};
use server::runs::{RunEvent, RunManager, StartOptions};
use server::sandbox;
use server::tools::RunExecutionContext;
use server::vm::{CapturedFrame, DockerRun, FrameCapture};
use store::Db;
use store::vms::{DockerResult, VmConfig};

#[derive(Default)]
struct RunningDocker {
    calls: AtomicUsize,
}

#[async_trait::async_trait]
impl DockerRun for RunningDocker {
    async fn call(&self, _args: &[&str], _timeout_ms: u64) -> DockerResult {
        self.calls.fetch_add(1, Ordering::SeqCst);
        DockerResult {
            ok: true,
            stdout: "running true".into(),
            stderr: String::new(),
        }
    }
}

struct CountingCapture {
    calls: AtomicUsize,
    containers: Mutex<Vec<String>>,
    succeeds: bool,
}

impl CountingCapture {
    fn new(succeeds: bool) -> Self {
        Self {
            calls: AtomicUsize::new(0),
            containers: Mutex::new(Vec::new()),
            succeeds,
        }
    }
}

#[async_trait::async_trait]
impl FrameCapture for CountingCapture {
    async fn capture(&self, container: &str, _cfg: &VmConfig) -> Option<CapturedFrame> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        self.containers.lock().unwrap().push(container.to_string());
        self.succeeds.then(|| CapturedFrame {
            png: vec![0x89, 0x50, 0x4e, 0x47, 7, 8, 9],
            width: 640,
            height: 480,
        })
    }
}

struct DetachedCleanupCapture {
    started: AtomicUsize,
    cleanup: Arc<tokio::sync::Semaphore>,
}

#[async_trait::async_trait]
impl FrameCapture for DetachedCleanupCapture {
    async fn capture(&self, _container: &str, _cfg: &VmConfig) -> Option<CapturedFrame> {
        panic!("observation capture must use the lease-aware seam")
    }

    async fn capture_observation(
        &self,
        _container: &str,
        _cfg: &VmConfig,
        lease: CaptureWorkerLease,
    ) -> Option<CapturedObservationFrame> {
        let cleanup = Arc::clone(&self.cleanup);
        let (tx, rx) = tokio::sync::oneshot::channel();
        self.started.fetch_add(1, Ordering::SeqCst);
        tokio::spawn(async move {
            let permit = cleanup.acquire().await.expect("cleanup gate open");
            permit.forget();
            drop(lease);
            let _ = tx.send(None);
        });
        rx.await.ok().flatten()
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

fn catalog(id: &str, images: bool, tools: bool) -> Arc<dyn model::Catalog> {
    let json = serde_json::json!([{
        "id": id,
        "name": id,
        "inPerM": 1.0,
        "outPerM": 1.0,
        "contextLength": 32000,
        "supportsTools": tools,
        "supportsImages": images,
        "supportsReasoning": false,
        "providerCount": 2,
        "supportsCaching": false
    }]);
    Arc::new(model::FixtureCatalog::from_json(&json.to_string()).unwrap())
}

fn insert_run(db: &Arc<Mutex<Db>>, id: &str, bot_id: &str, model: &str) {
    let bot_exists: bool = db
        .lock()
        .unwrap()
        .conn()
        .query_row(
            "SELECT EXISTS(SELECT 1 FROM bots WHERE id = ?1)",
            [bot_id],
            |row| row.get(0),
        )
        .unwrap();
    if !bot_exists {
        seed_bot(db, bot_id, bot_id);
    }
    let conversation_id = own_conversation(db, bot_id);
    db.lock()
        .unwrap()
        .conn()
        .execute(
            "INSERT INTO runs (id, bot_id, conversation_id, trigger, status, model, messages, text, created_at, updated_at) VALUES (?1, ?2, ?3, 'chat', 'running', ?4, '[]', '', '2026-09-18T12:00:00Z', '2026-09-18T12:00:00Z')",
            rusqlite::params![id, bot_id, conversation_id, model],
        )
        .unwrap();
}

fn insert_vm(db: &Arc<Mutex<Db>>, bot_id: &str) {
    db.lock()
        .unwrap()
        .conn()
        .execute(
            "INSERT INTO vms (bot_id, container, cdp_port, web_port, state, last_used_at) VALUES (?1, ?2, 9501, 6501, 'running', '2026-09-18T12:00:00Z')",
            rusqlite::params![bot_id, format!("bullpen-vm-{bot_id}")],
        )
        .unwrap();
}

fn manager(
    db: Arc<Mutex<Db>>,
    model_catalog: Arc<dyn model::Catalog>,
    docker: Arc<dyn DockerRun>,
    capture: Arc<dyn FrameCapture>,
    vm_enabled: bool,
) -> Arc<RunManager> {
    Arc::new(RunManager::with_screen_capture(
        db,
        as_port(ScriptedPort::new(vec![])),
        sandbox::default_sandbox(),
        docker,
        Arc::new(config()),
        vm_enabled,
        model_catalog,
        capture,
    ))
}

fn toolbox(
    manager: &Arc<RunManager>,
    bot_id: &str,
    run_id: &str,
    caller_model: &str,
) -> server::tools::ToolBox {
    manager.toolbox_for_context(
        bot_id,
        Trigger::Chat,
        false,
        caller_model,
        None,
        RunExecutionContext::ModelTurn {
            run_id: run_id.to_string(),
        },
    )
}

struct EscalationHoldPort {
    turn: AtomicUsize,
    second_started: Arc<tokio::sync::Semaphore>,
    release_second: Arc<tokio::sync::Semaphore>,
}

impl ModelPort for EscalationHoldPort {
    fn stream(&self, _request: ModelRequest) -> EventStream {
        if self.turn.fetch_add(1, Ordering::SeqCst) == 0 {
            return Box::pin(futures::stream::iter(vec![ModelEvent::ToolCalls {
                calls: vec![ToolCall {
                    id: "escalate-1".into(),
                    name: "escalate".into(),
                    arguments: serde_json::json!({
                        "reason": "need the code model",
                        "kind": "code"
                    })
                    .to_string(),
                }],
                usage: None,
            }]));
        }
        let started = Arc::clone(&self.second_started);
        let release = Arc::clone(&self.release_second);
        Box::pin(async_stream::stream! {
            started.add_permits(1);
            let permit = release.acquire().await.expect("release gate open");
            permit.forget();
            yield ModelEvent::Done {
                model: "provider/actual".into(),
                usage: None,
                finish_reason: None,
            };
        })
    }
}

#[tokio::test]
async fn escalation_persists_effective_model_before_the_next_request_finishes() {
    let db = Arc::new(Mutex::new(Db::open(":memory:").unwrap()));
    seed_bot(&db, "arthur", "Arthur");
    let conversation_id = own_conversation(&db, "arthur");
    model::routing::set_routing_settings(&db.lock().unwrap(), Some(false), None).unwrap();
    let second_started = Arc::new(tokio::sync::Semaphore::new(0));
    let release_second = Arc::new(tokio::sync::Semaphore::new(0));
    let port = Arc::new(EscalationHoldPort {
        turn: AtomicUsize::new(0),
        second_started: Arc::clone(&second_started),
        release_second: Arc::clone(&release_second),
    });
    let manager = Arc::new(RunManager::new(Arc::clone(&db), port as Arc<dyn ModelPort>));
    let run_id = manager.start(StartOptions {
        bot_id: "arthur".into(),
        conversation_id,
        model: "test/unconfigured-model".into(),
        messages: vec![ModelMessage::user("fix the build")],
        trigger: Trigger::Chat,
        room: false,
    });
    let permit = tokio::time::timeout(Duration::from_secs(1), second_started.acquire())
        .await
        .expect("second request started")
        .expect("second-start gate open");
    permit.forget();

    let (status, persisted_model): (String, String) = db
        .lock()
        .unwrap()
        .conn()
        .query_row(
            "SELECT status, model FROM runs WHERE id = ?1",
            [&run_id],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .unwrap();
    assert_eq!(status, "running");
    assert_eq!(persisted_model, model::ladder::DEFAULT_TIER1.code);

    let events = manager.subscribe(&run_id);
    release_second.add_permits(1);
    tokio::time::timeout(Duration::from_secs(2), common::drain(events))
        .await
        .expect("run completed after the held request was released");
}

async fn assert_escalation_persistence_failure(trigger_action: &str) {
    let db = Arc::new(Mutex::new(Db::open(":memory:").unwrap()));
    seed_bot(&db, "arthur", "Arthur");
    let conversation_id = own_conversation(&db, "arthur");
    model::routing::set_routing_settings(&db.lock().unwrap(), Some(false), None).unwrap();
    let climbed = model::ladder::DEFAULT_TIER1
        .code
        .replace(char::from(39), "''");
    db.lock()
        .unwrap()
        .conn()
        .execute_batch(&format!(
            "CREATE TRIGGER reject_climbed_model BEFORE UPDATE OF model ON runs WHEN NEW.model = '{climbed}' BEGIN {trigger_action} END;"
        ))
        .unwrap();
    let port = Arc::new(EscalationHoldPort {
        turn: AtomicUsize::new(0),
        second_started: Arc::new(tokio::sync::Semaphore::new(0)),
        release_second: Arc::new(tokio::sync::Semaphore::new(0)),
    });
    let manager = Arc::new(RunManager::new(
        Arc::clone(&db),
        Arc::clone(&port) as Arc<dyn ModelPort>,
    ));
    let run_id = manager.start(StartOptions {
        bot_id: "arthur".into(),
        conversation_id,
        model: "test/unconfigured-model".into(),
        messages: vec![ModelMessage::user("fix the build")],
        trigger: Trigger::Chat,
        room: false,
    });
    let events = tokio::time::timeout(
        Duration::from_secs(2),
        common::drain(manager.subscribe(&run_id)),
    )
    .await
    .expect("persistence refusal stopped the run without issuing another request");
    assert!(events.iter().any(|event| matches!(
        event,
        RunEvent::Error { message, .. }
            if message.contains("Could not persist the selected model")
    )));
    assert_eq!(
        port.turn.load(Ordering::SeqCst),
        1,
        "an unpersisted climbed model must never reach a second request"
    );
    let persisted: String = db
        .lock()
        .unwrap()
        .conn()
        .query_row("SELECT model FROM runs WHERE id = ?1", [&run_id], |row| {
            row.get(0)
        })
        .unwrap();
    assert_eq!(persisted, "test/unconfigured-model");
}

#[tokio::test]
async fn escalation_sql_error_stops_before_a_request_uses_unpersisted_model() {
    assert_escalation_persistence_failure("SELECT RAISE(ABORT, 'fixture reject');").await;
}

#[tokio::test]
async fn escalation_zero_row_update_stops_before_a_request_uses_unpersisted_model() {
    assert_escalation_persistence_failure("SELECT RAISE(IGNORE);").await;
}

#[tokio::test]
async fn success_uses_persisted_effective_model_and_own_vm_without_persisting_png() {
    let db = Arc::new(Mutex::new(Db::open(":memory:").unwrap()));
    insert_run(&db, "run-1", "arthur", "vision/model");
    insert_vm(&db, "arthur");
    let docker = Arc::new(RunningDocker::default());
    let capture = Arc::new(CountingCapture::new(true));
    let manager = manager(
        Arc::clone(&db),
        catalog("vision/model", true, true),
        Arc::clone(&docker) as Arc<dyn DockerRun>,
        Arc::clone(&capture) as Arc<dyn FrameCapture>,
        true,
    );

    let outcome = manager
        .capture_screen_internal(&toolbox(&manager, "arthur", "run-1", "wrong/text-model"))
        .await;
    let observation = outcome.observation.expect("captured observation");
    assert_eq!(observation.metadata().run_id, "run-1");
    assert_eq!(observation.metadata().bot_id, "arthur");
    assert_eq!(
        (observation.metadata().width, observation.metadata().height),
        (640, 480)
    );
    assert_eq!(capture.calls.load(Ordering::SeqCst), 1);
    assert_eq!(
        capture.containers.lock().unwrap().as_slice(),
        ["bullpen-vm-arthur"]
    );
    assert!(manager.observation_registry().has_bytes("run-1"));
    assert!(manager.observation_registry().release_bytes("run-1"));
    assert!(!manager.observation_registry().has_bytes("run-1"));
    assert!(manager.observation_registry().metadata("run-1").is_some());
    assert_eq!(
        manager.observation_admission().snapshot().retained_in_use,
        1
    );
    drop(observation);
    assert_eq!(
        manager.observation_admission().snapshot().retained_in_use,
        0
    );

    let stored: (String, String) = db
        .lock()
        .unwrap()
        .conn()
        .query_row(
            "SELECT messages, text FROM runs WHERE id = 'run-1'",
            [],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .unwrap();
    assert_eq!(stored, ("[]".into(), String::new()));
}

#[tokio::test]
async fn invalid_context_ownership_and_capability_refuse_before_vm_resolution() {
    for case in [
        "unbound",
        "direct",
        "missing",
        "other-bot",
        "unknown",
        "no-images",
        "no-tools",
    ] {
        let db = Arc::new(Mutex::new(Db::open(":memory:").unwrap()));
        insert_run(
            &db,
            "run-1",
            "arthur",
            match case {
                "unknown" => "missing/model",
                _ => "test/model",
            },
        );
        let docker = Arc::new(RunningDocker::default());
        let capture = Arc::new(CountingCapture::new(true));
        let model_catalog = match case {
            "unknown" => catalog("different/model", true, true),
            "no-images" => catalog("test/model", false, true),
            "no-tools" => catalog("test/model", true, false),
            _ => catalog("test/model", true, true),
        };
        let manager = manager(
            Arc::clone(&db),
            model_catalog,
            Arc::clone(&docker) as Arc<dyn DockerRun>,
            Arc::clone(&capture) as Arc<dyn FrameCapture>,
            true,
        );
        let box_ = match case {
            "unbound" => manager.toolbox_for("arthur", Trigger::Chat, false, "ignored", None),
            "direct" => manager.toolbox_for_context(
                "arthur",
                Trigger::Routine,
                false,
                "ignored",
                None,
                RunExecutionContext::DirectRoutine,
            ),
            "missing" => toolbox(&manager, "arthur", "missing-run", "ignored"),
            "other-bot" => toolbox(&manager, "merlin", "run-1", "ignored"),
            _ => toolbox(&manager, "arthur", "run-1", "ignored"),
        };
        let outcome = manager.capture_screen_internal(&box_).await;
        assert!(outcome.observation.is_none(), "{case}");
        assert_eq!(docker.calls.load(Ordering::SeqCst), 0, "{case} woke a VM");
        assert_eq!(capture.calls.load(Ordering::SeqCst), 0, "{case} captured");
        let expected_attempts = u32::from(matches!(case, "unknown" | "no-images" | "no-tools"));
        assert_eq!(
            read_run_image_counters(&db.lock().unwrap(), "run-1").unwrap(),
            Some(RunImageCounters {
                capture_attempts: expected_attempts,
                image_dispatches: 0,
            }),
            "{case} counter ordering"
        );
    }
}

#[tokio::test]
async fn vm_off_stop_counter_cap_and_admission_refuse_before_wake_and_count_valid_attempts() {
    let db = Arc::new(Mutex::new(Db::open(":memory:").unwrap()));
    for id in ["vm-off", "stopped", "capped", "admission"] {
        insert_run(&db, id, "arthur", "test/model");
    }
    let docker = Arc::new(RunningDocker::default());
    let capture = Arc::new(CountingCapture::new(true));
    let vm_off_manager = manager(
        Arc::clone(&db),
        catalog("test/model", true, true),
        Arc::clone(&docker) as Arc<dyn DockerRun>,
        Arc::clone(&capture) as Arc<dyn FrameCapture>,
        false,
    );
    assert!(
        vm_off_manager
            .capture_screen_internal(&toolbox(&vm_off_manager, "arthur", "vm-off", "ignored"))
            .await
            .observation
            .is_none()
    );

    let manager = manager(
        Arc::clone(&db),
        catalog("test/model", true, true),
        Arc::clone(&docker) as Arc<dyn DockerRun>,
        Arc::clone(&capture) as Arc<dyn FrameCapture>,
        true,
    );
    manager.stop("stopped");
    assert!(
        manager
            .capture_screen_internal(&toolbox(&manager, "arthur", "stopped", "ignored"))
            .await
            .observation
            .is_none()
    );

    {
        let db = db.lock().unwrap();
        db.conn()
            .execute(
                "UPDATE runs SET screen_capture_attempts = 8 WHERE id = 'capped'",
                [],
            )
            .unwrap();
    }
    assert!(
        manager
            .capture_screen_internal(&toolbox(&manager, "arthur", "capped", "ignored"))
            .await
            .observation
            .is_none()
    );

    let admission = manager.observation_admission();
    let _held_a = admission.try_begin_capture().unwrap();
    let _held_b = admission.try_begin_capture().unwrap();
    assert!(
        manager
            .capture_screen_internal(&toolbox(&manager, "arthur", "admission", "ignored"))
            .await
            .observation
            .is_none()
    );

    assert_eq!(docker.calls.load(Ordering::SeqCst), 0);
    assert_eq!(capture.calls.load(Ordering::SeqCst), 0);
    let db = db.lock().unwrap();
    for id in ["vm-off", "stopped", "admission"] {
        assert_eq!(
            read_run_image_counters(&db, id).unwrap(),
            Some(RunImageCounters {
                capture_attempts: 1,
                image_dispatches: 0,
            }),
            "{id}"
        );
    }
    assert_eq!(
        server::observations::claim_capture_attempt(&db, "capped").unwrap(),
        CounterClaim::Exhausted(RunImageCounters {
            capture_attempts: 8,
            image_dispatches: 0,
        })
    );
}

#[tokio::test]
async fn capture_failure_releases_every_reservation_and_keeps_no_registry_bytes() {
    let db = Arc::new(Mutex::new(Db::open(":memory:").unwrap()));
    insert_run(&db, "run-1", "arthur", "test/model");
    insert_vm(&db, "arthur");
    let capture = Arc::new(CountingCapture::new(false));
    let manager = manager(
        Arc::clone(&db),
        catalog("test/model", true, true),
        Arc::new(RunningDocker::default()),
        Arc::clone(&capture) as Arc<dyn FrameCapture>,
        true,
    );

    let outcome = manager
        .capture_screen_internal(&toolbox(&manager, "arthur", "run-1", "ignored"))
        .await;
    assert!(outcome.observation.is_none());
    assert_eq!(capture.calls.load(Ordering::SeqCst), 1);
    assert_eq!(
        read_run_image_counters(&db.lock().unwrap(), "run-1").unwrap(),
        Some(RunImageCounters {
            capture_attempts: 1,
            image_dispatches: 0,
        })
    );
    assert_eq!(
        manager
            .observation_admission()
            .snapshot()
            .capture_decode_in_use,
        0
    );
    assert_eq!(
        manager.observation_admission().snapshot().retained_in_use,
        0
    );
    assert!(manager.observation_registry().metadata("run-1").is_none());
}

#[tokio::test]
async fn cancelled_callers_keep_both_capture_budgets_until_detached_cleanup_finishes() {
    let db = Arc::new(Mutex::new(Db::open(":memory:").unwrap()));
    for id in ["run-1", "run-2", "run-3"] {
        insert_run(&db, id, "arthur", "test/model");
    }
    insert_vm(&db, "arthur");
    let capture = Arc::new(DetachedCleanupCapture {
        started: AtomicUsize::new(0),
        cleanup: Arc::new(tokio::sync::Semaphore::new(0)),
    });
    let manager = manager(
        Arc::clone(&db),
        catalog("test/model", true, true),
        Arc::new(RunningDocker::default()),
        Arc::clone(&capture) as Arc<dyn FrameCapture>,
        true,
    );

    let mut tasks = Vec::new();
    for id in ["run-1", "run-2"] {
        let manager = Arc::clone(&manager);
        let box_ = toolbox(&manager, "arthur", id, "ignored");
        tasks.push(tokio::spawn(async move {
            manager.capture_screen_internal(&box_).await
        }));
    }
    tokio::time::timeout(Duration::from_secs(1), async {
        while capture.started.load(Ordering::SeqCst) < 2 {
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("both detached owners started");

    manager.stop("run-1");
    manager.stop("run-2");
    for task in tasks {
        assert!(task.await.unwrap().observation.is_none());
    }
    assert_eq!(
        manager
            .observation_admission()
            .snapshot()
            .capture_decode_in_use,
        2
    );
    assert_eq!(
        manager.observation_admission().snapshot().retained_in_use,
        2
    );

    let refused = manager
        .capture_screen_internal(&toolbox(&manager, "arthur", "run-3", "ignored"))
        .await;
    assert!(refused.observation.is_none());
    assert_eq!(capture.started.load(Ordering::SeqCst), 2);
    assert_eq!(
        manager
            .observation_admission()
            .snapshot()
            .capture_decode_in_use,
        2
    );
    assert_eq!(
        manager.observation_admission().snapshot().retained_in_use,
        2
    );

    capture.cleanup.add_permits(2);
    tokio::time::timeout(Duration::from_secs(1), async {
        while {
            let snapshot = manager.observation_admission().snapshot();
            snapshot.capture_decode_in_use != 0 || snapshot.retained_in_use != 0
        } {
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("detached cleanup released both leases");
}
