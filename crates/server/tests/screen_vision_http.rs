mod common;

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use base64::Engine;
use common::{own_conversation, seed_bot};
use model::ladder::Trigger;
use model::secrets::KeySource;
use model::{ModelMessage, OpenRouterPort};
use server::observations::ObservationAdmission;
use server::runs::{RunEvent, RunManager, StartOptions};
use server::sandbox;
use server::vm::{CapturedFrame, DockerRun, FrameCapture};
use sha2::{Digest, Sha256};
use store::Db;
use store::vms::{DockerResult, VmConfig};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;

#[derive(Clone, Copy)]
enum FinalResponse {
    Success,
    ProviderHttpError,
    ProviderSseError,
    BusyExhausted,
}

#[derive(Debug, Clone)]
struct WireSummary {
    model: String,
    body_hash: String,
    roles: Vec<String>,
    assistant_call: Option<(String, String)>,
    tool_results: Vec<(String, String)>,
    user_part_types: Vec<String>,
    provenance: Option<String>,
    image_hash: Option<String>,
    dispatch_in_use: usize,
}

struct Recorder {
    summaries: Arc<Mutex<Vec<WireSummary>>>,
    admission: Arc<Mutex<Option<Arc<ObservationAdmission>>>>,
    final_response: FinalResponse,
}

impl Recorder {
    async fn spawn(final_response: FinalResponse) -> (String, Arc<Self>) {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let endpoint = format!(
            "http://127.0.0.1:{}/api/v1/chat/completions",
            listener.local_addr().unwrap().port()
        );
        let recorder = Arc::new(Self {
            summaries: Arc::new(Mutex::new(Vec::new())),
            admission: Arc::new(Mutex::new(None)),
            final_response,
        });
        let state = Arc::clone(&recorder);
        tokio::spawn(async move {
            loop {
                let Ok((mut socket, _)) = listener.accept().await else {
                    return;
                };
                let mut bytes = Vec::new();
                let header_end = loop {
                    let mut chunk = [0u8; 4096];
                    let n = socket.read(&mut chunk).await.unwrap_or(0);
                    if n == 0 {
                        return;
                    }
                    bytes.extend_from_slice(&chunk[..n]);
                    if let Some(pos) = bytes.windows(4).position(|w| w == b"\r\n\r\n") {
                        break pos + 4;
                    }
                };
                let headers = String::from_utf8_lossy(&bytes[..header_end]);
                let content_length = headers
                    .lines()
                    .find_map(|line| {
                        line.split_once(':').and_then(|(name, value)| {
                            name.eq_ignore_ascii_case("content-length")
                                .then(|| value.trim().parse::<usize>().ok())
                                .flatten()
                        })
                    })
                    .unwrap_or(0);
                while bytes.len() < header_end + content_length {
                    let mut chunk = [0u8; 4096];
                    let n = socket.read(&mut chunk).await.unwrap_or(0);
                    if n == 0 {
                        break;
                    }
                    bytes.extend_from_slice(&chunk[..n]);
                }
                let body = &bytes[header_end..header_end + content_length];
                let dispatch_in_use = state
                    .admission
                    .lock()
                    .unwrap()
                    .as_ref()
                    .map_or(0, |admission| admission.snapshot().dispatch_in_use);
                let summary = summarize(body, dispatch_in_use);
                let request_number = {
                    let mut summaries = state.summaries.lock().unwrap();
                    summaries.push(summary);
                    summaries.len()
                };

                let response = match request_number {
                    1 => sse_response(tool_call_sse()),
                    _ if matches!(state.final_response, FinalResponse::BusyExhausted)
                        || (matches!(state.final_response, FinalResponse::Success)
                            && matches!(request_number, 2 | 3)) =>
                    {
                        http_response(
                            "429 Too Many Requests",
                            "application/json",
                            r#"{"error":{"metadata":{"retry_after_seconds":0}}}"#,
                            &[("Retry-After", "0")],
                        )
                    }
                    _ if matches!(state.final_response, FinalResponse::Success) => {
                        sse_response(done_sse())
                    }
                    _ if matches!(state.final_response, FinalResponse::ProviderSseError) => {
                        sse_response(error_sse())
                    }
                    _ => http_response(
                        "400 Bad Request",
                        "application/json",
                        r#"{"error":{"message":"rejected data:image/png;base64,SHOULD_NOT_ESCAPE"}}"#,
                        &[],
                    ),
                };
                socket.write_all(response.as_bytes()).await.unwrap();
                socket.shutdown().await.unwrap();
            }
        });
        (endpoint, recorder)
    }
}

fn summarize(body: &[u8], dispatch_in_use: usize) -> WireSummary {
    let value: serde_json::Value = serde_json::from_slice(body).expect("request JSON");
    let messages = value["messages"].as_array().expect("messages array");
    let roles = messages
        .iter()
        .map(|message| message["role"].as_str().unwrap_or("").to_string())
        .collect();
    let assistant_call = messages.iter().rev().find_map(|message| {
        (message["role"] == "assistant").then(|| {
            let call = &message["tool_calls"][0];
            (
                call["id"].as_str().unwrap_or("").to_string(),
                call["function"]["name"].as_str().unwrap_or("").to_string(),
            )
        })
    });
    let tool_results = messages
        .iter()
        .filter(|message| message["role"] == "tool")
        .map(|message| {
            (
                message["tool_call_id"].as_str().unwrap_or("").to_string(),
                message["content"].as_str().unwrap_or("").to_string(),
            )
        })
        .collect();
    let mut user_part_types = Vec::new();
    let mut provenance = None;
    let mut image_hash = None;
    if let Some(parts) = messages
        .last()
        .and_then(|message| message["content"].as_array())
    {
        for part in parts {
            let kind = part["type"].as_str().unwrap_or("").to_string();
            user_part_types.push(kind.clone());
            if kind == "text" {
                provenance = part["text"].as_str().map(str::to_string);
            } else if kind == "image_url" {
                let image = part["image_url"]["url"].as_str().unwrap_or("");
                image_hash = Some(hex::encode(Sha256::digest(image.as_bytes())));
            }
        }
    }
    WireSummary {
        model: value["model"].as_str().unwrap_or("").to_string(),
        body_hash: hex::encode(Sha256::digest(body)),
        roles,
        assistant_call,
        tool_results,
        user_part_types,
        provenance,
        image_hash,
        dispatch_in_use,
    }
}

fn http_response(
    status: &str,
    content_type: &str,
    body: &str,
    extra_headers: &[(&str, &str)],
) -> String {
    let extras = extra_headers
        .iter()
        .map(|(name, value)| format!("{name}: {value}\r\n"))
        .collect::<String>();
    format!(
        "HTTP/1.1 {status}\r\nContent-Type: {content_type}\r\nContent-Length: {}\r\n{extras}Connection: close\r\n\r\n{body}",
        body.len()
    )
}

fn sse_response(body: String) -> String {
    http_response("200 OK", "text/event-stream", &body, &[])
}

fn tool_call_sse() -> String {
    let frame = serde_json::json!({
        "model": "vision/model",
        "choices": [{
            "delta": {
                "tool_calls": [{
                    "index": 0,
                    "id": "snap-call",
                    "function": {"name": "snap_desk", "arguments": "{}"}
                }]
            },
            "finish_reason": "tool_calls"
        }]
    });
    format!("data: {frame}\n\ndata: [DONE]\n\n")
}

fn error_sse() -> String {
    let frame = serde_json::json!({
        "error": {
            "message": "stream rejected data:image/png;base64,SHOULD_NOT_ESCAPE"
        }
    });
    format!(
        "data: {frame}

data: [DONE]

"
    )
}

fn done_sse() -> String {
    let frame = serde_json::json!({
        "model": "provider/actual",
        "choices": [{"delta": {"content": "screen seen"}, "finish_reason": "stop"}],
        "usage": {
            "cost": 0.001,
            "prompt_tokens": 10,
            "completion_tokens": 2,
            "prompt_tokens_details": {"cached_tokens": 0}
        }
    });
    format!("data: {frame}\n\ndata: [DONE]\n\n")
}

struct RunningDocker;

#[async_trait]
impl DockerRun for RunningDocker {
    async fn call(&self, _args: &[&str], _timeout_ms: u64) -> DockerResult {
        DockerResult {
            ok: true,
            stdout: "running true".into(),
            stderr: String::new(),
        }
    }
}

const FRAME_BYTES: &[u8] = &[0x89, 0x50, 0x4e, 0x47, 9, 8, 7, 6];

#[derive(Default)]
struct OwnFrameCapture(AtomicUsize);

#[async_trait]
impl FrameCapture for OwnFrameCapture {
    async fn capture(&self, _container: &str, _cfg: &VmConfig) -> Option<CapturedFrame> {
        self.0.fetch_add(1, Ordering::SeqCst);
        Some(CapturedFrame {
            png: FRAME_BYTES.to_vec(),
            width: 320,
            height: 200,
        })
    }
}

fn config() -> VmConfig {
    VmConfig {
        image: "test".into(),
        docker_host: "unix:///test.sock".into(),
        cdp_base: 9500,
        web_base: 6500,
        slots: 8,
        idle_ms: 1_800_000,
        init_dir: "/vm-init".into(),
        memory: "3g".into(),
        cpus: "1.5".into(),
        shm_size: "1g".into(),
        timezone: "UTC".into(),
        puid: "1004".into(),
        pgid: "1004".into(),
    }
}

fn catalog() -> Arc<dyn model::Catalog> {
    let json = serde_json::json!([{
        "id": "vision/model",
        "name": "vision/model",
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

async fn run_case(
    final_response: FinalResponse,
) -> (
    Arc<Mutex<Db>>,
    Arc<RunManager>,
    Arc<OwnFrameCapture>,
    Arc<Recorder>,
    String,
    Vec<RunEvent>,
) {
    let (endpoint, recorder) = Recorder::spawn(final_response).await;
    let port = Arc::new(
        OpenRouterPort::with_local_endpoint(KeySource::Inline("fixture-key".into()), endpoint)
            .unwrap(),
    );
    let db = Arc::new(Mutex::new(Db::open(":memory:").unwrap()));
    seed_bot(&db, "arthur", "Arthur");
    let conversation_id = own_conversation(&db, "arthur");
    {
        let db = db.lock().unwrap();
        model::routing::set_routing_settings(&db, Some(false), None).unwrap();
        server::judge::set_judge_enabled(&db, false).unwrap();
        db.conn()
            .execute(
                "INSERT INTO vms (bot_id, container, cdp_port, web_port, state, last_used_at)
                 VALUES ('arthur', 'bullpen-vm-arthur', 9501, 6501, 'running', '2026-09-18T12:00:00Z')",
                [],
            )
            .unwrap();
    }
    let capture = Arc::new(OwnFrameCapture::default());
    let manager = Arc::new(RunManager::with_screen_capture(
        Arc::clone(&db),
        port,
        sandbox::default_sandbox(),
        Arc::new(RunningDocker),
        Arc::new(config()),
        true,
        catalog(),
        Arc::clone(&capture) as Arc<dyn FrameCapture>,
    ));
    *recorder.admission.lock().unwrap() = Some(manager.observation_admission());
    let run_id = manager.start(StartOptions {
        bot_id: "arthur".into(),
        conversation_id,
        model: "vision/model".into(),
        messages: vec![ModelMessage::user("inspect the screen")],
        trigger: Trigger::Chat,
        room: false,
    });
    let events = tokio::time::timeout(
        std::time::Duration::from_secs(5),
        common::drain(manager.subscribe(&run_id)),
    )
    .await
    .expect("real HTTP run settled");
    (db, manager, capture, recorder, run_id, events)
}

#[tokio::test]
async fn real_http_runmanager_image_request_retries_identical_wire_body() {
    let (db, manager, capture, recorder, run_id, events) = run_case(FinalResponse::Success).await;
    assert_eq!(capture.0.load(Ordering::SeqCst), 1);
    let summaries = recorder.summaries.lock().unwrap().clone();
    assert_eq!(summaries.len(), 4);
    let image_attempts = &summaries[1..];
    assert!(
        image_attempts
            .iter()
            .all(|request| request.model == "vision/model")
    );
    assert!(
        image_attempts
            .iter()
            .all(|request| request.body_hash == image_attempts[0].body_hash)
    );
    assert!(
        image_attempts
            .iter()
            .all(|request| request.image_hash == image_attempts[0].image_hash)
    );
    assert!(
        image_attempts
            .iter()
            .all(|request| request.dispatch_in_use == 1)
    );

    let wire = &image_attempts[0];
    assert_eq!(
        &wire.roles[wire.roles.len() - 3..],
        ["assistant", "tool", "user"]
    );
    assert_eq!(
        wire.assistant_call,
        Some(("snap-call".into(), "snap_desk".into()))
    );
    assert_eq!(wire.tool_results.len(), 1);
    assert_eq!(wire.tool_results[0].0, "snap-call");
    assert!(wire.tool_results[0].1.contains("Captured observation"));
    assert_eq!(wire.user_part_types, ["text", "image_url"]);
    assert!(
        wire.provenance
            .as_deref()
            .is_some_and(|text| text.contains("Screen observation"))
    );
    let expected_uri = format!(
        "data:image/png;base64,{}",
        base64::engine::general_purpose::STANDARD.encode(FRAME_BYTES)
    );
    assert_eq!(
        wire.image_hash.as_deref(),
        Some(hex::encode(Sha256::digest(expected_uri.as_bytes())).as_str()),
        "wire image must match the captured frame, not merely match prior retries"
    );

    let stored: (String, String, String) = db
        .lock()
        .unwrap()
        .conn()
        .query_row(
            "SELECT status, messages, text FROM runs WHERE id = ?1",
            [&run_id],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
        )
        .unwrap();
    assert_eq!(stored.0, "done");
    assert!(!stored.1.contains("data:image"));
    assert!(!stored.2.contains("data:image"));
    assert!(!format!("{events:?}").contains("data:image"));
    assert_eq!(
        manager.observation_admission().snapshot().dispatch_in_use,
        0
    );
    assert!(manager.observation_registry().metadata(&run_id).is_none());
}

#[tokio::test]
async fn real_http_provider_error_redacts_image_and_never_falls_back() {
    for response in [
        FinalResponse::ProviderHttpError,
        FinalResponse::ProviderSseError,
    ] {
        let (db, manager, capture, recorder, run_id, events) = run_case(response).await;
        assert_eq!(capture.0.load(Ordering::SeqCst), 1);
        let summaries = recorder.summaries.lock().unwrap().clone();
        assert_eq!(
            summaries.len(),
            2,
            "image provider error must not fall back"
        );
        assert_eq!(summaries[1].model, "vision/model");
        assert_eq!(summaries[1].dispatch_in_use, 1);

        let rendered = format!("{events:?}");
        assert!(!rendered.contains("data:image"));
        assert!(!rendered.contains("SHOULD_NOT_ESCAPE"));
        assert!(rendered.contains("without exposing its payload"));
        let stored: (String, Option<String>, String) = db
            .lock()
            .unwrap()
            .conn()
            .query_row(
                "SELECT status, error, messages FROM runs WHERE id = ?1",
                [&run_id],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
            )
            .unwrap();
        assert_eq!(stored.0, "failed");
        assert!(!stored.1.unwrap_or_default().contains("data:image"));
        assert!(!stored.2.contains("data:image"));
        assert_eq!(
            manager.observation_admission().snapshot().dispatch_in_use,
            0
        );
        assert!(manager.observation_registry().metadata(&run_id).is_none());
    }
}

#[test]
fn local_endpoint_constructor_refuses_external_destinations() {
    assert!(
        OpenRouterPort::with_local_endpoint(
            KeySource::Inline("fixture-key".into()),
            "http://example.com/api/v1/chat/completions"
        )
        .is_err(),
        "plain HTTP must still reject a non-loopback host"
    );
    assert!(
        OpenRouterPort::with_local_endpoint(
            KeySource::Inline("fixture-key".into()),
            "https://openrouter.ai/api/v1/chat/completions"
        )
        .is_err()
    );
}

fn manager_for_endpoint(
    db: Arc<Mutex<Db>>,
    endpoint: String,
    capture: Arc<OwnFrameCapture>,
) -> Arc<RunManager> {
    let port = Arc::new(
        OpenRouterPort::with_local_endpoint(KeySource::Inline("fixture-key".into()), endpoint)
            .unwrap(),
    );
    Arc::new(RunManager::with_screen_capture(
        db,
        port,
        sandbox::default_sandbox(),
        Arc::new(RunningDocker),
        Arc::new(config()),
        true,
        catalog(),
        capture,
    ))
}

#[tokio::test]
async fn real_http_pause_is_metadata_only_and_persisted_approval_resumes_after_restart() {
    let (endpoint, recorder) = Recorder::spawn(FinalResponse::Success).await;
    let db = Arc::new(Mutex::new(Db::open(":memory:").unwrap()));
    seed_bot(&db, "arthur", "Arthur");
    let conversation_id = own_conversation(&db, "arthur");
    {
        let db = db.lock().unwrap();
        model::routing::set_routing_settings(&db, Some(false), None).unwrap();
        server::judge::set_judge_enabled(&db, false).unwrap();
        db.conn()
            .execute(
                "INSERT INTO vms (bot_id, container, cdp_port, web_port, state, last_used_at)
                 VALUES ('arthur', 'bullpen-vm-arthur', 9501, 6501, 'running', '2026-09-18T12:00:00Z')",
                [],
            )
            .unwrap();
        let mut permissions = server::permissions::get_permissions(&db, "arthur").unwrap();
        permissions.insert("snap_desk".into(), server::permissions::Decision::Ask);
        server::permissions::set_permissions(&db, "arthur", &permissions).unwrap();
    }
    let first_capture = Arc::new(OwnFrameCapture::default());
    let first = manager_for_endpoint(
        Arc::clone(&db),
        endpoint.clone(),
        Arc::clone(&first_capture),
    );
    *recorder.admission.lock().unwrap() = Some(first.observation_admission());
    let run_id = first.start(StartOptions {
        bot_id: "arthur".into(),
        conversation_id,
        model: "vision/model".into(),
        messages: vec![ModelMessage::user("inspect the screen")],
        trigger: Trigger::Chat,
        room: false,
    });
    let mut first_events = first.subscribe(&run_id);
    let approval_id = tokio::time::timeout(std::time::Duration::from_secs(2), async {
        loop {
            if let Some(RunEvent::ApprovalNeeded { approval_id, .. }) = first_events.recv().await {
                break approval_id;
            }
        }
    })
    .await
    .expect("real HTTP snap paused for approval");
    assert_eq!(first_capture.0.load(Ordering::SeqCst), 0);
    assert_eq!(recorder.summaries.lock().unwrap().len(), 1);
    let paused: (String, String) = db
        .lock()
        .unwrap()
        .conn()
        .query_row(
            "SELECT status, messages FROM runs WHERE id = ?1",
            [&run_id],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .unwrap();
    assert_eq!(paused.0, "waiting");
    assert!(!paused.1.contains("data:image"));
    assert!(!format!("{:?}", first_events.try_recv()).contains("data:image"));
    drop(first_events);
    drop(first);

    let resumed_capture = Arc::new(OwnFrameCapture::default());
    let resumed = manager_for_endpoint(Arc::clone(&db), endpoint, Arc::clone(&resumed_capture));
    *recorder.admission.lock().unwrap() = Some(resumed.observation_admission());
    let events = resumed.subscribe(&run_id);
    assert!(resumed.decide_approval(&approval_id, true, None).await);
    let seen = tokio::time::timeout(std::time::Duration::from_secs(5), common::drain(events))
        .await
        .expect("persisted approval resumed over real HTTP");
    assert_eq!(resumed_capture.0.load(Ordering::SeqCst), 1);
    assert_eq!(recorder.summaries.lock().unwrap().len(), 4);
    assert!(!format!("{seen:?}").contains("data:image"));
    let settled: (String, String) = db
        .lock()
        .unwrap()
        .conn()
        .query_row(
            "SELECT status, messages FROM runs WHERE id = ?1",
            [&run_id],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .unwrap();
    assert_eq!(settled.0, "done");
    assert!(!settled.1.contains("data:image"));
}

#[tokio::test]
async fn real_http_exhausted_image_retries_never_switch_models() {
    let (db, manager, capture, recorder, run_id, events) =
        run_case(FinalResponse::BusyExhausted).await;
    assert_eq!(capture.0.load(Ordering::SeqCst), 1);
    let summaries = recorder.summaries.lock().unwrap().clone();
    assert_eq!(
        summaries.len(),
        4,
        "one tool-call request plus exactly three image attempts; no fallback attempt"
    );
    assert!(
        summaries
            .iter()
            .all(|request| request.model == "vision/model")
    );
    assert!(
        summaries[1..]
            .iter()
            .all(|request| request.image_hash.is_some() && request.dispatch_in_use == 1)
    );
    let status: String = db
        .lock()
        .unwrap()
        .conn()
        .query_row("SELECT status FROM runs WHERE id=?1", [&run_id], |row| {
            row.get(0)
        })
        .unwrap();
    assert_eq!(status, "failed");
    assert!(!format!("{events:?}").contains("data:image"));
    assert_eq!(
        manager.observation_admission().snapshot().dispatch_in_use,
        0
    );
    assert!(manager.observation_registry().metadata(&run_id).is_none());
}
