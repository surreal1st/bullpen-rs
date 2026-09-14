//! The model boundary: OpenRouter streaming, the escalation ladder, the routing
//! classifier, `model_for_run` floors, spend. Port of `model-port.ts`,
//! `escalation.ts`, `routing.ts`, `spend.ts`. This is the pillar Grok Bot lacks.
//!
//! S1-02 lands the first slice: the OpenRouter port itself (`port`), key
//! loading and redaction (`secrets`), and the scripted test double
//! (`fake`) every later slice's tests replay against. Routing, the ladder,
//! and the run loop are later tickets.

pub mod ladder;
pub mod port;
pub mod secrets;

#[cfg(any(test, feature = "fake"))]
pub mod fake;

pub use port::{
    CHEAP_DEFAULT_MODEL, CHEAP_FALLBACK_MODELS, ContentPart, EventStream, FunctionCall, ImageUrl,
    MAX_OUTPUT_TOKENS, MessageContent, MessageToolCall, ModelEvent, ModelMessage, ModelPort,
    ModelRequest, ModelUsage, OpenRouterPort, Reasoning, ToolCall, ToolSpec, VISION_DEFAULT_MODEL,
    busy_wait_ms, parse_sse_stream, utility_messages,
};
pub use secrets::{KeySource, redact};
