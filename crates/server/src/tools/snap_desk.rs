//! Model-facing screen capture. The run manager supplies the bound capture
//! callback; this module owns only the strict tool contract.

use model::ToolSpec;

use super::{ObservationCapture, ToolOutcome};

pub fn spec() -> ToolSpec {
    ToolSpec {
        name: "snap_desk".to_string(),
        description: "Capture a fresh image of your own computer screen for your next model step. Takes no arguments.".to_string(),
        parameters: serde_json::json!({
            "type": "object",
            "properties": {},
            "required": [],
            "additionalProperties": false
        }),
    }
}

pub async fn run(args: &str, capture: &ObservationCapture) -> ToolOutcome {
    let valid = serde_json::from_str::<serde_json::Value>(args)
        .ok()
        .and_then(|value| value.as_object().cloned())
        .is_some_and(|object| object.is_empty());
    if !valid {
        return ToolOutcome::new("snap_desk requires exactly an empty JSON object: {}.", None);
    }
    capture().await
}
