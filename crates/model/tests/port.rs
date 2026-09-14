//! S1-02 acceptance: Rust twins of the captured-response cases in the TS
//! original (`test/wire-format.test.ts`, `test/model-port-retry.test.ts`,
//! and the tool-call fragment test in `test/memory.test.ts` - there is no
//! `test/model-port.test.ts` in `bullpen-night`; those three files are its
//! actual coverage for `parseSseStream`/`busyWaitMs`/`redact`). Fixtures are
//! byte-for-byte copies under `tests/fixtures/`, captured real OpenRouter SSE
//! bodies, never hand-written.
//!
//! No test here performs network I/O - every stream is built from an
//! in-memory byte fixture, never a live `reqwest` call.

use futures::stream::{self, StreamExt};
use model::{ModelEvent, ModelUsage, busy_wait_ms, parse_sse_stream, redact};

const STREAM_FIXTURE: &str = include_str!("fixtures/openrouter-stream.txt");
const TOOLCALL_FIXTURE: &str = include_str!("fixtures/openrouter-toolcall.txt");

/// Splits `text` into byte chunks of `size` (the last one shorter), feeds
/// them through `parse_sse_stream`, and collects every event. `size` only
/// controls how awkwardly the network is simulated to have split the
/// frames - never the parsed result, since the parser is pure over the
/// concatenated bytes.
async fn parse_chunked(text: &str, model: &str, size: usize) -> Vec<ModelEvent> {
    let bytes = text.as_bytes();
    let chunks: Vec<Result<Vec<u8>, String>> = bytes
        .chunks(size.max(1))
        .map(|c| Ok::<Vec<u8>, String>(c.to_vec()))
        .collect();
    let body = stream::iter(chunks);
    parse_sse_stream(body, model.to_string(), None)
        .collect()
        .await
}

fn deltas(events: &[ModelEvent]) -> String {
    events
        .iter()
        .filter_map(|e| match e {
            ModelEvent::Delta { text } => Some(text.as_str()),
            _ => None,
        })
        .collect()
}

// ---------------------------------------------------------------- 1. parsing

#[tokio::test]
async fn assembles_the_visible_answer_and_nothing_else() {
    // TEST 1 for the bite check: break the frame splitter on a multi-line
    // `data:` payload and this goes red.
    let events = parse_chunked(STREAM_FIXTURE, "requested/model", 97).await;
    assert_eq!(deltas(&events), "pen is warm");
}

#[tokio::test]
async fn does_not_leak_reasoning_tokens_into_the_answer() {
    // The captured response carries 42 frames of `delta.reasoning`. None of
    // it is part of what the user asked for.
    assert!(STREAM_FIXTURE.contains("\"reasoning\""));
    let events = parse_chunked(STREAM_FIXTURE, "requested/model", 97).await;
    let text = deltas(&events);
    assert!(!text.contains("User says"));
    assert!(text.len() < 40);
}

#[tokio::test]
async fn reports_the_model_the_provider_actually_used_not_the_one_requested() {
    let events = parse_chunked(STREAM_FIXTURE, "requested/model", 97).await;
    match events.last() {
        Some(ModelEvent::Done { model, .. }) => assert_eq!(model, "openai/gpt-oss-120b"),
        other => panic!("expected a Done event, got {other:?}"),
    }
}

#[tokio::test]
async fn carries_the_providers_own_cost_out_of_the_stream() {
    // Values read from the recorded response, never from running our own
    // code. The provider bills an exact figure; Bullpen must never estimate
    // one.
    let events = parse_chunked(STREAM_FIXTURE, "requested/model", 97).await;
    match events.last() {
        Some(ModelEvent::Done {
            usage:
                Some(ModelUsage {
                    cost_usd,
                    input_tokens,
                    output_tokens,
                    cached_tokens,
                }),
            ..
        }) => {
            assert!((cost_usd - 2.435e-5).abs() < 1e-9);
            assert_eq!(*input_tokens, 82);
            assert_eq!(*output_tokens, 45);
            assert_eq!(*cached_tokens, 0);
        }
        other => panic!("expected a Done event with usage, got {other:?}"),
    }
}

#[tokio::test]
async fn emits_exactly_one_terminal_event() {
    let events = parse_chunked(STREAM_FIXTURE, "requested/model", 97).await;
    let terminal = events
        .iter()
        .filter(|e| matches!(e, ModelEvent::Done { .. } | ModelEvent::Error { .. }))
        .count();
    assert_eq!(terminal, 1);
}

#[tokio::test]
async fn survives_frames_split_across_chunk_boundaries_one_byte_at_a_time() {
    let events = parse_chunked(STREAM_FIXTURE, "requested/model", 1).await;
    assert_eq!(deltas(&events), "pen is warm");
}

#[tokio::test]
async fn surfaces_an_error_frame_instead_of_treating_it_as_content() {
    let error_frame = "data: {\"error\":{\"message\":\"only available through the Batch API\",\"code\":404}}\n\ndata: [DONE]\n";
    let events = parse_chunked(error_frame, "requested/model", 97).await;
    assert_eq!(
        events,
        vec![ModelEvent::Error {
            message: "only available through the Batch API".to_string(),
            status: None
        }]
    );
}

#[tokio::test]
async fn ignores_openrouters_keep_alive_comment_lines() {
    let with_comments = format!(": OPENROUTER PROCESSING\n\n{STREAM_FIXTURE}");
    let events = parse_chunked(&with_comments, "requested/model", 97).await;
    assert_eq!(deltas(&events), "pen is warm");
}

#[tokio::test]
async fn reassembles_tool_call_arguments_that_arrived_in_fragments() {
    // Recorded from OpenRouter, not written by hand. The live response
    // splits the JSON arguments across many frames, keyed by index, with
    // only the first frame carrying the id and the name.
    let events = parse_chunked(TOOLCALL_FIXTURE, "requested/model", 61).await;

    let last = events.last();
    let calls = match last {
        Some(ModelEvent::ToolCalls { calls, .. }) => calls,
        other => panic!("expected a ToolCalls event, got {other:?}"),
    };
    assert_eq!(calls.len(), 1);
    assert_eq!(calls[0].name, "search_memory");
    assert!(calls[0].id.starts_with("chatcmpl-tool-"));

    // The whole point: the arguments must parse. A parser that took one
    // frame would hand back a truncated fragment that fails to parse here.
    let args: serde_json::Value =
        serde_json::from_str(&calls[0].arguments).expect("tool call arguments must be valid JSON");
    let query = args
        .get("query")
        .and_then(|q| q.as_str())
        .expect("query must be a string");
    assert!(query.to_lowercase().contains("zenith"));
}

// ----------------------------------------------------------- 2. busy_wait_ms

#[test]
fn busy_wait_ms_reads_the_retry_after_header() {
    assert_eq!(busy_wait_ms(429, "not json", Some("2")), 2_000);
}

#[test]
fn busy_wait_ms_reads_the_body_hint_over_the_header() {
    let body = r#"{"error":{"metadata":{"retry_after_seconds":3}}}"#;
    assert_eq!(busy_wait_ms(429, body, Some("99")), 3_000);
}

#[test]
fn busy_wait_ms_defaults_to_one_second_when_neither_says() {
    assert_eq!(busy_wait_ms(429, "", None), 1_000);
}

#[test]
fn busy_wait_ms_is_zero_off_a_429() {
    assert_eq!(
        busy_wait_ms(
            500,
            r#"{"error":{"metadata":{"retry_after_seconds":9}}}"#,
            Some("9")
        ),
        0
    );
}

#[test]
fn busy_wait_ms_never_exceeds_the_five_second_cap() {
    let body = r#"{"error":{"metadata":{"retry_after_seconds":9999}}}"#;
    assert_eq!(busy_wait_ms(429, body, None), 5_000);
}

// ------------------------------------------------------------------ 3. redact

#[test]
fn redact_never_returns_the_configured_key() {
    let key = "sk-or-v1-abc123456789";
    let text = format!("Authorization: Bearer {key}");
    let out = redact(&text, Some(key));
    assert!(!out.contains(key));
}

#[test]
fn redact_strips_a_key_shaped_token_even_when_no_key_is_configured() {
    // Belt and braces: an upstream error body can carry a token that is not
    // ours, and it must still not reach a log or client.
    let out = redact("upstream said: sk-or-v1-someoneelsestoken", None);
    assert!(!out.contains("sk-or-v1-someoneelsestoken"));
}

#[test]
fn an_error_built_from_a_body_containing_the_key_does_not_contain_it() {
    let key = "sk-or-v1-do-not-leak-this-0001";
    let upstream_body = format!(r#"{{"error":"upstream rejected key {key}"}}"#);
    let message = redact(
        &format!("OpenRouter returned 401: {upstream_body}"),
        Some(key),
    );
    assert!(!message.contains(key));
}

// 4. No network I/O: every event stream above is built from an in-memory
// byte fixture via `futures::stream::iter`, never a live web request - see
// the ticket's Result section for the grep that confirms it.
