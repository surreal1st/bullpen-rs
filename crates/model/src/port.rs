//! THE model boundary. Every model call in Bullpen goes through this
//! interface and nothing else talks to OpenRouter. Tests replace it with
//! `fake::FakePort`, driven by a captured real response. Port of
//! `src/server/model-port.ts`.

use std::collections::BTreeMap;
use std::pin::Pin;
use std::time::Duration;

use futures::stream::{Stream, StreamExt};
use serde::{Deserialize, Serialize};

use crate::secrets::{KeySource, redact};

/// A stream of [`ModelEvent`]s. Dropping it cancels the call in flight - the
/// Rust equivalent of the TS original's `AbortSignal` parameter, which this
/// port has no separate argument for.
pub type EventStream = Pin<Box<dyn Stream<Item = ModelEvent> + Send>>;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ToolCall {
    pub id: String,
    pub name: String,
    /// Raw JSON text as the model produced it. Parsed by the caller.
    pub arguments: String,
}

/// A message is usually text. It becomes parts only when an image travels
/// with it, which is the shape OpenRouter expects for a vision model.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum MessageContent {
    Text(String),
    Parts(Vec<ContentPart>),
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ContentPart {
    Text { text: String },
    ImageUrl { image_url: ImageUrl },
}

#[derive(Clone, PartialEq, Serialize, Deserialize)]
pub struct ImageUrl {
    pub url: String,
}

impl std::fmt::Debug for ImageUrl {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ImageUrl")
            .field("url", &"<redacted image data>")
            .finish()
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct FunctionCall {
    pub name: String,
    pub arguments: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct MessageToolCall {
    pub id: String,
    #[serde(rename = "type")]
    pub kind: String,
    pub function: FunctionCall,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ModelMessage {
    pub role: String,
    pub content: MessageContent,
    /// Set on an assistant turn that asked for tools.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tool_calls: Option<Vec<MessageToolCall>>,
    /// Set on a tool result turn, matching the call it answers.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tool_call_id: Option<String>,
}

impl ModelMessage {
    pub fn system(text: impl Into<String>) -> Self {
        Self {
            role: "system".into(),
            content: MessageContent::Text(text.into()),
            tool_calls: None,
            tool_call_id: None,
        }
    }

    pub fn user(text: impl Into<String>) -> Self {
        Self {
            role: "user".into(),
            content: MessageContent::Text(text.into()),
            tool_calls: None,
            tool_call_id: None,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ToolSpec {
    pub name: String,
    pub description: String,
    /// JSON Schema for the arguments object.
    pub parameters: serde_json::Value,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Reasoning {
    pub effort: String, // "low" | "medium" | "high"
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ModelRequest {
    pub model: String,
    pub messages: Vec<ModelMessage>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tools: Option<Vec<ToolSpec>>,
    /// M3: OpenRouter's reasoning weight. The caller decides whether to set
    /// this at all - it checks the catalogue for `supportsReasoning` before
    /// ever building one of these - so this boundary does exactly one thing
    /// with it: put it on the wire when present, leave it off when not.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reasoning: Option<Reasoning>,
    /// The most output tokens this completion may produce, forwarded to
    /// OpenRouter as `max_tokens`. `None` falls back to `MAX_OUTPUT_TOKENS`,
    /// so nothing reaches the wire uncapped.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub max_output_tokens: Option<u32>,
}

/// Provider-reported usage. Never computed here.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ModelUsage {
    /// Exact dollars for this request, as OpenRouter billed it.
    pub cost_usd: f64,
    pub input_tokens: u32,
    pub output_tokens: u32,
    /// Prompt tokens served from the provider's cache.
    pub cached_tokens: u32,
}

#[derive(Debug, Clone, PartialEq)]
pub enum ModelEvent {
    Delta {
        text: String,
    },
    /// The model wants tools run. The caller executes them and calls back.
    ToolCalls {
        calls: Vec<ToolCall>,
        usage: Option<ModelUsage>,
    },
    /// `finish_reason` is the provider's raw `finish_reason` for the step
    /// that ended the turn with no tool call - "stop" on an ordinary answer,
    /// something else (Gemini's `MALFORMED_FUNCTION_CALL` among them) when
    /// the model went quiet instead. `None` only when a frame never carried
    /// one at all, which normal OpenRouter traffic does not do.
    Done {
        model: String,
        usage: Option<ModelUsage>,
        finish_reason: Option<String>,
    },
    Error {
        message: String,
        status: Option<u16>,
    },
}

pub trait ModelPort: Send + Sync {
    fn stream(&self, request: ModelRequest) -> EventStream;
}

/// Messages for a UTILITY call: one whose output no one reads as prose and
/// that offers no tools - a keyword list, a classification. A utility call
/// is not a bot run; anything that produces text Josh reads goes through the
/// server crate's prompt-building, not this.
pub fn utility_messages(
    instruction: impl Into<String>,
    input: impl Into<String>,
) -> Vec<ModelMessage> {
    vec![ModelMessage::system(instruction), ModelMessage::user(input)]
}

/// The cheap default. Anything unconfigured lands here, never on a premium
/// model. See `model-port.ts` for the model-choice rationale; this is a
/// straight constant port.
pub const CHEAP_DEFAULT_MODEL: &str = "google/gemini-2.5-flash-lite";

/// What reads an image when the bot's own model cannot.
pub const VISION_DEFAULT_MODEL: &str = "google/gemini-2.5-flash-lite";

/// The most output tokens any single completion may produce, sent as
/// OpenRouter's `max_tokens` on every request that does not name its own.
pub const MAX_OUTPUT_TOKENS: u32 = 8_000;

/// Where a busy model's call goes after three 429s, in order. The first
/// entry that is not the busy model is used, so a busy default still has
/// somewhere to go.
pub const CHEAP_FALLBACK_MODELS: [&str; 2] =
    ["google/gemini-2.5-flash-lite", "openai/gpt-oss-120b"];

const ENDPOINT: &str = "https://openrouter.ai/api/v1/chat/completions";

/// Tries on the requested model before the fallback gets one.
pub(crate) const BUSY_TRIES: usize = 3;
/// The longest a single wait may be, whatever the provider asks.
const BUSY_WAIT_CAP_MS: u64 = 5_000;

/// The milliseconds a 429 asks for, from the body OpenRouter forwards
/// (`error.metadata.retry_after_seconds`) or a `Retry-After` header. One
/// second when neither says. Never longer than `BUSY_WAIT_CAP_MS`. Zero for
/// anything that is not a 429.
pub fn busy_wait_ms(status: u16, body: &str, retry_after_header: Option<&str>) -> u64 {
    if status != 429 {
        return 0;
    }
    let mut seconds: Option<f64> = None;
    if let Ok(parsed) = serde_json::from_str::<serde_json::Value>(body)
        && let Some(raw) = parsed
            .get("error")
            .and_then(|e| e.get("metadata"))
            .and_then(|m| m.get("retry_after_seconds"))
        && let Some(n) = raw.as_f64()
        && n.is_finite()
    {
        seconds = Some(n);
    }
    if seconds.is_none()
        && let Some(header) = retry_after_header
        && let Ok(n) = header.parse::<f64>()
        && n.is_finite()
    {
        seconds = Some(n);
    }
    let ms = (seconds.unwrap_or(1.0) * 1000.0).max(0.0) as u64;
    ms.min(BUSY_WAIT_CAP_MS)
}

/// An image turn cannot move to a text-only model; it waits out the 429
/// instead.
fn carries_image(request: &ModelRequest) -> bool {
    request.messages.iter().any(|m| match &m.content {
        MessageContent::Parts(parts) => parts
            .iter()
            .any(|p| matches!(p, ContentPart::ImageUrl { .. })),
        MessageContent::Text(_) => false,
    })
}

fn fallback_for(request: &ModelRequest) -> Option<&'static str> {
    if carries_image(request) {
        return None;
    }
    CHEAP_FALLBACK_MODELS
        .iter()
        .find(|&&m| m != request.model)
        .copied()
}

/// The live OpenRouter port: streams a chat completion, retries a busy
/// model up to `BUSY_TRIES` times honouring the provider's requested wait,
/// then falls back to a cheap model once. `done.model` names what actually
/// answered, so a fallback shows in the trace instead of hiding behind the
/// caller's pin.
#[derive(Clone)]
pub struct OpenRouterPort {
    client: reqwest::Client,
    key_source: KeySource,
    endpoint: String,
    #[cfg(feature = "fake")]
    test_tls_root: Option<Vec<u8>>,
}

impl OpenRouterPort {
    pub fn new(key_source: KeySource) -> Self {
        Self {
            client: Self::client(),
            key_source,
            endpoint: ENDPOINT.to_string(),
            #[cfg(feature = "fake")]
            test_tls_root: None,
        }
    }

    fn client() -> reqwest::Client {
        reqwest::Client::builder()
            .connect_timeout(Duration::from_secs(10))
            .timeout(Duration::from_secs(120))
            .build()
            .expect("Client builder failed")
    }

    #[cfg(feature = "fake")]
    fn validate_local_endpoint(endpoint: &str, scheme: &str) -> Result<(), String> {
        let parsed = reqwest::Url::parse(endpoint).map_err(|err| err.to_string())?;
        let local = parsed.host_str().is_some_and(|host| {
            host.eq_ignore_ascii_case("localhost")
                || host
                    .parse::<std::net::IpAddr>()
                    .is_ok_and(|address| address.is_loopback())
        });
        if parsed.scheme() != scheme || !local {
            return Err(format!(
                "test model endpoint must be an {scheme} loopback address"
            ));
        }
        Ok(())
    }

    #[cfg(feature = "fake")]
    pub fn with_local_endpoint(
        key_source: KeySource,
        endpoint: impl Into<String>,
    ) -> Result<Self, String> {
        let endpoint = endpoint.into();
        Self::validate_local_endpoint(&endpoint, "http")?;
        Ok(Self {
            client: Self::client(),
            key_source,
            endpoint,
            test_tls_root: None,
        })
    }

    #[cfg(feature = "fake")]
    pub fn with_local_https_endpoint(
        key_source: KeySource,
        endpoint: impl Into<String>,
        root_der: Vec<u8>,
    ) -> Result<Self, String> {
        let endpoint = endpoint.into();
        Self::validate_local_endpoint(&endpoint, "https")?;
        let certificate = reqwest::Certificate::from_der(&root_der)
            .map_err(|_| "invalid test model trust root".to_string())?;
        let client = reqwest::Client::builder()
            .no_proxy()
            .connect_timeout(Duration::from_secs(10))
            .timeout(Duration::from_secs(120))
            .add_root_certificate(certificate)
            .build()
            .map_err(|_| "could not build test model TLS client".to_string())?;
        Ok(Self {
            client,
            key_source,
            endpoint,
            test_tls_root: Some(root_der),
        })
    }
}

impl ModelPort for OpenRouterPort {
    fn stream(&self, request: ModelRequest) -> EventStream {
        let client = self.client.clone();
        let key_source = self.key_source.clone();
        let endpoint = self.endpoint.clone();
        #[cfg(feature = "fake")]
        let test_tls_root = self.test_tls_root.clone();
        let image_request = carries_image(&request);
        if image_request {
            #[cfg(feature = "fake")]
            {
                return crate::image_transport::stream(
                    request,
                    key_source,
                    endpoint,
                    true,
                    test_tls_root,
                );
            }
            #[cfg(not(feature = "fake"))]
            {
                return crate::image_transport::stream(request, key_source, endpoint, false);
            }
        }

        Box::pin(async_stream::stream! {
            let mut attempts: Vec<String> = std::iter::repeat_n(request.model.clone(), BUSY_TRIES).collect();
            if let Some(fallback) = fallback_for(&request) {
                attempts.push(fallback.to_string());
            }

            let mut last_busy: Option<String> = None;
            let attempts_len = attempts.len();

            for (i, model) in attempts.iter().enumerate() {
                let key = match key_source.resolve() {
                    Some(k) => k,
                    None => {
                        yield ModelEvent::Error {
                            message: redact(
                                &format!("No OpenRouter key. Set {} to a file outside the repo, or {}.", crate::secrets::KEY_FILE_VAR, crate::secrets::KEY_VAR),
                                None,
                            ),
                            status: None,
                        };
                        return;
                    }
                };

                let body = build_body(&request, model);
                let request_builder = client
                    .post(&endpoint)
                    .bearer_auth(&key)
                    .header("HTTP-Referer", "https://rainmade.io")
                    .header("X-Title", "Bullpen");
                let send = request_builder.json(&body).send().await;

                let res = match send {
                    Ok(r) => r,
                    Err(e) => {
                        // S8b: the transport itself failed. This DID yield an
                        // Error event, but nothing logged it, so a run that
                        // never reached OpenRouter left no trace on the box.
                        tracing::warn!(model = %model, "model request failed before a response");
                        yield ModelEvent::Error { message: redact(&e.to_string(), Some(&key)), status: None };
                        return;
                    }
                };

                tracing::debug!(model = %model, status = res.status().as_u16(), attempt = i + 1, "model response");

                if res.status().as_u16() == 429 {
                    let retry_after = res
                        .headers()
                        .get("retry-after")
                        .and_then(|v| v.to_str().ok())
                        .map(|s| s.to_string());
                    let text = res.text().await.unwrap_or_default();
                    last_busy = Some(text.clone());
                    let is_last = i + 1 == attempts_len;
                    if !is_last {
                        let wait = busy_wait_ms(429, &text, retry_after.as_deref());
                        tokio::time::sleep(Duration::from_millis(wait)).await;
                        continue;
                    }
                    break;
                }

                if !res.status().is_success() {
                    let status = res.status().as_u16();
                    let text = res.text().await.unwrap_or_default();
                    let raw = if text.is_empty() { format!("OpenRouter returned {status}") } else { text };
                    let message = provider_error(&raw, Some(&key), image_request);
                    yield ModelEvent::Error { status: Some(status), message };
                    return;
                }

                let model = model.clone();
                let key_for_stream = key.clone();
                let byte_stream = res
                    .bytes_stream()
                    .map(move |r| r.map(|b| b.to_vec()).map_err(|e| redact(&e.to_string(), Some(&key_for_stream))));
                let mut inner = parse_sse_stream_inner(
                    byte_stream,
                    model,
                    Some(key),
                    None,
                    image_request,
                    false,
                );
                while let Some(event) = inner.next().await {
                    yield event;
                }
                return;
            }

            let tried: Vec<&str> = {
                let mut seen = Vec::new();
                for m in &attempts {
                    if !seen.contains(&m.as_str()) {
                        seen.push(m.as_str());
                    }
                }
                seen
            };
            let upstream = match &last_busy {
                Some(b) if !b.is_empty() => b.clone(),
                _ => "OpenRouter returned 429".to_string(),
            };
            let raw = format!(
                "Busy after {} tries ({}). Upstream said: {}",
                attempts_len,
                tried.join(", then "),
                upstream
            );
            let key_for_final = key_source.resolve();
            let message = provider_error(&raw, key_for_final.as_deref(), image_request);
            yield ModelEvent::Error { status: Some(429), message };
        })
    }
}

pub(crate) fn build_body(request: &ModelRequest, model: &str) -> serde_json::Value {
    let mut body = serde_json::json!({
        "model": model,
        "messages": request.messages,
        "max_tokens": request.max_output_tokens.unwrap_or(MAX_OUTPUT_TOKENS),
        "stream": true,
        // Without this OpenRouter does not return a cost, and Bullpen would
        // be left estimating a number the provider already knows exactly.
        "usage": { "include": true },
    });
    if let Some(tools) = &request.tools
        && !tools.is_empty()
    {
        let wire_tools: Vec<serde_json::Value> = tools
            .iter()
            .map(|t| {
                serde_json::json!({
                    "type": "function",
                    "function": { "name": t.name, "description": t.description, "parameters": t.parameters },
                })
            })
            .collect();
        body["tools"] = serde_json::Value::Array(wire_tools);
    }
    if let Some(r) = &request.reasoning {
        body["reasoning"] = serde_json::json!({ "effort": r.effort });
    }
    body
}

const IMAGE_PENDING_LINE_LIMIT: usize = 256 * 1024;
const IMAGE_DECODED_OUTPUT_LIMIT: usize = 1024 * 1024;
const IMAGE_PARTIAL_CALL_LIMIT: usize = 128;

#[derive(Default, Clone)]
struct PartialCall {
    id: String,
    name: String,
    args: String,
}

/// Parses OpenAI-style SSE off an arbitrary byte-chunk stream. Pure over the
/// concatenated bytes: however the input is split into chunks - a fixture
/// fed in whole, in 97-byte pieces, or one byte at a time - the resulting
/// event sequence is identical, because parsing waits for a complete `\n`
/// before decoding and acting on a line. Shared by the live port and the
/// captured-response tests, so a test that used a different parser would not
/// be testing what actually runs in production.
/// `idle_timeout` overrides the default 60-second deadline (used in tests only).
pub fn parse_sse_stream(
    body: impl Stream<Item = Result<Vec<u8>, String>> + Send + 'static,
    model: String,
    key: Option<String>,
    idle_timeout: Option<Duration>,
) -> EventStream {
    parse_sse_stream_inner(body, model, key, idle_timeout, false, false)
}

pub(crate) fn provider_error(raw: &str, key: Option<&str>, image_request: bool) -> String {
    if image_request {
        return "The model provider rejected the screen observation without exposing its payload."
            .to_string();
    }
    redact(raw, key).chars().take(2000).collect()
}

pub(crate) fn parse_sse_stream_inner<T>(
    body: impl Stream<Item = Result<T, String>> + Send + 'static,
    model: String,
    key: Option<String>,
    idle_timeout: Option<Duration>,
    redact_image_errors: bool,
    image_limits: bool,
) -> EventStream
where
    T: AsRef<[u8]> + Send + 'static,
{
    Box::pin(async_stream::stream! {
        let mut body = Box::pin(body);
        let mut buffer: Vec<u8> = Vec::new();
        let mut resolved_model = model;
        let mut usage: Option<ModelUsage> = None;
        // Tool calls arrive as fragments. Verified against a captured live
        // response: the first fragment carries id, type and function.name,
        // then later fragments append `arguments` a few characters at a
        // time, keyed by `index`.
        let mut partial: BTreeMap<u32, PartialCall> = BTreeMap::new();
        let mut saw_tool_finish = false;
        let mut finish_reason: Option<String> = None;
        // S8b: counted only so the end-of-stream summary below can say what
        // arrived. A run that produces no text and no tool call is
        // indistinguishable, after the fact, from one that was never sent -
        // this crate had ZERO tracing calls, so ~9% of chat runs were
        // finishing empty with no record of which of those two happened.
        let mut delta_count: u32 = 0;
        let mut frame_count: u32 = 0;
        let mut emitted_bytes: usize = 0;
        let timeout_duration = idle_timeout.unwrap_or(Duration::from_secs(60));

        macro_rules! final_event {
            () => {{
                // S8b: logged HERE, inside the macro, not after the loop.
                // The first version of this sat before the fall-through
                // `yield` at the end of the function - but `[DONE]` yields
                // and RETURNS, and OpenRouter always sends `[DONE]`, so the
                // summary never fired on the normal path. It was shipped,
                // observed missing from the journal on four live runs, and
                // moved here. Every terminal event goes through this macro,
                // so every stream now leaves exactly one summary line.
                if delta_count == 0 && partial.is_empty() {
                    tracing::warn!(
                        model = %resolved_model,
                        frames = frame_count,
                        finish_reason = finish_reason.as_deref().unwrap_or("(none)"),
                        usage = usage.is_some(),
                        "model stream produced no text and no tool call"
                    );
                } else {
                    tracing::debug!(
                        model = %resolved_model,
                        frames = frame_count,
                        deltas = delta_count,
                        tool_calls = partial.len(),
                        finish_reason = finish_reason.as_deref().unwrap_or("(none)"),
                        usage = usage.is_some(),
                        "model stream complete"
                    );
                }
                if !partial.is_empty() && saw_tool_finish {
                    let calls: Vec<ToolCall> = partial
                        .iter()
                        .map(|(_, v)| ToolCall { id: v.id.clone(), name: v.name.clone(), arguments: v.args.clone() })
                        .collect();
                    ModelEvent::ToolCalls { calls, usage: usage.clone() }
                } else {
                    ModelEvent::Done { model: resolved_model.clone(), usage: usage.clone(), finish_reason: finish_reason.clone() }
                }
            }};
        }

        while let Some(chunk) = match tokio::time::timeout(timeout_duration, body.next()).await {
            Ok(Some(c)) => Some(c),
            Ok(None) => None,
            Err(_) => {
                yield ModelEvent::Error { message: "provider went silent".to_string(), status: None };
                return;
            }
        } {
            let chunk = match chunk {
                Ok(c) => c,
                Err(e) => {
                    yield ModelEvent::Error { message: provider_error(&e, key.as_deref(), redact_image_errors), status: None };
                    return;
                }
            };
            let chunk = chunk.as_ref();
            if image_limits && chunk.len() > crate::image_transport::RESPONSE_FRAME_LIMIT {
                yield ModelEvent::Error {
                    message: "The model provider returned an oversized screen observation response.".to_string(),
                    status: None,
                };
                return;
            }

            for segment in chunk.split_inclusive(|byte| *byte == b'\n') {
                if image_limits
                    && buffer.len().saturating_add(segment.len()) > IMAGE_PENDING_LINE_LIMIT
                {
                    yield ModelEvent::Error {
                        message: "The model provider returned an oversized screen observation response.".to_string(),
                        status: None,
                    };
                    return;
                }
                buffer.extend_from_slice(segment);
                if !segment.ends_with(b"\n") {
                    continue;
                }
                let line_bytes = std::mem::take(&mut buffer);                let mut line = String::from_utf8_lossy(&line_bytes[..line_bytes.len() - 1]).into_owned();
                if line.ends_with('\r') {
                    line.pop();
                }

                if !line.starts_with("data:") {
                    continue; // OpenRouter interleaves `: OPENROUTER PROCESSING` comments.
                }
                let payload = line[5..].trim();
                if payload.is_empty() {
                    continue;
                }
                if payload == "[DONE]" {
                    yield final_event!();
                    return;
                }

                let frame: OpenRouterFrame = match serde_json::from_str(payload) {
                    Ok(f) => f,
                    Err(_) if !image_limits => continue,
                    Err(_) => {
                        yield ModelEvent::Error {
                            message: "The model provider returned a malformed screen observation response.".to_string(),
                            status: None,
                        };
                        return;
                    }
                };

                frame_count += 1;
                if let Some(m) = &frame.model
                    && !m.is_empty()
                {
                    resolved_model = m.clone();
                }
                // The `cost` field used to GATE this whole block, so a usage
                // frame that carried token counts but no cost was discarded
                // entirely and the run recorded 0 input / 0 output / $0 - a
                // run that provably called the model, filed as one that
                // never ran. Cost is the one field Bullpen must never
                // estimate (see `build_body`'s `usage.include`), so an
                // absent cost leaves the legacy numeric field with a 0.0
                // placeholder - it does NOT establish that the call was free -
                // but it no longer takes the
                // token counts down with it, because "we do not know what it
                // cost" and "nothing happened" are different facts and only
                // one of them is true here.
                if let Some(u) = &frame.usage {
                    if u.cost.is_none() {
                        tracing::warn!(
                            model = %resolved_model,
                            prompt_tokens = u.prompt_tokens.unwrap_or(0),
                            "usage frame carried no cost; legacy numeric cost placeholder is 0.0 but actual cost is unknown"
                        );
                    }
                    usage = Some(ModelUsage {
                        cost_usd: u.cost.unwrap_or(0.0),
                        input_tokens: u.prompt_tokens.unwrap_or(0),
                        output_tokens: u.completion_tokens.unwrap_or(0),
                        cached_tokens: u.prompt_tokens_details.as_ref().and_then(|d| d.cached_tokens).unwrap_or(0),
                    });
                }
                if let Some(err) = &frame.error {
                    yield ModelEvent::Error { message: provider_error(err.message.as_deref().unwrap_or("upstream error"), key.as_deref(), redact_image_errors), status: None };
                    return;
                }

                if let Some(choice) = frame.choices.as_ref().and_then(|c| c.first()) {
                    if choice.finish_reason.as_deref() == Some("tool_calls") {
                        saw_tool_finish = true;
                    }
                    if let Some(fr) = &choice.finish_reason {
                        finish_reason = Some(fr.clone());
                    }
                    if let Some(delta) = &choice.delta {
                        for fragment in delta.tool_calls.iter().flatten() {
                            let index = fragment.index.unwrap_or(0);
                            if image_limits
                                && !partial.contains_key(&index)
                                && partial.len() >= IMAGE_PARTIAL_CALL_LIMIT
                            {
                                yield ModelEvent::Error {
                                    message: "The model provider returned too many screen observation tool calls.".to_string(),
                                    status: None,
                                };
                                return;
                            }
                            let added = fragment.id.as_ref().map_or(0, String::len)
                                + fragment
                                    .function
                                    .as_ref()
                                    .and_then(|function| function.name.as_ref())
                                    .map_or(0, String::len)
                                + fragment
                                    .function
                                    .as_ref()
                                    .and_then(|function| function.arguments.as_ref())
                                    .map_or(0, String::len);
                            if image_limits
                                && emitted_bytes.saturating_add(added)
                                    > IMAGE_DECODED_OUTPUT_LIMIT
                            {
                                yield ModelEvent::Error {
                                    message: "The model provider returned too much screen observation output.".to_string(),
                                    status: None,
                                };
                                return;
                            }
                            emitted_bytes += added;
                            let slot = partial.entry(index).or_default();
                            if let Some(id) = &fragment.id
                                && !id.is_empty()
                            {
                                slot.id = id.clone();
                            }
                            if let Some(function) = &fragment.function {
                                if let Some(name) = &function.name
                                    && !name.is_empty()
                                {
                                    slot.name = name.clone();
                                }
                                if let Some(args) = &function.arguments {
                                    slot.args.push_str(args);
                                }
                            }
                        }
                        if let Some(text) = &delta.content
                            && !text.is_empty()
                        {
                            if image_limits
                                && emitted_bytes.saturating_add(text.len())
                                    > IMAGE_DECODED_OUTPUT_LIMIT
                            {
                                yield ModelEvent::Error {
                                    message: "The model provider returned too much screen observation output.".to_string(),
                                    status: None,
                                };
                                return;
                            }
                            emitted_bytes += text.len();
                            delta_count += 1;
                            yield ModelEvent::Delta { text: text.clone() };
                        }
                    }
                }
            }
        }

        // The stream ended without a `[DONE]` marker - same decision as that
        // marker would have made, from whatever state was accumulated.
        yield final_event!();
    })
}

#[derive(Debug, Deserialize)]
struct OpenRouterFrame {
    model: Option<String>,
    error: Option<FrameError>,
    choices: Option<Vec<FrameChoice>>,
    usage: Option<FrameUsage>,
}

#[derive(Debug, Deserialize)]
struct FrameError {
    message: Option<String>,
}

#[derive(Debug, Deserialize)]
struct FrameChoice {
    finish_reason: Option<String>,
    delta: Option<FrameDelta>,
}

#[derive(Debug, Deserialize)]
struct FrameDelta {
    content: Option<String>,
    tool_calls: Option<Vec<FrameToolCallFragment>>,
}

#[derive(Debug, Deserialize)]
struct FrameToolCallFragment {
    index: Option<u32>,
    id: Option<String>,
    function: Option<FrameFunctionFragment>,
}

#[derive(Debug, Deserialize)]
struct FrameFunctionFragment {
    name: Option<String>,
    arguments: Option<String>,
}

#[derive(Debug, Deserialize)]
struct FrameUsage {
    cost: Option<f64>,
    prompt_tokens: Option<u32>,
    completion_tokens: Option<u32>,
    prompt_tokens_details: Option<FrameUsageDetails>,
}

#[derive(Debug, Deserialize)]
struct FrameUsageDetails {
    cached_tokens: Option<u32>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn busy_wait_ms_is_zero_for_anything_but_429() {
        assert_eq!(busy_wait_ms(500, "", None), 0);
        assert_eq!(busy_wait_ms(200, "", None), 0);
    }

    #[test]
    fn busy_wait_ms_reads_the_body_hint() {
        let body = r#"{"error":{"metadata":{"retry_after_seconds":3}}}"#;
        assert_eq!(busy_wait_ms(429, body, None), 3_000);
    }

    #[test]
    fn busy_wait_ms_falls_back_to_the_header() {
        assert_eq!(busy_wait_ms(429, "not json", Some("2")), 2_000);
    }

    #[test]
    fn busy_wait_ms_defaults_to_one_second_when_neither_says() {
        assert_eq!(busy_wait_ms(429, "", None), 1_000);
    }

    #[test]
    fn busy_wait_ms_never_exceeds_the_cap() {
        let body = r#"{"error":{"metadata":{"retry_after_seconds":9999}}}"#;
        assert_eq!(busy_wait_ms(429, body, None), BUSY_WAIT_CAP_MS);
    }

    #[test]
    fn busy_wait_ms_prefers_the_body_over_the_header() {
        let body = r#"{"error":{"metadata":{"retry_after_seconds":4}}}"#;
        assert_eq!(busy_wait_ms(429, body, Some("1")), 4_000);
    }

    #[test]
    fn image_request_provider_errors_never_echo_the_payload() {
        let secret = "data:image/png;base64,TOPSECRET";
        let message = provider_error(&format!("bad request: {secret}"), None, true);
        assert!(!message.contains("data:image"));
        assert!(!message.contains("TOPSECRET"));
    }

    #[test]
    fn image_url_debug_is_metadata_only() {
        let debug = format!(
            "{:?}",
            ImageUrl {
                url: "data:image/png;base64,TOPSECRET".into()
            }
        );
        assert!(!debug.contains("data:image"));
        assert!(!debug.contains("TOPSECRET"));
    }
}
