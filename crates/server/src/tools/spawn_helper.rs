//! `spawn_helper` — a throwaway errand with a fixed tool list and cheap model.
//! Port of `helpers.ts` / `app.ts` dispatch.

use std::sync::Arc;

use model::ToolSpec;
use serde::Deserialize;
use serde_json::json;

use crate::delegate::{DEPTH_REFUSAL, MAX_DELEGATION_DEPTH};
use crate::helpers::{self, valid_kind_names};
use crate::runs::RunManager;
use crate::tools::ToolOutcome;

pub fn spec() -> ToolSpec {
    ToolSpec {
        name: "spawn_helper".to_string(),
        description: "Send a throwaway helper on a side errand with a fixed brief and a narrowed tool list. Use it for research, browsing, or reading memory — not for asking a colleague (use message_bot) or hiring someone (use hire_bot)."
            .to_string(),
        parameters: json!({
            "type": "object",
            "properties": {
                "kind": {
                    "type": "string",
                    "description": "One of explore, browse, or read."
                },
                "brief": {
                    "type": "string",
                    "description": "What the helper should do, in full."
                }
            },
            "required": ["kind", "brief"]
        }),
    }
}

#[derive(Deserialize, Default)]
struct Args {
    #[serde(default)]
    kind: String,
    #[serde(default)]
    brief: String,
}

pub async fn run(
    manager: &Arc<RunManager>,
    caller_bot_id: &str,
    trigger: model::ladder::Trigger,
    room: bool,
    delegation_depth: u32,
    args: &str,
) -> ToolOutcome {
    if delegation_depth >= MAX_DELEGATION_DEPTH {
        return ToolOutcome::new(DEPTH_REFUSAL, None);
    }
    let parsed: Args = serde_json::from_str(args).unwrap_or_default();
    let model_kind = parsed.kind.trim();
    if helpers::helper_spec(model_kind).is_none() {
        let names = valid_kind_names().join(", ");
        return ToolOutcome::new(format!("A helper's kind must be one of: {names}."), None);
    }
    let kind = crate::jev::gate_helper_kind(parsed.brief.trim(), model_kind).await;
    let result = helpers::run_helper(
        manager,
        caller_bot_id,
        trigger,
        room,
        &kind,
        parsed.brief.trim(),
        delegation_depth,
    )
    .await;
    ToolOutcome::new(
        format!("Helper ({kind}) says:\n{}", result.reply),
        result.usage,
    )
}
