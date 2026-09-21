//! S9-07/08: repo tools — offered only when the bot has a `repo` row.
//! Port of `app.ts` repo specs + dispatch.

use std::sync::{Arc, Mutex, PoisonError};

use model::ToolSpec;
use serde_json::json;

use crate::egress;
use crate::mcp::{self, McpCallOptions};
use crate::repo::{self, RepoEdit};
use crate::runs::ConnectorHooks;
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

pub fn repo_edit_spec() -> ToolSpec {
    ToolSpec {
        name: "repo_edit".to_string(),
        description: "Edit a file in your repo checkout with exact find/replace pairs, applied in \
order. Each find must match EXACTLY ONCE in the file - if it matches zero or several times, the \
whole call is refused and nothing changes. Give enough surrounding context in find to make it unique."
            .to_string(),
        parameters: json!({
            "type": "object",
            "properties": {
                "path": { "type": "string", "description": "Path inside the repo." },
                "edits": {
                    "type": "array",
                    "items": {
                        "type": "object",
                        "properties": {
                            "find": { "type": "string", "description": "Exact text to find, unique in the file." },
                            "replace": { "type": "string", "description": "What to replace it with." }
                        },
                        "required": ["find", "replace"]
                    }
                }
            },
            "required": ["path", "edits"]
        }),
    }
}

pub fn repo_pr_spec() -> ToolSpec {
    ToolSpec {
        name: "repo_pr".to_string(),
        description: "Act on a pull request through GitHub, for your repo. create pushes your \
current branch's commits (make them with repo_run's git first) and opens a PR against the repo's \
configured branch. update/comment/labels/ci_status act on an existing PR. The push happens on the \
server with GitHub's own credential - you never see or hold a token."
            .to_string(),
        parameters: json!({
            "type": "object",
            "properties": {
                "action": { "type": "string", "enum": ["create", "update", "comment", "labels", "ci_status"] },
                "title": { "type": "string", "description": "create/update: the PR title." },
                "body": { "type": "string", "description": "create/update/comment: the PR body or comment text." },
                "base": { "type": "string", "description": "create only: override the base branch. Defaults to the repo's configured branch." },
                "id": { "type": "string", "description": "update/comment/labels/ci_status: the PR number, as a string." },
                "label": { "type": "string", "description": "labels: a comma-separated list of labels to set." }
            },
            "required": ["action"]
        }),
    }
}

pub fn all_repo_specs() -> Vec<ToolSpec> {
    vec![
        repo_read_spec(),
        repo_grep_spec(),
        repo_edit_spec(),
        repo_run_spec(),
        repo_branch_spec(),
        repo_pr_spec(),
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
    db: &Arc<Mutex<Db>>,
    sandbox: &dyn Sandbox,
    bot_id: &str,
    name: &str,
    args: &str,
    connector_hooks: Option<&Arc<ConnectorHooks>>,
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
        "repo_grep" => run_grep(sandbox, bot_id, &parsed, &repo_cfg).await,
        "repo_edit" => run_edit(sandbox, bot_id, &parsed).await,
        "repo_run" => run_run(sandbox, bot_id, &parsed).await,
        "repo_branch" => run_branch(sandbox, bot_id, &parsed).await,
        "repo_pr" => run_pr(db, sandbox, bot_id, &repo_cfg, &parsed, connector_hooks).await,
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

async fn run_edit(sandbox: &dyn Sandbox, bot_id: &str, input: &serde_json::Value) -> String {
    let path = input.get("path").and_then(|v| v.as_str()).unwrap_or("");
    if path.trim().is_empty() {
        return "No path was given.".to_string();
    }
    let raw_edits = input
        .get("edits")
        .and_then(|v| v.as_array())
        .cloned()
        .unwrap_or_default();
    let mut edits = Vec::new();
    for e in raw_edits {
        let Some(obj) = e.as_object() else { continue };
        let Some(find) = obj.get("find").and_then(|v| v.as_str()) else {
            continue;
        };
        let Some(replace) = obj.get("replace").and_then(|v| v.as_str()) else {
            continue;
        };
        edits.push(RepoEdit {
            find: find.to_string(),
            replace: replace.to_string(),
        });
    }
    if edits.is_empty() {
        return "No valid edits were given. Each needs a find and a replace, both strings."
            .to_string();
    }
    let applied = repo::apply_repo_edits(sandbox, bot_id, path, &edits).await;
    applied.detail
}

async fn run_grep(
    sandbox: &dyn Sandbox,
    bot_id: &str,
    input: &serde_json::Value,
    _repo_cfg: &repo::RepoConfig,
) -> String {
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

fn connector_catalogue(
    hooks: &ConnectorHooks,
) -> std::collections::HashMap<String, Vec<mcp::ConnectorTool>> {
    hooks
        .catalogue
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .clone()
}

async fn run_pr(
    db: &Arc<Mutex<Db>>,
    sandbox: &dyn Sandbox,
    bot_id: &str,
    repo_cfg: &repo::RepoConfig,
    input: &serde_json::Value,
    connector_hooks: Option<&Arc<ConnectorHooks>>,
) -> String {
    let action = input
        .get("action")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();
    const ACTIONS: &[&str] = &["create", "update", "comment", "labels", "ci_status"];
    if !ACTIONS.contains(&action.as_str()) {
        return "action must be one of create, update, comment, labels, ci_status.".to_string();
    }

    let Some(hooks) = connector_hooks else {
        return "The GitHub connector is not switched on for you. Ask Josh to enable it."
            .to_string();
    };

    let connector = {
        let db = lock_db(db);
        store::connectors_for_bot(&db, bot_id)
            .unwrap_or_default()
            .into_iter()
            .find(|c| mcp::slug_name(&c.name) == "github")
    };
    let Some(connector) = connector else {
        return "The GitHub connector is not switched on for you. Ask Josh to enable it."
            .to_string();
    };

    let bearer =
        crate::oauth::bearer_for_shared(db, &connector.id, hooks.oauth_http.as_ref()).await;
    let Some(bearer) = bearer.filter(|b| !b.is_empty()) else {
        return "GitHub is not authorized yet. Tell Josh to open Connectors and press Connect on GitHub. Do not retry.".to_string();
    };

    let Some(parsed) = repo::parse_github_repo(&repo_cfg.url) else {
        return format!("Could not read an owner/repo out of {}.", repo_cfg.url);
    };

    let catalogue = connector_catalogue(hooks);
    let tools = catalogue.get(&connector.id).cloned().unwrap_or_default();
    let mcp_opts = McpCallOptions {
        transport: hooks.transport.as_ref(),
        resolver: hooks.resolver.as_ref(),
        bearer: Some(bearer.as_str()),
    };

    if action.as_str() == "create" {
        let base = input
            .get("base")
            .and_then(|v| v.as_str())
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .unwrap_or(repo_cfg.branch.as_str());
        let branch_check = sandbox
            .exec(bot_id, "cd /work/repo && git rev-parse --abbrev-ref HEAD")
            .await;
        let branch = branch_check.stdout.trim();
        if branch.is_empty() || branch == base {
            return format!(
                "Make a branch first with repo_branch - you are on {}.",
                if branch.is_empty() {
                    "no branch"
                } else {
                    "the base branch itself"
                }
            );
        }
        let patch_cmd = format!(
            "cd /work/repo && git format-patch {}..HEAD --stdout",
            repo::sh_quote(base)
        );
        let patch = sandbox.exec(bot_id, &patch_cmd).await;
        let pushed = repo::push_patch_default(repo::PushInput {
            repo_url: &repo_cfg.url,
            base,
            branch,
            patch_text: &patch.stdout,
            token: Some(bearer.as_str()),
        })
        .await;
        if !pushed.ok {
            return pushed.detail;
        }
        let Some(tool) = repo::pick_github_tool(&tools, "create") else {
            return format!(
                "{} The PR itself could not be opened - GitHub's connector offers no tool that looks like \"create pull request\".",
                pushed.detail
            );
        };
        let default_title = format!("Changes from {branch}");
        let title = input
            .get("title")
            .and_then(|v| v.as_str())
            .filter(|s| !s.is_empty())
            .unwrap_or(default_title.as_str());
        let body = input.get("body").and_then(|v| v.as_str()).unwrap_or("");
        let args = json!({
            "owner": parsed.owner,
            "repo": parsed.repo,
            "title": title,
            "body": body,
            "head": branch,
            "base": base,
        });
        let result = mcp::call_connector_tool(&connector, &tool.name, args, mcp_opts).await;
        return format!("{}\n{result}", pushed.detail);
    }

    let Some(tool) = repo::pick_github_tool(&tools, action.as_str()) else {
        return format!("GitHub's connector offers no tool that looks like \"{action}\".");
    };

    let pr_number = input.get("id").and_then(|v| v.as_str()).unwrap_or("");
    let mut pr_args = json!({
        "owner": parsed.owner,
        "repo": parsed.repo,
    });
    if !pr_number.is_empty()
        && let Some(obj) = pr_args.as_object_mut()
    {
        obj.insert("pull_number".into(), json!(pr_number));
        obj.insert("issue_number".into(), json!(pr_number));
    }
    if action.as_str() == "update" {
        if let Some(title) = input.get("title").and_then(|v| v.as_str()) {
            pr_args["title"] = json!(title);
        }
        if let Some(body) = input.get("body").and_then(|v| v.as_str()) {
            pr_args["body"] = json!(body);
        }
    }
    if action.as_str() == "comment"
        && let Some(body) = input.get("body").and_then(|v| v.as_str())
    {
        pr_args["body"] = json!(body);
    }
    if action.as_str() == "labels"
        && let Some(label) = input.get("label").and_then(|v| v.as_str())
    {
        let labels: Vec<&str> = label
            .split(',')
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .collect();
        pr_args["labels"] = json!(labels);
    }

    mcp::call_connector_tool(&connector, &tool.name, pr_args, mcp_opts).await
}
