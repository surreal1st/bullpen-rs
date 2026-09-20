//! W5: bot-written tools. Heavy lifting (guard, typecheck, vm jail) runs in
//! Node via `crates/server/w5/run.mjs`, which imports bullpen-night's
//! `bot-tools.ts` read-only. Set `BULLPEN_NIGHT_ROOT` when the default
//! relative path is wrong.

mod node;

use std::path::Path;
use std::sync::{Arc, Mutex};

use chrono::Utc;
use model::ToolSpec;
use serde_json::json;
use sha2::{Digest, Sha256};
use store::{
    Db, ToolProposal, insert_live_tool, is_live_bot_tool, latest_proposal, list_bot_tools,
    list_live_tool_rows, proposal_by_id, reject_proposal, revoke_bot_tool, upsert_proposal_row,
};

use crate::permissions::Decision;
use crate::sandbox::Sandbox;
use crate::tools::lock_db;

pub use node::guard_source;

/// `before_ask` hook for `propose_tool` — port of `app.ts:733-742`.
pub async fn before_ask_propose_tool(
    db_path: &str,
    data_dir: &str,
    bot_id: &str,
    args: &str,
) -> Decision {
    let parsed: serde_json::Value = match serde_json::from_str(args) {
        Ok(v) => v,
        Err(_) => return Decision::Allow,
    };
    if db_path == ":memory:" {
        tracing::warn!("propose_tool before_ask skipped: W5 node bridge needs a file database");
        return Decision::Ask;
    }
    let proposal = match node::prepare_proposal(db_path, data_dir, bot_id, &parsed).await {
        Ok(p) => p,
        Err(err) => {
            tracing::error!("prepareProposal failed: {err}");
            return Decision::Allow;
        }
    };
    if proposal.ok {
        Decision::Ask
    } else {
        Decision::Allow
    }
}

pub async fn run_propose_tool(
    db: &Arc<Mutex<Db>>,
    db_path: &str,
    data_dir: &str,
    bot_id: &str,
    args: &str,
) -> String {
    let parsed: serde_json::Value = match serde_json::from_str(args) {
        Ok(v) => v,
        Err(_) => return "Could not parse the proposal arguments as JSON.".to_string(),
    };
    if db_path == ":memory:" {
        return "propose_tool is not available on an in-memory database in this build.".to_string();
    }
    let proposal = match node::prepare_proposal(db_path, data_dir, bot_id, &parsed).await {
        Ok(p) => p,
        Err(err) => return format!("Could not check the proposal: {err}."),
    };
    {
        let db = lock_db(db);
        if let Err(err) = upsert_proposal_row(&db, &proposal) {
            tracing::error!("persist proposal: {err}");
        }
    }
    if !proposal.ok {
        return node::describe_proposal(&proposal)
            .await
            .unwrap_or_else(|_err| {
                format!(
                    "Your tool \"{}\" was not accepted. {}",
                    proposal.name,
                    proposal.refusal.unwrap_or_default()
                )
            });
    }
    let approved = approve_proposal(db, data_dir, &proposal.id);
    if !approved.ok {
        return format!(
            "{} could not be made live: {}",
            proposal.name,
            approved.error.unwrap_or_default()
        );
    }
    format!(
        "\"{}\" is live. Every bot on the roster is offered it from their next run, and Josh can withdraw it in Settings.",
        proposal.name
    )
}

pub struct ApproveOutcome {
    pub ok: bool,
    pub error: Option<String>,
}

pub fn approve_proposal(db: &Arc<Mutex<Db>>, data_dir: &str, id: &str) -> ApproveOutcome {
    let db = lock_db(db);
    let proposal = match proposal_by_id(&db, id) {
        Ok(Some(p)) => p,
        Ok(None) => {
            return ApproveOutcome {
                ok: false,
                error: Some("no such proposal".to_string()),
            };
        }
        Err(err) => {
            return ApproveOutcome {
                ok: false,
                error: Some(err.to_string()),
            };
        }
    };
    if !proposal.ok {
        return ApproveOutcome {
            ok: false,
            error: Some("that proposal did not pass its own checks".to_string()),
        };
    }
    if is_live_bot_tool(&db, &proposal.name).unwrap_or(false) {
        return ApproveOutcome {
            ok: false,
            error: Some(format!("{} is already live", proposal.name)),
        };
    }
    let path = Path::new(data_dir)
        .join("tools")
        .join("approved")
        .join(format!("{}.ts", proposal.name));
    if let Some(parent) = path.parent()
        && let Err(err) = std::fs::create_dir_all(parent)
    {
        return ApproveOutcome {
            ok: false,
            error: Some(format!("could not write tool file: {err}")),
        };
    }
    if let Err(err) = std::fs::write(&path, &proposal.source) {
        return ApproveOutcome {
            ok: false,
            error: Some(format!("could not write tool file: {err}")),
        };
    }
    let hash = hex::encode(Sha256::digest(proposal.source.as_bytes()));
    let now = Utc::now();
    if let Err(err) = insert_live_tool(&db, &proposal, path.to_string_lossy().as_ref(), &hash, now)
    {
        return ApproveOutcome {
            ok: false,
            error: Some(err.to_string()),
        };
    }
    ApproveOutcome {
        ok: true,
        error: None,
    }
}

pub fn reject_proposal_id(db: &Arc<Mutex<Db>>, id: &str) {
    let db = lock_db(db);
    let _ = reject_proposal(&db, id, Utc::now());
}

pub fn approved_tool_specs(db: &Arc<Mutex<Db>>, _db_path: &str) -> Vec<ToolSpec> {
    let rows = {
        let db = lock_db(db);
        list_live_tool_rows(&db).unwrap_or_default()
    };
    let mut specs = Vec::new();
    for row in rows {
        // TS re-guards source on every read; doing that here via the node
        // bridge would call `block_on` from inside `run_turn`'s async worker
        // during `tools::build` and wedge the runtime. Live rows are only
        // written after `guardSource` passed at approve time.
        let parameters: serde_json::Value = serde_json::from_str(&row.parameters)
            .unwrap_or_else(|_| json!({"type":"object","properties":{}}));
        specs.push(ToolSpec {
            name: row.name,
            description: format!("{} (a tool one of your colleagues wrote)", row.description),
            parameters,
        });
    }
    specs
}

pub fn is_bot_made_tool(db: &Arc<Mutex<Db>>, name: &str) -> bool {
    let db = lock_db(db);
    is_live_bot_tool(&db, name).unwrap_or(false)
}

pub async fn run_bot_tool(
    db: &Arc<Mutex<Db>>,
    db_path: &str,
    sandbox: &Arc<dyn Sandbox>,
    bot_id: &str,
    name: &str,
    args: &str,
) -> String {
    if !is_bot_made_tool(db, name) {
        return format!("There is no tool called {name} any more.");
    }
    let args_value: serde_json::Value = serde_json::from_str(args).unwrap_or(json!({}));
    if db_path == ":memory:" {
        return "Bot-made tools are not available on an in-memory database in this build."
            .to_string();
    }
    let _ = sandbox; // node path uses its own sandbox stub today
    match node::run_bot_tool(db_path, bot_id, name, &args_value).await {
        Ok(result) => {
            let db = lock_db(db);
            let _ = store::increment_tool_calls(&db, name);
            result
        }
        Err(err) => format!("{name} failed: {err}"),
    }
}

pub fn list_tools(db: &Arc<Mutex<Db>>) -> Vec<store::BotMadeToolRow> {
    let db = lock_db(db);
    list_bot_tools(&db).unwrap_or_default()
}

pub fn revoke_tool(db: &Arc<Mutex<Db>>, name: &str) -> bool {
    let db = lock_db(db);
    revoke_bot_tool(&db, name, Utc::now()).unwrap_or(false)
}

pub fn proposal_for_approval(
    db: &Arc<Mutex<Db>>,
    bot_id: &str,
    tool_args: &str,
) -> Option<ToolProposal> {
    let name = parse_proposal_name(tool_args);
    if name.is_empty() {
        return None;
    }
    let db = lock_db(db);
    latest_proposal(&db, bot_id, &name).ok().flatten()
}

fn parse_proposal_name(tool_args: &str) -> String {
    let parsed: serde_json::Value = serde_json::from_str(tool_args).unwrap_or(json!({}));
    parsed
        .get("name")
        .and_then(|v| v.as_str())
        .map(|s| s.trim())
        .filter(|s| !s.is_empty())
        .unwrap_or_default()
        .to_string()
}

pub fn spec() -> ToolSpec {
    ToolSpec {
        name: "propose_tool".to_string(),
        description:
            "Write a new tool for the whole roster and put it to Josh. The source is one TypeScript module that exports run(args, ctx) - ctx gives you ctx.fetch(url) through your own internet allow list, ctx.readWork(path) and ctx.writeWork(path, text) inside your own /work, and ctx.log(line). You may not import anything, and there is nothing else to reach. It is compiled and run against your own examples before Josh sees it, so if it does not work you are told why and he is not disturbed. Once he approves it, every bot can call it."
                .to_string(),
        parameters: json!({
            "type": "object",
            "properties": {
                "name": { "type": "string", "description": "Lower case letters, digits and underscores, e.g. word_count." },
                "description": { "type": "string", "description": "What it does, as another bot would need to read it to decide to call it." },
                "parameters": { "type": "object", "description": "A JSON schema for the arguments." },
                "examples": {
                    "type": "array",
                    "description": "At least one worked example { args, expect }.",
                    "items": {
                        "type": "object",
                        "properties": {
                            "args": { "type": "object" },
                            "expect": { "type": "string" }
                        },
                        "required": ["args", "expect"]
                    }
                },
                "source": { "type": "string", "description": "The whole module. export async function run(args, ctx) { ... }" }
            },
            "required": ["name", "description", "parameters", "examples", "source"]
        }),
    }
}
