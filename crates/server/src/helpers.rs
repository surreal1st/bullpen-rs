//! Throwaway helper errands (`spawn_helper`). Port of `helpers.ts`.

use std::sync::Arc;

use model::ModelUsage;
use model::ladder::{Trigger, default_model, model_for_run};
use uuid::Uuid;

use crate::prompt;
use crate::runs::RunManager;

pub const HELPER_MAX_STEPS: i64 = 6;

pub struct HelperKindSpec {
    pub tools: &'static [&'static str],
    pub system: &'static str,
}

pub fn helper_spec(kind: &str) -> Option<HelperKindSpec> {
    match kind {
        "explore" => Some(HelperKindSpec {
            tools: &["web_search", "fetch_url"],
            system: "You are a research helper. Find what was asked on the public web, read the best two or three sources, and answer with the facts and their URLs. No opinions, no padding.",
        }),
        "browse" => Some(HelperKindSpec {
            tools: &["browse", "read_page", "click", "type_text"],
            system: "You are a browsing helper on the shared computer. Do only what the brief asks, read what the page shows, and report exactly what you saw.",
        }),
        "read" => Some(HelperKindSpec {
            tools: &["search_memory", "read_output"],
            system: "You are a reading helper. Answer from the memory and held outputs you can search; say plainly when they do not contain it.",
        }),
        _ => None,
    }
}

pub fn valid_kind_names() -> &'static [&'static str] {
    &["explore", "browse", "read"]
}

pub struct HelperResult {
    pub reply: String,
    pub usage: Option<ModelUsage>,
}

/// Runs one helper errand (silent emit — no run subscriber).
pub async fn run_helper(
    manager: &Arc<RunManager>,
    caller_bot_id: &str,
    trigger: Trigger,
    room: bool,
    kind: &str,
    brief: &str,
    depth: u32,
) -> HelperResult {
    let spec = helper_spec(kind).expect("kind validated before call");
    let model = {
        let db = manager.db();
        let floor = default_model(&db);
        model_for_run(&db, trigger, &floor, room)
    };
    let messages = {
        let db = manager.db();
        prompt::build_helper_prompt(&db, spec.system, brief)
    };
    let only: Vec<String> = spec.tools.iter().map(|s| (*s).to_string()).collect();
    let toolbox = manager.toolbox_for_helper(caller_bot_id, trigger, room, depth, only);
    let narrowed = toolbox.narrow_to(spec.tools);
    let run_id = format!("helper-{}", Uuid::new_v4());
    let outcome = manager
        .run_turn(
            &run_id,
            caller_bot_id,
            trigger,
            room,
            model,
            messages,
            &narrowed,
            0,
            String::new(),
            None,
            None,
            HELPER_MAX_STEPS,
        )
        .await;
    match outcome {
        crate::runs::Outcome::Answered(state) => HelperResult {
            reply: state.text,
            usage: state.usage,
        },
        crate::runs::Outcome::Paused { pending, state, .. } => HelperResult {
            reply: format!(
                "The helper needed permission to use {}, which cannot be asked for inside a helper errand. Ask directly instead.",
                pending.name
            ),
            usage: state.usage,
        },
        crate::runs::Outcome::Failed { failure, state, .. } => HelperResult {
            reply: failure,
            usage: state.usage,
        },
    }
}
