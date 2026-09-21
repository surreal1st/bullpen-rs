//! S9-07: `repo_read`, `repo_grep`, `repo_run`, `repo_branch` — offered only
//! when the bot has a `repo` row. Port of `app.ts` repo specs + dispatch.

use model::ToolSpec;
use serde_json::json;

use crate::egress;
use crate::repo;
use crate::sandbox::Sandbox;
use store::Db;

use super::shell;
use super::{fence_tool_output, lock_db};

pub fn repo_read_spec() -> ToolSpec {
    ToolSpec {
        name: "repo_read".to_string(),
        description: "Read a file from your repo checkout. Clones the repo on first use - the \
first call may take a moment. Returns up to 4000 lines; use offset/limit for a bigger file."
            .to_string(),
        parameters: json!({
            "type": "object",
            "properties": {
                "path": { "type": "string", "description": "Path inside the repo, e.g. src/index.ts." },
                "offset": { "type": "number", "description": "First line to return, 1-based. Default 1." },
                "limit": { "type": "number", "description": "Maximum lines to return. Default 4000." }
            },
            "required": ["path"]
        }),
    }
}

pub fn repo_grep_spec() -> ToolSpec {
    ToolSpec {
        name: "repo_grep".to_string(),
        description: "Search your repo checkout for a pattern. Clones the repo on first use."
            .to_string(),
        parameters: json!({
            "type": "object",
            "properties": {
                "pattern": { "type": "string", "description": "Text or regular expression to search for." },
                "glob": { "type": "string", "description": "Optional, e.g. *.ts to search only matching files." }
            },
            "required": ["pattern"]
        }),
    }
}

pub fn repo_run_spec() -> ToolSpec {
    ToolSpec {
        name: "repo_run".to_string(),
        description: "Run a shell command inside your repo checkout - tests, lint, a build. Same \
sandbox and caps as `shell`, just starting in the repo instead of an empty /work."
            .to_string(),
        parameters: json!({
            "type": "object",
            "properties": {
                "command": { "type": "string", "description": "A shell command, run with sh -c in the repo root." }
            },
            "required": ["command"]
        }),
    }
}

pub fn repo_branch_spec() -> ToolSpec {
    ToolSpec {
        name: "repo_branch".to_string(),
        description:
            "Create a local branch in your repo checkout, or switch to it if it already exists."
                .to_string(),
        parameters: json!({
            "type": "object",
            "properties": {
                "name": { "type": "string", "description": "The branch name." }
            },
            "required": ["name"]
        }),
    }
}

pub fn all_repo_specs() -> Vec<ToolSpec> {
    vec![
        repo_read_spec(),
        repo_grep_spec(),
        repo_run_spec(),
        repo_branch_spec(),
    ]
}

pub fn repo_tool_names() -> Vec<String> {
    all_repo_specs().into_iter().map(|s| s.name).collect()
}

fn bot_egress_for(db: &Db, bot_id: &str) -> egress::BotEgressConfig {
    let raw = store::get_bot_egress(db, bot_id).ok().flatten();
    egress::parse_bot_egress(raw.as_deref())
}

pub async fn run(
    db: &std::sync::Arc<std::sync::Mutex<Db>>,
    sandbox: &dyn Sandbox,
    bot_id: &str,
    name: &str,
    args: &str,
) -> String {
    let (repo_cfg, bot_egress) = {
        let db = lock_db(db);
        (
            repo::get_bot_repo(&db, bot_id).ok().flatten(),
            bot_egress_for(&db, bot_id),
        )
    };
    let Some(repo_cfg) = repo_cfg else {
        return "This bot has no repository set.".to_string();
    };

    let parsed: serde_json::Value =
        serde_json::from_str(args).unwrap_or_else(|_| serde_json::json!({}));

    let egress_env = std::env::var("BULLPEN_SANDBOX_EGRESS")
        .ok()
        .map(|s| s.to_string());
    if let Err(detail) = repo::prepare_repo(
        sandbox,
        bot_id,
        &repo_cfg,
        &bot_egress,
        egress_env.as_deref(),
    )
    .await
    {
        return detail;
    }

    match name {
        "repo_read" => run_read(sandbox, bot_id, &parsed).await,
        "repo_grep" => run_grep(sandbox, bot_id, &parsed).await,
        "repo_run" => run_run(sandbox, bot_id, &parsed).await,
        "repo_branch" => run_branch(sandbox, bot_id, &parsed).await,
        other => format!("Unknown tool: {other}"),
    }
}

async fn run_read(sandbox: &dyn Sandbox, bot_id: &str, input: &serde_json::Value) -> String {
    let path = input.get("path").and_then(|v| v.as_str()).unwrap_or("");
    if path.trim().is_empty() {
        return "No path was given.".to_string();
    }
    let read = repo::read_repo_file(sandbox, bot_id, path).await;
    if !read.ok {
        return read.detail;
    }
    let lines: Vec<&str> = read.content.split('\n').collect();
    let offset = input
        .get("offset")
        .and_then(|v| v.as_f64())
        .filter(|&n| n > 0.0)
        .map(|n| n.floor() as usize)
        .unwrap_or(1);
    let limit = input
        .get("limit")
        .and_then(|v| v.as_f64())
        .filter(|&n| n > 0.0)
        .map(|n| n.floor() as usize)
        .unwrap_or(4000);
    let start = offset.saturating_sub(1);
    let slice = lines
        .iter()
        .skip(start)
        .take(limit)
        .enumerate()
        .map(|(i, line)| format!("{}\t{line}", offset + i))
        .collect::<Vec<_>>()
        .join("\n");
    if slice.is_empty() {
        "(empty file, or offset past the end)".to_string()
    } else {
        fence_tool_output(&slice)
    }
}

async fn run_grep(sandbox: &dyn Sandbox, bot_id: &str, input: &serde_json::Value) -> String {
    let pattern = input.get("pattern").and_then(|v| v.as_str()).unwrap_or("");
    if pattern.trim().is_empty() {
        return "No pattern was given.".to_string();
    }
    let glob = input.get("glob").and_then(|v| v.as_str()).unwrap_or("");
    let glob_part = if glob.is_empty() {
        String::new()
    } else {
        format!("--include={} ", repo::sh_quote(glob))
    };
    let command = format!(
        "cd /work/repo && grep -rn {glob_part}-- {} . ; true",
        repo::sh_quote(pattern)
    );
    let result = sandbox.exec(bot_id, &command).await;
    let out = result.stdout.trim();
    if out.is_empty() {
        "No matches.".to_string()
    } else {
        fence_tool_output(out)
    }
}

async fn run_run(sandbox: &dyn Sandbox, bot_id: &str, input: &serde_json::Value) -> String {
    let command = input.get("command").and_then(|v| v.as_str()).unwrap_or("");
    if command.trim().is_empty() {
        return "No command was given.".to_string();
    }
    let wrapped = format!("cd /work/repo && {{ {command} ; }}");
    let result = sandbox.exec(bot_id, &wrapped).await;
    shell::format_exec_result(&result)
}

async fn run_branch(sandbox: &dyn Sandbox, bot_id: &str, input: &serde_json::Value) -> String {
    let branch_name = input.get("name").and_then(|v| v.as_str()).unwrap_or("");
    let result = repo::repo_branch(sandbox, bot_id, branch_name).await;
    result.detail
}
