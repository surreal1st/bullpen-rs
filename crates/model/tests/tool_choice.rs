//! SEC5-10: `tool_choice` wire shape and per-model capability flag.

use model::{
    ModelMessage, ModelRequest, ToolChoice, ToolSpec, build_body, supports_tool_choice_required,
};

#[test]
fn build_body_omits_tool_choice_by_default() {
    let request = ModelRequest {
        model: "test/model".to_string(),
        messages: vec![ModelMessage::user("hi")],
        tools: Some(vec![ToolSpec {
            name: "say".to_string(),
            description: "speak".to_string(),
            parameters: serde_json::json!({ "type": "object" }),
        }]),
        ..Default::default()
    };
    let body = build_body(&request, "test/model");
    assert!(body.get("tool_choice").is_none());
}

#[test]
fn build_body_sends_required_when_caller_asked() {
    let request = ModelRequest {
        model: "google/gemini-3.8-flash".to_string(),
        messages: vec![ModelMessage::user("use a tool")],
        tools: Some(vec![ToolSpec {
            name: "say".to_string(),
            description: "speak".to_string(),
            parameters: serde_json::json!({ "type": "object" }),
        }]),
        tool_choice: Some(ToolChoice::Required),
        ..Default::default()
    };
    let body = build_body(&request, "google/gemini-3.8-flash");
    assert_eq!(body["tool_choice"], "required");
}

#[test]
fn capability_flag_matches_measured_models() {
    assert!(supports_tool_choice_required(
        "google/gemini-3.8-flash",
        false
    ));
    assert!(!supports_tool_choice_required("qwen/qwen3.8-flash", true));
    assert!(!supports_tool_choice_required("openai/gpt-oss-120b", true));
}
