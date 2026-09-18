mod common;

use std::collections::VecDeque;
use std::pin::Pin;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::task::{Context, Poll};
use std::time::Duration;

use futures::Stream;
use model::ladder::Trigger;
use model::{
    EventStream, MessageContent, ModelEvent, ModelMessage, ModelPort, ModelRequest, ToolCall,
};
use server::observations::{ObservationAdmission, RunImageCounters, read_run_image_counters};
use server::runs::{RunEvent, RunManager, StartOptions};
use server::sandbox;
use server::vm::{CapturedFrame, DockerRun, FrameCapture};
use store::Db;
use store::vms::{DockerResult, VmConfig};

use common::{own_conversation, seed_bot};

#[derive(Default)]
struct RunningDocker;

#[async_trait::async_trait]
impl DockerRun for RunningDocker {
    async fn call(&self, _args: &[&str], _timeout_ms: u64) -> DockerResult {
        DockerResult {
            ok: true,
            stdout: "running true".into(),
            stderr: String::new(),
        }
    }
}

#[derive(Default)]
struct CountingCapture(AtomicUsize);

#[async_trait::async_trait]
impl FrameCapture for CountingCapture {
    async fn capture(&self, _container: &str, _cfg: &VmConfig) -> Option<CapturedFrame> {
        self.0.fetch_add(1, Ordering::SeqCst);
        Some(CapturedFrame {
            png: vec![0x89, 0x50, 0x4e, 0x47, 1, 2, 3],
            width: 640,
            height: 480,
        })
    }
}

struct HoldingCapture {
    started: Arc<tokio::sync::Semaphore>,
    release: Arc<tokio::sync::Semaphore>,
}

#[async_trait::async_trait]
impl FrameCapture for HoldingCapture {
    async fn capture(&self, _container: &str, _cfg: &VmConfig) -> Option<CapturedFrame> {
        self.started.add_permits(1);
        let permit = self.release.acquire().await.ok()?;
        permit.forget();
        Some(CapturedFrame {
            png: vec![0x89, 0x50, 0x4e, 0x47, 4, 5, 6],
            width: 640,
            height: 480,
        })
    }
}

struct ScriptPort {
    scripts: Mutex<VecDeque<Vec<ModelEvent>>>,
    requests: Mutex<Vec<ModelRequest>>,
}

impl ScriptPort {
    fn new(scripts: Vec<Vec<ModelEvent>>) -> Self {
        Self {
            scripts: Mutex::new(scripts.into()),
            requests: Mutex::new(Vec::new()),
        }
    }

    fn requests(&self) -> Vec<ModelRequest> {
        self.requests.lock().unwrap().clone()
    }
}

impl ModelPort for ScriptPort {
    fn stream(&self, request: ModelRequest) -> EventStream {
        self.requests.lock().unwrap().push(request);
        let events = self.scripts.lock().unwrap().pop_front().unwrap_or_default();
        Box::pin(futures::stream::iter(events))
    }
}

struct DropEosStream {
    admission: Arc<ObservationAdmission>,
    saw_held: Arc<AtomicBool>,
}

impl Stream for DropEosStream {
    type Item = ModelEvent;

    fn poll_next(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        Poll::Ready(None)
    }
}

impl Drop for DropEosStream {
    fn drop(&mut self) {
        self.saw_held.store(
            self.admission.snapshot().dispatch_in_use == 1,
            Ordering::SeqCst,
        );
    }
}

struct StreamDropPort {
    turn: AtomicUsize,
    requests: Mutex<Vec<ModelRequest>>,
    admission: Mutex<Option<Arc<ObservationAdmission>>>,
    saw_held: Arc<AtomicBool>,
}

struct HeldImagePort {
    turn: AtomicUsize,
    started: Arc<tokio::sync::Semaphore>,
}

impl ModelPort for HeldImagePort {
    fn stream(&self, _request: ModelRequest) -> EventStream {
        if self.turn.fetch_add(1, Ordering::SeqCst) == 0 {
            return Box::pin(futures::stream::iter(vec![snap_call("snap", "{}")]));
        }
        let started = Arc::clone(&self.started);
        Box::pin(async_stream::stream! {
            started.add_permits(1);
            std::future::pending::<()>().await;
            #[allow(unreachable_code)]
            yield ModelEvent::Done { model: "never".into(), usage: None, finish_reason: None };
        })
    }
}

impl ModelPort for StreamDropPort {
    fn stream(&self, request: ModelRequest) -> EventStream {
        self.requests.lock().unwrap().push(request);
        if self.turn.fetch_add(1, Ordering::SeqCst) == 0 {
            return Box::pin(futures::stream::iter(vec![snap_call("snap-1", "{}")]));
        }
        Box::pin(DropEosStream {
            admission: self
                .admission
                .lock()
                .unwrap()
                .clone()
                .expect("admission installed"),
            saw_held: Arc::clone(&self.saw_held),
        })
    }
}

fn snap_call(id: &str, arguments: &str) -> ModelEvent {
    ModelEvent::ToolCalls {
        calls: vec![ToolCall {
            id: id.into(),
            name: "snap_desk".into(),
            arguments: arguments.into(),
        }],
        usage: None,
    }
}

fn done() -> Vec<ModelEvent> {
    vec![ModelEvent::Done {
        model: "provider/actual".into(),
        usage: None,
        finish_reason: None,
    }]
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

fn catalog(images: bool, tools: bool) -> Arc<dyn model::Catalog> {
    let json = serde_json::json!([{
        "id": "vision/model", "name": "vision/model", "inPerM": 1.0, "outPerM": 1.0,
        "contextLength": 32000, "supportsTools": tools, "supportsImages": images,
        "supportsReasoning": false, "providerCount": 2, "supportsCaching": false
    }]);
    Arc::new(model::FixtureCatalog::from_json(&json.to_string()).unwrap())
}

fn setup(
    port: Arc<dyn ModelPort>,
    model_catalog: Arc<dyn model::Catalog>,
) -> (
    Arc<Mutex<Db>>,
    Arc<RunManager>,
    Arc<CountingCapture>,
    String,
) {
    let db = Arc::new(Mutex::new(Db::open(":memory:").unwrap()));
    seed_bot(&db, "arthur", "Arthur");
    let conversation = own_conversation(&db, "arthur");
    {
        let db = db.lock().unwrap();
        model::routing::set_routing_settings(&db, Some(false), None).unwrap();
        server::judge::set_judge_enabled(&db, false).unwrap();
        db.conn().execute(
            "INSERT INTO vms (bot_id, container, cdp_port, web_port, state, last_used_at) VALUES ('arthur', 'bullpen-vm-arthur', 9501, 6501, 'running', '2026-09-18T12:00:00Z')",
            [],
        ).unwrap();
    }
    let capture = Arc::new(CountingCapture::default());
    let manager = Arc::new(RunManager::with_screen_capture(
        Arc::clone(&db),
        port,
        sandbox::default_sandbox(),
        Arc::new(RunningDocker),
        Arc::new(config()),
        true,
        model_catalog,
        Arc::clone(&capture) as Arc<dyn FrameCapture>,
    ));
    (db, manager, capture, conversation)
}

fn manager_with_capture(
    db: Arc<Mutex<Db>>,
    port: Arc<dyn ModelPort>,
    capture: Arc<dyn FrameCapture>,
) -> Arc<RunManager> {
    Arc::new(RunManager::with_screen_capture(
        db,
        port,
        sandbox::default_sandbox(),
        Arc::new(RunningDocker),
        Arc::new(config()),
        true,
        catalog(true, true),
        capture,
    ))
}

fn start(manager: &Arc<RunManager>, conversation_id: String) -> String {
    start_model(manager, conversation_id, "vision/model")
}

fn start_model(manager: &Arc<RunManager>, conversation_id: String, model: &str) -> String {
    manager.start(StartOptions {
        bot_id: "arthur".into(),
        conversation_id,
        model: model.into(),
        messages: vec![ModelMessage::user("look at your screen")],
        trigger: Trigger::Chat,
        room: false,
    })
}

async fn drain(manager: &Arc<RunManager>, run_id: &str) -> Vec<RunEvent> {
    tokio::time::timeout(
        Duration::from_secs(2),
        common::drain(manager.subscribe(run_id)),
    )
    .await
    .expect("run completed")
}

#[tokio::test]
async fn image_is_request_only_after_tool_result_and_dispatch_releases_after_stream_drop() {
    let saw_held = Arc::new(AtomicBool::new(false));
    let port = Arc::new(StreamDropPort {
        turn: AtomicUsize::new(0),
        requests: Mutex::new(Vec::new()),
        admission: Mutex::new(None),
        saw_held: Arc::clone(&saw_held),
    });
    let (db, manager, capture, conversation) = setup(port.clone(), catalog(true, true));
    *port.admission.lock().unwrap() = Some(manager.observation_admission());
    let run_id = start(&manager, conversation);
    let events = drain(&manager, &run_id).await;

    assert_eq!(capture.0.load(Ordering::SeqCst), 1);
    assert!(saw_held.load(Ordering::SeqCst));
    assert_eq!(
        manager.observation_admission().snapshot().dispatch_in_use,
        0
    );
    let requests = port.requests.lock().unwrap();
    let snap_spec = requests[0]
        .tools
        .as_ref()
        .unwrap()
        .iter()
        .find(|spec| spec.name == "snap_desk")
        .unwrap();
    assert_eq!(snap_spec.parameters["type"], "object");
    assert_eq!(snap_spec.parameters["additionalProperties"], false);
    assert_eq!(snap_spec.parameters["properties"], serde_json::json!({}));
    let image_request = &requests[1];
    let tail = &image_request.messages[image_request.messages.len() - 3..];
    assert_eq!(tail[0].role, "assistant");
    assert_eq!(tail[1].role, "tool");
    assert_eq!(tail[2].role, "user");
    match &tail[2].content {
        MessageContent::Parts(parts) => {
            assert!(matches!(parts[0], model::ContentPart::Text { .. }));
            assert!(matches!(parts[1], model::ContentPart::ImageUrl { .. }));
        }
        other => panic!("expected multipart observation, got {other:?}"),
    }
    let stored: String = db
        .lock()
        .unwrap()
        .conn()
        .query_row(
            "SELECT messages FROM runs WHERE id = ?1",
            [&run_id],
            |row| row.get(0),
        )
        .unwrap();
    assert!(!stored.contains("data:image"));
    assert!(!format!("{events:?}").contains("data:image"));
    assert_eq!(
        read_run_image_counters(&db.lock().unwrap(), &run_id).unwrap(),
        Some(RunImageCounters {
            capture_attempts: 1,
            image_dispatches: 1
        })
    );
}

#[tokio::test]
async fn stop_cancels_an_image_wait_and_invalidates_the_observation() {
    let started = Arc::new(tokio::sync::Semaphore::new(0));
    let port = Arc::new(HeldImagePort {
        turn: AtomicUsize::new(0),
        started: Arc::clone(&started),
    });
    let (_db, manager, _capture, conversation) = setup(port, catalog(true, true));
    let run_id = start(&manager, conversation);
    let events = manager.subscribe(&run_id);
    let permit = tokio::time::timeout(Duration::from_secs(2), started.acquire())
        .await
        .expect("image request started")
        .expect("started gate open");
    permit.forget();
    assert_eq!(
        manager.observation_admission().snapshot().dispatch_in_use,
        1
    );
    manager.stop(&run_id);
    let seen = tokio::time::timeout(Duration::from_secs(2), common::drain(events))
        .await
        .expect("stopped image request settled");
    assert!(
        seen.iter()
            .any(|event| matches!(event, RunEvent::Error { message, .. } if message == "Stopped."))
    );
    let snapshot = manager.observation_admission().snapshot();
    assert_eq!(snapshot.dispatch_in_use, 0);
    assert_eq!(snapshot.retained_in_use, 0);
    assert!(manager.observation_registry().metadata(&run_id).is_none());
}

#[tokio::test]
async fn persisted_dispatch_cap_refuses_the_image_and_invalidates_its_bytes() {
    let port = Arc::new(ScriptPort::new(vec![vec![snap_call("snap", "{}")], done()]));
    let (db, _unused, _capture, conversation) = setup(port.clone(), catalog(true, true));
    let started = Arc::new(tokio::sync::Semaphore::new(0));
    let release = Arc::new(tokio::sync::Semaphore::new(0));
    let manager = manager_with_capture(
        Arc::clone(&db),
        port.clone(),
        Arc::new(HoldingCapture {
            started: Arc::clone(&started),
            release: Arc::clone(&release),
        }),
    );
    let run_id = start(&manager, conversation);
    let events = manager.subscribe(&run_id);
    let permit = tokio::time::timeout(Duration::from_secs(2), started.acquire())
        .await
        .expect("capture started")
        .expect("started gate open");
    permit.forget();
    db.lock()
        .unwrap()
        .conn()
        .execute(
            "UPDATE runs SET screen_image_dispatches = 8 WHERE id = ?1",
            [&run_id],
        )
        .unwrap();
    release.add_permits(1);
    tokio::time::timeout(Duration::from_secs(2), common::drain(events))
        .await
        .expect("capped run completed");

    let requests = port.requests();
    assert_eq!(requests.len(), 2);
    assert!(
        !requests[1]
            .messages
            .iter()
            .any(|message| matches!(message.content, MessageContent::Parts(_)))
    );
    assert_eq!(
        read_run_image_counters(&db.lock().unwrap(), &run_id).unwrap(),
        Some(RunImageCounters {
            capture_attempts: 1,
            image_dispatches: 8
        })
    );
    assert!(manager.observation_registry().metadata(&run_id).is_none());
    assert_eq!(
        manager.observation_admission().snapshot().retained_in_use,
        0
    );
}

#[tokio::test]
async fn hidden_hallucinated_and_nonempty_snap_calls_refuse_before_capture() {
    for (images, tools, arguments) in [
        (false, true, "{}"),
        (true, false, "{}"),
        (true, true, "{\"bot\":\"other\"}"),
    ] {
        let port = Arc::new(ScriptPort::new(vec![
            vec![snap_call("snap", arguments)],
            done(),
        ]));
        let (_db, manager, capture, conversation) = setup(port.clone(), catalog(images, tools));
        let run_id = start(&manager, conversation);
        drain(&manager, &run_id).await;
        assert_eq!(capture.0.load(Ordering::SeqCst), 0);
        let requests = port.requests();
        let offered = requests[0]
            .tools
            .as_ref()
            .is_some_and(|specs| specs.iter().any(|spec| spec.name == "snap_desk"));
        assert_eq!(offered, images && tools);
    }
}

#[tokio::test]
async fn explicit_deny_and_narrowing_refuse_snap_before_capture() {
    let port = Arc::new(ScriptPort::new(vec![vec![snap_call("snap", "{}")], done()]));
    let (db, manager, capture, conversation) = setup(port, catalog(true, true));
    {
        let db = db.lock().unwrap();
        let mut permissions = server::permissions::get_permissions(&db, "arthur").unwrap();
        permissions.insert("snap_desk".into(), server::permissions::Decision::Deny);
        server::permissions::set_permissions(&db, "arthur", &permissions).unwrap();
    }
    let run_id = start(&manager, conversation);
    let events = drain(&manager, &run_id).await;
    assert_eq!(capture.0.load(Ordering::SeqCst), 0);
    assert!(events.iter().any(|event| matches!(event, RunEvent::ToolResult { result, .. } if result.contains("switched off"))));

    let port = Arc::new(ScriptPort::new(vec![vec![snap_call("snap", "{}")], done()]));
    let (_db, manager, capture, conversation) = setup(port, catalog(true, true));
    let run_id = manager.start_routine_narrowed(
        StartOptions {
            bot_id: "arthur".into(),
            conversation_id: conversation,
            model: "vision/model".into(),
            messages: vec![ModelMessage::user("look")],
            trigger: Trigger::Chat,
            room: false,
        },
        "routine-fixture".into(),
        vec!["say".into()],
    );
    let events = drain(&manager, &run_id).await;
    assert_eq!(capture.0.load(Ordering::SeqCst), 0);
    assert!(events.iter().any(|event| matches!(event, RunEvent::ToolResult { result, .. } if result.contains("switched off"))));
}

#[tokio::test]
async fn mixed_and_repeated_snap_batches_refuse_every_snap_before_capture() {
    for calls in [
        vec![
            ToolCall {
                id: "snap".into(),
                name: "snap_desk".into(),
                arguments: "{}".into(),
            },
            ToolCall {
                id: "say".into(),
                name: "say".into(),
                arguments: "{\"text\":\"hi\"}".into(),
            },
        ],
        vec![
            ToolCall {
                id: "snap-1".into(),
                name: "snap_desk".into(),
                arguments: "{}".into(),
            },
            ToolCall {
                id: "snap-2".into(),
                name: "snap_desk".into(),
                arguments: "{}".into(),
            },
        ],
    ] {
        let port = Arc::new(ScriptPort::new(vec![
            vec![ModelEvent::ToolCalls { calls, usage: None }],
            done(),
        ]));
        let (_db, manager, capture, conversation) = setup(port, catalog(true, true));
        let run_id = start(&manager, conversation);
        let events = drain(&manager, &run_id).await;
        assert_eq!(capture.0.load(Ordering::SeqCst), 0);
        assert!(events.iter().any(|event| matches!(event, RunEvent::ToolResult { name, result } if name == "snap_desk" && result.contains("only tool call"))));
    }
}

async fn approval_case(approved: bool) -> (usize, Vec<ModelRequest>, RunImageCounters) {
    let port = Arc::new(ScriptPort::new(vec![vec![snap_call("snap", "{}")], done()]));
    let (db, manager, capture, conversation) = setup(port.clone(), catalog(true, true));
    {
        let db = db.lock().unwrap();
        let mut permissions = server::permissions::get_permissions(&db, "arthur").unwrap();
        permissions.insert("snap_desk".into(), server::permissions::Decision::Ask);
        server::permissions::set_permissions(&db, "arthur", &permissions).unwrap();
    }
    let run_id = start(&manager, conversation);
    let mut events = manager.subscribe(&run_id);
    let approval_id = tokio::time::timeout(Duration::from_secs(2), async {
        loop {
            if let Some(RunEvent::ApprovalNeeded { approval_id, .. }) = events.recv().await {
                break approval_id;
            }
        }
    })
    .await
    .expect("approval requested");
    assert_eq!(capture.0.load(Ordering::SeqCst), 0);
    assert!(manager.decide_approval(&approval_id, approved, None).await);
    tokio::time::timeout(Duration::from_secs(2), common::drain(events))
        .await
        .expect("resumed run completed");
    let counters = read_run_image_counters(&db.lock().unwrap(), &run_id)
        .unwrap()
        .unwrap();
    (capture.0.load(Ordering::SeqCst), port.requests(), counters)
}

#[tokio::test]
async fn snap_approval_runs_after_safe_resume_while_denial_never_captures_or_dispatches() {
    let (captures, requests, counters) = approval_case(true).await;
    assert_eq!(captures, 1);
    assert_eq!(requests.len(), 2);
    assert!(matches!(
        requests[1].messages.last().map(|message| &message.content),
        Some(MessageContent::Parts(parts))
            if parts.iter().any(|part| matches!(part, model::ContentPart::ImageUrl { .. }))
    ));
    assert_eq!(
        counters,
        RunImageCounters {
            capture_attempts: 1,
            image_dispatches: 1
        }
    );

    let (captures, requests, counters) = approval_case(false).await;
    assert_eq!(captures, 0);
    assert_eq!(requests.len(), 2);
    assert_eq!(
        counters,
        RunImageCounters {
            capture_attempts: 0,
            image_dispatches: 0
        }
    );
}

#[tokio::test]
async fn approved_capture_is_owned_after_the_decision_request_returns() {
    let port = Arc::new(ScriptPort::new(vec![vec![snap_call("snap", "{}")], done()]));
    let (db, _unused, _capture, conversation) = setup(port.clone(), catalog(true, true));
    {
        let db = db.lock().unwrap();
        let mut permissions = server::permissions::get_permissions(&db, "arthur").unwrap();
        permissions.insert("snap_desk".into(), server::permissions::Decision::Ask);
        server::permissions::set_permissions(&db, "arthur", &permissions).unwrap();
    }
    let started = Arc::new(tokio::sync::Semaphore::new(0));
    let release = Arc::new(tokio::sync::Semaphore::new(0));
    let manager = manager_with_capture(
        Arc::clone(&db),
        port,
        Arc::new(HoldingCapture {
            started: Arc::clone(&started),
            release: Arc::clone(&release),
        }),
    );
    let run_id = start(&manager, conversation);
    let mut events = manager.subscribe(&run_id);
    let approval_id = tokio::time::timeout(Duration::from_secs(2), async {
        loop {
            if let Some(RunEvent::ApprovalNeeded { approval_id, .. }) = events.recv().await {
                break approval_id;
            }
        }
    })
    .await
    .expect("approval requested");

    assert!(manager.decide_approval(&approval_id, true, None).await);
    let permit = tokio::time::timeout(Duration::from_secs(2), started.acquire())
        .await
        .expect("owned approval task reached capture")
        .expect("started gate open");
    permit.forget();
    assert_eq!(
        manager.observation_admission().snapshot().retained_in_use,
        1
    );
    release.add_permits(1);
    tokio::time::timeout(Duration::from_secs(2), common::drain(events))
        .await
        .expect("owned approval task completed");
    let snapshot = manager.observation_admission().snapshot();
    assert_eq!(snapshot.retained_in_use, 0);
    assert_eq!(snapshot.capture_decode_in_use, 0);
    assert_eq!(snapshot.dispatch_in_use, 0);
    assert!(!manager.observation_registry().has_bytes(&run_id));
}

#[tokio::test]
async fn escalation_refreshes_snap_offering_for_the_next_request() {
    let target = model::ladder::DEFAULT_TIER1.code;
    let json = serde_json::json!([
        {
            "id": "test/unconfigured-model", "name": "initial", "inPerM": 1.0, "outPerM": 1.0,
            "contextLength": 32000, "supportsTools": true, "supportsImages": false,
            "supportsReasoning": false, "providerCount": 2, "supportsCaching": false
        },
        {
            "id": target, "name": target, "inPerM": 1.0, "outPerM": 1.0,
            "contextLength": 32000, "supportsTools": true, "supportsImages": true,
            "supportsReasoning": false, "providerCount": 2, "supportsCaching": false
        }
    ]);
    let port = Arc::new(ScriptPort::new(vec![
        vec![ModelEvent::ToolCalls {
            calls: vec![ToolCall {
                id: "escalate".into(),
                name: "escalate".into(),
                arguments: r#"{"reason":"need vision","kind":"code"}"#.into(),
            }],
            usage: None,
        }],
        done(),
    ]));
    let (_db, manager, _capture, conversation) = setup(
        port.clone(),
        Arc::new(model::FixtureCatalog::from_json(&json.to_string()).unwrap()),
    );
    let run_id = start_model(&manager, conversation, "test/unconfigured-model");
    drain(&manager, &run_id).await;
    let requests = port.requests();
    assert!(
        !requests[0]
            .tools
            .as_ref()
            .unwrap()
            .iter()
            .any(|spec| spec.name == "snap_desk")
    );
    assert!(
        requests[1]
            .tools
            .as_ref()
            .unwrap()
            .iter()
            .any(|spec| spec.name == "snap_desk")
    );
    assert_eq!(requests[1].model, target);
}

#[test]
fn tainted_snap_is_ask_and_an_allow_rule_cannot_lift_it() {
    assert_eq!(
        server::permissions::decide_call(
            server::permissions::Decision::Allow,
            "snap_desk",
            "{}",
            true,
        ),
        server::permissions::Decision::Ask
    );
    let rule = server::rules::Rule {
        id: "allow-snap".into(),
        bot_id: None,
        text: "allow screen captures".into(),
        behavior: server::rules::RuleBehavior::Allow,
        hits: 0,
        created_at: "2026-09-18T12:00:00Z".into(),
    };
    assert_eq!(
        server::rules::resolve_decision(
            &[rule],
            &["allow-snap".into()],
            "snap_desk",
            Trigger::Chat,
            true,
        )
        .decision,
        server::permissions::Decision::Ask
    );
}

#[tokio::test]
async fn capture_on_final_step_settles_without_retaining_an_undelivered_frame() {
    let mut scripts = Vec::new();
    for step in 0..23 {
        scripts.push(vec![ModelEvent::ToolCalls {
            calls: vec![ToolCall {
                id: format!("say-{step}"),
                name: "say".into(),
                arguments: r#"{"text":"continuing"}"#.into(),
            }],
            usage: None,
        }]);
    }
    scripts.push(vec![snap_call("final-snap", "{}")]);
    let port = Arc::new(ScriptPort::new(scripts));
    let (db, manager, capture, conversation) = setup(port.clone(), catalog(true, true));
    let run_id = start(&manager, conversation);
    let events = drain(&manager, &run_id).await;
    assert_eq!(capture.0.load(Ordering::SeqCst), 1);
    assert_eq!(port.requests().len(), 24);
    assert!(
        events
            .iter()
            .any(|event| matches!(event, RunEvent::Error { message, .. }
        if message == "Stopped after 24 tool steps without an answer."))
    );
    let snapshot = manager.observation_admission().snapshot();
    assert_eq!(snapshot.retained_in_use, 0);
    assert_eq!(snapshot.capture_decode_in_use, 0);
    assert_eq!(snapshot.dispatch_in_use, 0);
    assert!(manager.observation_registry().metadata(&run_id).is_none());
    let counters = read_run_image_counters(&db.lock().unwrap(), &run_id)
        .unwrap()
        .unwrap();
    assert_eq!(counters.capture_attempts, 1);
    assert_eq!(counters.image_dispatches, 0);
}
