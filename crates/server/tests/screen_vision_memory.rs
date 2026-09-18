use std::alloc::{GlobalAlloc, Layout, System};
use std::io::{BufRead, BufReader, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::process::{Child, Command, Stdio};
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::thread;
use std::time::{Duration, Instant};

use futures::StreamExt;
use model::secrets::KeySource;
use model::{
    FunctionCall, MessageContent, MessageToolCall, ModelMessage, ModelPort, ModelRequest,
    OpenRouterPort,
};
use server::observations::ObservationAdmission;
use server::runs::screen_observation_request_message;
use server::vm::CapturedFrame;
use sha2::{Digest, Sha256};
use socket2::SockRef;
use tokio::io::{AsyncReadExt, AsyncWriteExt};

const ROLE_ENV: &str = "BULLPEN_S8D_MEMORY_ROLE";
const SCENARIO_ENV: &str = "BULLPEN_S8D_MEMORY_SCENARIO";
const ENDPOINT_ENV: &str = "BULLPEN_S8D_MEMORY_ENDPOINT";
const CONTROL_ENV: &str = "BULLPEN_S8D_MEMORY_CONTROL";
const FRAME_BYTES: usize = server::vm::MAX_FRAME_PNG_BYTES;
const SINGLE_BUDGET: usize = 24 * 1024 * 1024;
const DOUBLE_BUDGET: usize = 48 * 1024 * 1024;
const CLEANUP_TOLERANCE: usize = 1024 * 1024;
const UPLOAD_PREFIX_BYTES: usize = 4096;

struct CountingAllocator;

static LIVE: AtomicUsize = AtomicUsize::new(0);
static PEAK: AtomicUsize = AtomicUsize::new(0);

fn add_live(bytes: usize) {
    let current = LIVE.fetch_add(bytes, Ordering::SeqCst) + bytes;
    let mut peak = PEAK.load(Ordering::SeqCst);
    while current > peak {
        match PEAK.compare_exchange_weak(peak, current, Ordering::SeqCst, Ordering::SeqCst) {
            Ok(_) => break,
            Err(actual) => peak = actual,
        }
    }
}

fn subtract_live(bytes: usize) {
    LIVE.fetch_sub(bytes, Ordering::SeqCst);
}

unsafe impl GlobalAlloc for CountingAllocator {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        let ptr = unsafe { System.alloc(layout) };
        if !ptr.is_null() {
            add_live(layout.size());
        }
        ptr
    }

    unsafe fn alloc_zeroed(&self, layout: Layout) -> *mut u8 {
        let ptr = unsafe { System.alloc_zeroed(layout) };
        if !ptr.is_null() {
            add_live(layout.size());
        }
        ptr
    }

    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        unsafe { System.dealloc(ptr, layout) };
        subtract_live(layout.size());
    }

    unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
        let replacement = unsafe { System.realloc(ptr, layout, new_size) };
        if !replacement.is_null() {
            if new_size >= layout.size() {
                add_live(new_size - layout.size());
            } else {
                subtract_live(layout.size() - new_size);
            }
        }
        replacement
    }
}

#[global_allocator]
static ALLOCATOR: CountingAllocator = CountingAllocator;

fn reset_peak() -> usize {
    let current = LIVE.load(Ordering::SeqCst);
    PEAK.store(current, Ordering::SeqCst);
    current
}

fn signed_delta(current: usize, baseline: usize) -> i64 {
    current as i64 - baseline as i64
}

fn done_response() -> Vec<u8> {
    let body = concat!(
        "data: {\"model\":\"fixture/actual\",\"choices\":[{\"delta\":{\"content\":\"ok\"},\"finish_reason\":\"stop\"}],",
        "\"usage\":{\"cost\":0.001,\"prompt_tokens\":1,\"completion_tokens\":1,\"prompt_tokens_details\":{\"cached_tokens\":0}}}\n\n",
        "data: [DONE]\n\n"
    );
    http_response("200 OK", "text/event-stream", body)
}

fn busy_response() -> Vec<u8> {
    http_response(
        "429 Too Many Requests",
        "application/json",
        r#"{"error":{"metadata":{"retry_after_seconds":0}}}"#,
    )
}

fn http_response(status: &str, content_type: &str, body: &str) -> Vec<u8> {
    format!(
        "HTTP/1.1 {status}\r\nContent-Type: {content_type}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    )
    .into_bytes()
}

#[derive(serde::Serialize)]
struct RequestEvidence {
    content_length: usize,
    received: usize,
    complete: bool,
    body_hash: Option<String>,
    observed_close: bool,
}

fn read_request(stream: &mut TcpStream, body_limit: usize) -> RequestEvidence {
    stream
        .set_read_timeout(Some(Duration::from_secs(5)))
        .unwrap();
    let mut bytes = Vec::new();
    let header_end = loop {
        let mut chunk = [0u8; 8192];
        let n = stream.read(&mut chunk).expect("request headers");
        assert!(n > 0, "connection closed before request headers");
        bytes.extend_from_slice(&chunk[..n]);
        assert!(bytes.len() <= 64 * 1024, "request headers exceeded bound");
        if let Some(pos) = bytes.windows(4).position(|window| window == b"\r\n\r\n") {
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
        .expect("content-length");
    assert!(
        content_length <= 8 * 1024 * 1024,
        "request body exceeded fixture bound"
    );
    let already_received = bytes.len() - header_end;
    let target = content_length.min(body_limit.max(already_received));
    let mut body = bytes[header_end..].to_vec();
    body.truncate(target);
    while body.len() < target {
        let mut chunk = [0u8; 8192];
        let wanted = (target - body.len()).min(chunk.len());
        let n = stream.read(&mut chunk[..wanted]).unwrap_or(0);
        if n == 0 {
            break;
        }
        body.extend_from_slice(&chunk[..n]);
    }
    let complete = body.len() == content_length;
    RequestEvidence {
        content_length,
        received: body.len(),
        complete,
        body_hash: complete.then(|| hex::encode(Sha256::digest(&body))),
        observed_close: false,
    }
}

fn drain_after_cancel(stream: &mut TcpStream, evidence: &mut RequestEvidence) {
    let deadline = Instant::now() + Duration::from_secs(3);
    let mut total = evidence.received;
    let mut observed_close = false;
    while !observed_close {
        let remaining = deadline.saturating_duration_since(Instant::now());
        assert!(
            !remaining.is_zero(),
            "upload drain exceeded absolute deadline"
        );
        stream.set_read_timeout(Some(remaining)).unwrap();
        let mut chunk = [0u8; 8192];
        let wanted = evidence
            .content_length
            .saturating_sub(total)
            .min(chunk.len())
            .max(1);
        match stream.read(&mut chunk[..wanted]) {
            Ok(0) => {
                observed_close = true;
                break;
            }
            Ok(read) => {
                total += read;
                assert!(
                    total <= evidence.content_length,
                    "recorder received beyond Content-Length"
                );
            }
            Err(error)
                if matches!(
                    error.kind(),
                    std::io::ErrorKind::ConnectionReset
                        | std::io::ErrorKind::ConnectionAborted
                        | std::io::ErrorKind::BrokenPipe
                        | std::io::ErrorKind::UnexpectedEof
                ) =>
            {
                observed_close = true;
                break;
            }
            Err(error)
                if matches!(
                    error.kind(),
                    std::io::ErrorKind::TimedOut | std::io::ErrorKind::WouldBlock
                ) =>
            {
                panic!("upload drain reached its absolute deadline before EOF/reset")
            }
            Err(error) => panic!("upload drain failed: {error}"),
        }
    }
    evidence.received = total;
    evidence.complete = total == evidence.content_length;
    evidence.observed_close = observed_close;
}
fn accept_complete(listener: &TcpListener, response: &[u8]) -> RequestEvidence {
    let (mut stream, _) = listener.accept().expect("fixture accept");
    let evidence = read_request(&mut stream, usize::MAX);
    stream.write_all(response).expect("fixture response");
    stream.flush().expect("fixture flush");
    evidence
}

fn wait_for_peer_close(stream: &mut TcpStream) -> bool {
    stream
        .set_read_timeout(Some(Duration::from_secs(5)))
        .unwrap();
    let mut byte = [0u8; 1];
    match stream.read(&mut byte) {
        Ok(0) => true,
        Err(error)
            if matches!(
                error.kind(),
                std::io::ErrorKind::ConnectionReset
                    | std::io::ErrorKind::ConnectionAborted
                    | std::io::ErrorKind::BrokenPipe
                    | std::io::ErrorKind::UnexpectedEof
            ) =>
        {
            true
        }
        Ok(_) => panic!("unexpected pipelined request byte while waiting for cancellation"),
        Err(error) if error.kind() == std::io::ErrorKind::TimedOut => false,
        Err(error) => panic!("waiting for client cancellation failed: {error}"),
    }
}
fn signal(control: &mut TcpStream, marker: u8) {
    control.write_all(&[marker]).unwrap();
    control.flush().unwrap();
}

fn run_recorder(scenario: &str) {
    let listener = TcpListener::bind("127.0.0.1:0").expect("fixture bind");
    let control_listener = TcpListener::bind("127.0.0.1:0").expect("control bind");
    SockRef::from(&listener)
        .set_recv_buffer_size(4096)
        .expect("bounded fixture receive window");
    let port = listener.local_addr().unwrap().port();
    let control_port = control_listener.local_addr().unwrap().port();
    println!("S8D_RECORDER_READY {port} {control_port}");
    std::io::stdout().flush().unwrap();

    let mut evidence = vec![accept_complete(&listener, &done_response())];
    match scenario {
        "success" => evidence.push(accept_complete(&listener, &done_response())),
        "retry" => {
            evidence.push(accept_complete(&listener, &busy_response()));
            evidence.push(accept_complete(&listener, &busy_response()));
            evidence.push(accept_complete(&listener, &done_response()));
        }
        "stalled_response" => {
            let mut control = control_listener.accept().expect("response control").0;
            let (mut stream, _) = listener.accept().expect("stalled response accept");
            evidence.push(read_request(&mut stream, usize::MAX));
            stream
                .write_all(b"HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nConnection: close\r\n\r\n")
                .unwrap();
            stream.flush().unwrap();
            signal(&mut control, b'1');
            assert!(
                wait_for_peer_close(&mut stream),
                "response-stall client did not cancel"
            );
        }
        "cancel_upload" | "cancel_after_prefix" => {
            let mut control = control_listener.accept().expect("upload control").0;
            let (mut stream, _) = listener.accept().expect("upload accept");
            SockRef::from(&stream)
                .set_recv_buffer_size(4096)
                .expect("bounded upload receive window");
            let mut item = read_request(&mut stream, UPLOAD_PREFIX_BYTES);
            assert!(
                item.received > 0,
                "upload must have begun before cancellation readiness"
            );
            assert!(
                item.received < item.content_length,
                "upload must remain incomplete before cancellation readiness"
            );
            signal(&mut control, b'1');
            control
                .set_read_timeout(Some(Duration::from_secs(5)))
                .unwrap();
            let mut cancelled = [0u8; 1];
            control
                .read_exact(&mut cancelled)
                .expect("client cancellation acknowledgement");
            assert_eq!(cancelled, *b"0");
            drain_after_cancel(&mut stream, &mut item);
            assert!(
                item.observed_close,
                "upload cancellation did not produce EOF/reset"
            );
            assert!(
                item.received > 0,
                "recorder did not consume an upload prefix"
            );
            if scenario == "cancel_upload" {
                assert!(
                    item.received < item.content_length,
                    "Linux upload completed before cancellation"
                );
            }
            signal(&mut control, b'2');
            evidence.push(item);
        }
        "concurrent_stalled" => {
            let controls = [
                control_listener.accept().expect("first control").0,
                control_listener.accept().expect("second control").0,
            ];
            let streams = [
                listener.accept().expect("first concurrent accept").0,
                listener.accept().expect("second concurrent accept").0,
            ];
            let handles = streams
                .into_iter()
                .zip(controls)
                .map(|(mut stream, mut control)| {
                    thread::spawn(move || {
                        let item = read_request(&mut stream, usize::MAX);
                        stream
                            .write_all(b"HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nConnection: close\r\n\r\n")
                            .unwrap();
                        stream.flush().unwrap();
                        signal(&mut control, b'1');
                        assert!(
                            wait_for_peer_close(&mut stream),
                            "concurrent response-stall client did not cancel"
                        );
                        item
                    })
                })
                .collect::<Vec<_>>();
            for handle in handles {
                evidence.push(handle.join().unwrap());
            }
        }
        other => panic!("unknown recorder scenario {other}"),
    }

    let image = &evidence[1..];
    let hashes_equal = image
        .first()
        .and_then(|first| first.body_hash.as_ref())
        .is_some_and(|hash| {
            image
                .iter()
                .all(|item| item.body_hash.as_ref() == Some(hash))
        });
    println!(
        "S8D_RECORDER_SUMMARY {}",
        serde_json::json!({
            "scenario": scenario,
            "request_count": evidence.len(),
            "image_content_lengths": image.iter().map(|item| item.content_length).collect::<Vec<_>>(),
            "image_received": image.iter().map(|item| item.received).collect::<Vec<_>>(),
            "image_complete": image.iter().map(|item| item.complete).collect::<Vec<_>>(),
            "image_observed_close": image.iter().map(|item| item.observed_close).collect::<Vec<_>>(),
            "image_hashes_equal": hashes_equal,
        })
    );
}

fn canonical_messages() -> Vec<ModelMessage> {
    vec![
        ModelMessage::user("inspect the screen"),
        ModelMessage {
            role: "assistant".into(),
            content: MessageContent::Text(String::new()),
            tool_calls: Some(vec![MessageToolCall {
                id: "snap-call".into(),
                kind: "function".into(),
                function: FunctionCall {
                    name: "snap_desk".into(),
                    arguments: "{}".into(),
                },
            }]),
            tool_call_id: None,
        },
        ModelMessage {
            role: "tool".into(),
            content: MessageContent::Text("Captured observation fixture".into()),
            tool_calls: None,
            tool_call_id: Some("snap-call".into()),
        },
    ]
}

fn image_request(
    messages: &[ModelMessage],
    observation: &server::observations::ScreenObservation,
) -> ModelRequest {
    let mut request_messages = messages.to_vec();
    request_messages.push(screen_observation_request_message(observation));
    ModelRequest {
        model: "vision/model".into(),
        messages: request_messages,
        ..Default::default()
    }
}

async fn drain(port: &OpenRouterPort, request: ModelRequest) {
    let mut stream = port.stream(request);
    tokio::time::timeout(Duration::from_secs(5), async {
        while stream.next().await.is_some() {}
    })
    .await
    .expect("model stream settled");
    drop(stream);
}

async fn wait_for_recorder(control: &mut tokio::net::TcpStream, expected: u8) {
    let mut marker = [0u8; 1];
    tokio::time::timeout(Duration::from_secs(5), control.read_exact(&mut marker))
        .await
        .expect("recorder state signal timed out")
        .expect("recorder state signal closed");
    assert_eq!(marker, [expected]);
}
async fn run_measurement(scenario: &str, endpoint: String, control: String) {
    let port = Arc::new(
        OpenRouterPort::with_local_endpoint(KeySource::Inline("fixture-key".into()), endpoint)
            .unwrap(),
    );
    drain(
        &port,
        ModelRequest {
            model: "vision/model".into(),
            messages: vec![ModelMessage::user("warm transport")],
            ..Default::default()
        },
    )
    .await;

    let warmed_current = LIVE.load(Ordering::SeqCst);
    let admission = Arc::new(ObservationAdmission::new());
    let mut frames = Vec::new();
    for index in 0..4 {
        let reservation = admission.try_begin_capture().expect("retained reservation");
        frames.push(reservation.retain(
            CapturedFrame {
                png: vec![index as u8; FRAME_BYTES],
                width: 4096,
                height: 1024,
            },
            format!("run-{index}"),
            format!("bot-{index}"),
            format!("observation-{index}"),
            "2026-09-18T12:00:00Z",
            0,
        ));
    }
    let retained_current = LIVE.load(Ordering::SeqCst);
    assert!(
        retained_current.saturating_sub(warmed_current) >= 4 * FRAME_BYTES,
        "four retained fixtures did not allocate four production-max frame buffers"
    );
    let snapshot = admission.snapshot();
    assert_eq!(snapshot.retained_in_use, 4);
    assert_eq!(snapshot.capture_decode_in_use, 0);

    let messages = canonical_messages();
    let control_count = match scenario {
        "stalled_response" | "cancel_upload" | "cancel_after_prefix" => 1,
        "concurrent_stalled" => 2,
        _ => 0,
    };
    let mut controls = Vec::new();
    for _ in 0..control_count {
        controls.push(
            tokio::time::timeout(
                Duration::from_secs(5),
                tokio::net::TcpStream::connect(&control),
            )
            .await
            .expect("control connection timed out")
            .expect("control connection failed"),
        );
    }
    let dispatch_count = usize::from(scenario == "concurrent_stalled") + 1;
    let mut dispatch_permits = Vec::new();
    for _ in 0..dispatch_count {
        dispatch_permits.push(
            admission
                .try_begin_dispatch()
                .expect("dispatch reservation"),
        );
    }
    assert_eq!(admission.snapshot().dispatch_in_use, dispatch_count);
    let baseline = reset_peak();
    let mut stalled_cancel_cleanup_current = None;
    let mut stalled_cancel_cleanup_latency_ms = None;

    let active_current = match scenario {
        "success" | "retry" => {
            drain(&port, image_request(&messages, &frames[0])).await;
            LIVE.load(Ordering::SeqCst)
        }
        "stalled_response" | "cancel_upload" | "cancel_after_prefix" => {
            let request = image_request(&messages, &frames[0]);
            let task_port = Arc::clone(&port);
            let task = tokio::spawn(async move { drain(&task_port, request).await });
            wait_for_recorder(&mut controls[0], b'1').await;
            assert!(
                !task.is_finished(),
                "synchronized stalled request completed unexpectedly"
            );
            let current = LIVE.load(Ordering::SeqCst);
            task.abort();
            let cancelled = task
                .await
                .expect_err("aborted request task returned successfully");
            assert!(
                cancelled.is_cancelled(),
                "request task failed instead of cancelling"
            );
            if matches!(scenario, "cancel_upload" | "cancel_after_prefix") {
                dispatch_permits.clear();
                frames.clear();
                let released = admission.snapshot();
                assert_eq!(released.retained_in_use, 0);
                assert_eq!(released.capture_decode_in_use, 0);
                assert_eq!(released.dispatch_in_use, 0);
                let cleanup_started = Instant::now();
                let cleanup_deadline = cleanup_started + Duration::from_secs(1);
                let cleanup_current = loop {
                    let current = LIVE.load(Ordering::SeqCst);
                    if current <= warmed_current + CLEANUP_TOLERANCE {
                        break current;
                    }
                    assert!(
                        Instant::now() < cleanup_deadline,
                        "cancel cleanup exceeded one second while receiver was stalled: warmed={warmed_current} current={current} tolerance={CLEANUP_TOLERANCE} permits={:?}",
                        admission.snapshot()
                    );
                    tokio::time::sleep(Duration::from_millis(10)).await;
                };
                stalled_cancel_cleanup_current = Some(cleanup_current);
                stalled_cancel_cleanup_latency_ms =
                    Some(cleanup_started.elapsed().as_millis() as u64);
                controls[0]
                    .write_all(b"0")
                    .await
                    .expect("cancellation acknowledgement");
                wait_for_recorder(&mut controls[0], b'2').await;
            }
            current
        }
        "concurrent_stalled" => {
            let mut tasks = Vec::new();
            for frame in frames.iter().take(2) {
                let request = image_request(&messages, frame);
                let task_port = Arc::clone(&port);
                tasks.push(tokio::spawn(
                    async move { drain(&task_port, request).await },
                ));
            }
            let (first, second) = controls.split_at_mut(1);
            tokio::join!(
                wait_for_recorder(&mut first[0], b'1'),
                wait_for_recorder(&mut second[0], b'1')
            );
            assert!(tasks.iter().all(|task| !task.is_finished()));
            let current = LIVE.load(Ordering::SeqCst);
            for task in tasks {
                task.abort();
                let cancelled = task
                    .await
                    .expect_err("aborted request task returned successfully");
                assert!(
                    cancelled.is_cancelled(),
                    "request task failed instead of cancelling"
                );
            }
            current
        }
        other => panic!("unknown measurement scenario {other}"),
    };

    let peak = PEAK.load(Ordering::SeqCst);
    drop(controls);
    drop(dispatch_permits);
    assert_eq!(admission.snapshot().dispatch_in_use, 0);
    tokio::task::yield_now().await;
    let post_dispatch_current = LIVE.load(Ordering::SeqCst);
    drop(frames);
    drop(messages);
    let released = admission.snapshot();
    assert_eq!(released.retained_in_use, 0);
    assert_eq!(released.capture_decode_in_use, 0);
    assert_eq!(released.dispatch_in_use, 0);
    drop(admission);
    tokio::task::yield_now().await;
    let post_cleanup_current = LIVE.load(Ordering::SeqCst);
    let cleanup_tolerance = CLEANUP_TOLERANCE;
    assert!(
        post_cleanup_current <= warmed_current + cleanup_tolerance,
        "cleanup retained more than the 1 MiB runtime-pool allowance"
    );
    let budget = if dispatch_count == 2 {
        DOUBLE_BUDGET
    } else {
        SINGLE_BUDGET
    };
    println!(
        "S8D_MEMORY_MEASUREMENT {}",
        serde_json::json!({
            "scenario": scenario,
            "dispatches": dispatch_count,
            "frame_bytes_each": FRAME_BYTES,
            "frame_payload": "synthetic max-size byte buffers; not valid PNG decode fixtures",
            "warmed_current": warmed_current,
            "retained_current": retained_current,
            "retained_incremental": signed_delta(retained_current, warmed_current),
            "baseline_current": baseline,
            "peak_current": peak,
            "peak_incremental": signed_delta(peak, baseline),
            "active_current": active_current,
            "active_incremental": signed_delta(active_current, baseline),
            "cancel_cleanup_while_receiver_stalled_current": stalled_cancel_cleanup_current,
            "cancel_cleanup_while_receiver_stalled_latency_ms": stalled_cancel_cleanup_latency_ms,
            "post_dispatch_current": post_dispatch_current,
            "post_dispatch_incremental": signed_delta(post_dispatch_current, baseline),
            "post_cleanup_current": post_cleanup_current,
            "post_cleanup_incremental": signed_delta(post_cleanup_current, warmed_current),
            "cleanup_tolerance": cleanup_tolerance,
            "permits_after_cleanup": {"retained": 0, "capture_decode": 0, "dispatch": 0},
            "budget": budget,
            "within_budget": peak.saturating_sub(baseline) <= budget,
            "scope": "Raw OpenRouterPort requested heap for application image buffers over directly owned Hyper HTTP/1",
            "transport_path": "image-only directly owned Hyper HTTP/1",
            "counter_semantics": "live requested bytes; successful realloc applies the signed new-size minus old-size delta",
            "excluded": ["allocator arena overhead", "RSS", "kernel socket buffers", "recorder process", "Rustls certificate/session/record buffers", "native library allocations", "allocator-internal transient realloc overlap"],
        })
    );
}

fn wait_output(mut child: Child, timeout: Duration) -> Result<std::process::Output, String> {
    let deadline = Instant::now() + timeout;
    loop {
        match child.try_wait() {
            Ok(Some(_)) => return child.wait_with_output().map_err(|error| error.to_string()),
            Ok(None) => {}
            Err(error) => {
                let _ = child.kill();
                let output = child.wait_with_output().ok();
                return Err(format!(
                    "could not inspect measurement child: {error}; stdout: {}; stderr: {}",
                    output
                        .as_ref()
                        .map(|value| String::from_utf8_lossy(&value.stdout).into_owned())
                        .unwrap_or_default(),
                    output
                        .as_ref()
                        .map(|value| String::from_utf8_lossy(&value.stderr).into_owned())
                        .unwrap_or_default()
                ));
            }
        }
        if Instant::now() >= deadline {
            let _ = child.kill();
            let output = child
                .wait_with_output()
                .map_err(|error| format!("measurement child timeout reap failed: {error}"))?;
            return Err(format!(
                "measurement child timed out; stdout: {}; stderr: {}",
                String::from_utf8_lossy(&output.stdout),
                String::from_utf8_lossy(&output.stderr)
            ));
        }
        thread::sleep(Duration::from_millis(25));
    }
}

fn parse_prefixed(output: &str, prefix: &str) -> serde_json::Value {
    output
        .lines()
        .find_map(|line| line.find(prefix).map(|index| &line[index + prefix.len()..]))
        .map(|json| serde_json::from_str(json).unwrap())
        .unwrap_or_else(|| panic!("missing {prefix} in child output: {output}"))
}

struct RecorderChild {
    child: Child,
    lines: std::sync::mpsc::Receiver<String>,
    reader: thread::JoinHandle<()>,
    stderr: std::process::ChildStderr,
    scenario: String,
}

fn collect_recorder(
    mut recorder: RecorderChild,
    kill: bool,
) -> (std::process::ExitStatus, String, String, String) {
    if kill && recorder.child.try_wait().ok().flatten().is_none() {
        let _ = recorder.child.kill();
    }
    let status = recorder.child.wait().expect("recorder child reap");
    recorder.reader.join().expect("recorder stdout reader");
    let stdout = recorder.lines.try_iter().collect::<Vec<_>>().join("\n");
    let mut stderr = String::new();
    let _ = recorder.stderr.read_to_string(&mut stderr);
    (status, stdout, stderr, recorder.scenario)
}

fn fail_recorder(recorder: RecorderChild, context: impl std::fmt::Display) -> ! {
    let (status, stdout, stderr, scenario) = collect_recorder(recorder, true);
    panic!(
        "recorder scenario {scenario} aborted while {context}; status: {status}; stdout: {stdout}; stderr: {stderr}"
    )
}

fn start_recorder(exe: &std::path::Path, scenario: &str) -> (String, String, RecorderChild) {
    let mut child = Command::new(exe)
        .args([
            "--exact",
            "memory_fixture_process",
            "--nocapture",
            "--test-threads=1",
        ])
        .env(ROLE_ENV, "recorder")
        .env(SCENARIO_ENV, scenario)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let stdout = BufReader::new(child.stdout.take().unwrap());
    let stderr = child.stderr.take().unwrap();
    let (sender, lines) = std::sync::mpsc::channel();
    let reader = thread::spawn(move || {
        for line in stdout.lines() {
            if sender.send(line.unwrap()).is_err() {
                return;
            }
        }
    });
    let recorder = RecorderChild {
        child,
        lines,
        reader,
        stderr,
        scenario: scenario.to_string(),
    };
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            fail_recorder(recorder, "waiting for readiness");
        }
        let line = match recorder.lines.recv_timeout(remaining) {
            Ok(line) => line,
            Err(error) => fail_recorder(recorder, format!("waiting for readiness: {error}")),
        };
        if let Some(index) = line.find("S8D_RECORDER_READY ") {
            let ports = line[index + "S8D_RECORDER_READY ".len()..]
                .split_whitespace()
                .collect::<Vec<_>>();
            if ports.len() != 2 {
                fail_recorder(recorder, format!("parsing readiness line: {line}"));
            }
            let endpoint = format!("http://127.0.0.1:{}/api/v1/chat/completions", ports[0]);
            let control = format!("127.0.0.1:{}", ports[1]);
            return (endpoint, control, recorder);
        }
    }
}

fn finish_recorder(mut recorder: RecorderChild) -> serde_json::Value {
    let deadline = Instant::now() + Duration::from_secs(8);
    loop {
        match recorder.child.try_wait() {
            Ok(Some(_)) => break,
            Ok(None) => {}
            Err(error) => fail_recorder(recorder, format!("checking completion: {error}")),
        }
        if Instant::now() >= deadline {
            fail_recorder(recorder, "waiting for completion");
        }
        thread::sleep(Duration::from_millis(25));
    }
    let (status, stdout, stderr, scenario) = collect_recorder(recorder, false);
    assert!(
        status.success(),
        "recorder scenario {scenario} failed; status: {status}; stdout: {stdout}; stderr: {stderr}"
    );
    stdout
        .lines()
        .find_map(|line| {
            line.find("S8D_RECORDER_SUMMARY ")
                .map(|index| &line[index + "S8D_RECORDER_SUMMARY ".len()..])
        })
        .map(|json| serde_json::from_str(json).unwrap())
        .unwrap_or_else(|| {
            panic!(
                "recorder scenario {scenario} omitted summary; stdout: {stdout}; stderr: {stderr}"
            )
        })
}

#[test]
fn memory_fixture_process() {
    let Ok(role) = std::env::var(ROLE_ENV) else {
        return;
    };
    let scenario = std::env::var(SCENARIO_ENV).unwrap();
    if role == "recorder" {
        run_recorder(&scenario);
    } else if role == "measure" {
        let endpoint = std::env::var(ENDPOINT_ENV).unwrap();
        let control = std::env::var(CONTROL_ENV).unwrap();
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap()
            .block_on(run_measurement(&scenario, endpoint, control));
    } else {
        panic!("unknown fixture role {role}");
    }
}

#[test]
fn application_image_buffer_memory_acceptance() {
    if std::env::var_os(ROLE_ENV).is_some() {
        return;
    }
    let exe = std::env::current_exe().unwrap();
    let mut measurements = Vec::new();
    let mut scenarios = vec!["success", "retry", "stalled_response", "concurrent_stalled"];
    #[cfg(windows)]
    scenarios.push("cancel_after_prefix");
    #[cfg(not(windows))]
    scenarios.push("cancel_upload");
    for scenario in scenarios {
        let (endpoint, control, recorder) = start_recorder(&exe, scenario);
        let child = match Command::new(&exe)
            .args([
                "--exact",
                "memory_fixture_process",
                "--nocapture",
                "--test-threads=1",
            ])
            .env(ROLE_ENV, "measure")
            .env(SCENARIO_ENV, scenario)
            .env(ENDPOINT_ENV, endpoint)
            .env(CONTROL_ENV, control)
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
        {
            Ok(child) => child,
            Err(error) => fail_recorder(recorder, format!("spawning measurement child: {error}")),
        };
        let output = match wait_output(child, Duration::from_secs(12)) {
            Ok(output) => output,
            Err(error) => {
                fail_recorder(recorder, format!("waiting for measurement child: {error}"))
            }
        };
        if !output.status.success() {
            fail_recorder(
                recorder,
                format!(
                    "measurement child failed; status: {}; stdout: {}; stderr: {}",
                    output.status,
                    String::from_utf8_lossy(&output.stdout),
                    String::from_utf8_lossy(&output.stderr)
                ),
            );
        }
        let wire = finish_recorder(recorder);
        let stdout = String::from_utf8(output.stdout).unwrap();
        let measurement = parse_prefixed(&stdout, "S8D_MEMORY_MEASUREMENT ");
        assert_eq!(measurement["scenario"], scenario);
        assert_eq!(measurement["within_budget"], true, "{measurement}");
        assert_eq!(wire["scenario"], scenario);
        let expected_requests = match scenario {
            "retry" => 4,
            "concurrent_stalled" => 3,
            _ => 2,
        };
        assert_eq!(wire["request_count"], expected_requests);
        if scenario == "retry" {
            assert_eq!(wire["image_hashes_equal"], true);
        }
        if matches!(scenario, "cancel_upload" | "cancel_after_prefix") {
            assert_eq!(wire["image_observed_close"][0], true);
            let received = wire["image_received"][0].as_u64().unwrap();
            let content_length = wire["image_content_lengths"][0].as_u64().unwrap();
            assert!(received > 0, "recorder did not consume an upload prefix");
            if scenario == "cancel_upload" {
                assert_eq!(wire["image_complete"][0], false);
                assert!(
                    received < content_length,
                    "Linux cancellation happened after full upload"
                );
            } else {
                assert!(
                    received <= content_length,
                    "Windows recorder received beyond Content-Length"
                );
            }
        } else {
            assert!(
                wire["image_complete"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .all(|v| v == true)
            );
        }
        measurements.push(serde_json::json!({"memory": measurement, "wire": wire}));
    }
    println!(
        "S8D_MEMORY_SUMMARY {}",
        serde_json::Value::Array(measurements)
    );
}
