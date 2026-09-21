//! S9-07: a bot's own coding-agent checkout at `/work/repo`.
//!
//! Port of `projects/bullpen-night/src/server/repo.ts` (read/run/branch slice;
//! `repo_edit` / `repo_pr` land in S9-08). The sandbox never holds a GitHub
//! credential — clone/read/grep/run happen inside the bot volume only.

use base64::Engine;
use base64::engine::general_purpose::STANDARD as BASE64;
use rusqlite::OptionalExtension;
use serde::{Deserialize, Serialize};
use store::Db;

use crate::egress::{self, BotEgressConfig, EgressMode, EgressPolicy};
use crate::sandbox::Sandbox;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RepoConfig {
    pub url: String,
    #[serde(default = "default_branch")]
    pub branch: String,
    #[serde(rename = "setupCommand", skip_serializing_if = "Option::is_none")]
    pub setup_command: Option<String>,
}

fn default_branch() -> String {
    "main".to_string()
}

fn clean_string(input: &serde_json::Value) -> String {
    input.as_str().unwrap_or("").trim().to_string()
}

fn to_repo_config(obj: &serde_json::Map<String, serde_json::Value>) -> Option<RepoConfig> {
    let url = clean_string(obj.get("url").unwrap_or(&serde_json::Value::Null));
    if url.is_empty() {
        return None;
    }
    let branch = clean_string(obj.get("branch").unwrap_or(&serde_json::Value::Null));
    let branch = if branch.is_empty() {
        default_branch()
    } else {
        branch
    };
    let setup = clean_string(obj.get("setupCommand").unwrap_or(&serde_json::Value::Null));
    Some(RepoConfig {
        url,
        branch,
        setup_command: if setup.is_empty() { None } else { Some(setup) },
    })
}

pub fn parse_bot_repo(raw: Option<&str>) -> Option<RepoConfig> {
    let raw = raw?;
    if raw.trim().is_empty() {
        return None;
    }
    let parsed: serde_json::Value = serde_json::from_str(raw).ok()?;
    let obj = parsed.as_object()?;
    to_repo_config(obj)
}

pub fn sanitize_bot_repo(input: &serde_json::Value) -> Option<RepoConfig> {
    let obj = input.as_object()?;
    to_repo_config(obj)
}

pub fn ensure_bot_repo_column(db: &Db) -> rusqlite::Result<()> {
    let exists = {
        let mut stmt = db.conn().prepare("PRAGMA table_info(bots)")?;
        stmt.query_map([], |row| row.get::<_, String>(1))?
            .filter_map(Result::ok)
            .any(|name| name == "repo")
    };
    if !exists {
        db.conn()
            .execute_batch("ALTER TABLE bots ADD COLUMN repo TEXT")?;
    }
    Ok(())
}

pub fn get_bot_repo(db: &Db, bot_id: &str) -> rusqlite::Result<Option<RepoConfig>> {
    let raw: Option<String> = db
        .conn()
        .query_row(
            "SELECT repo FROM bots WHERE id = ?1",
            rusqlite::params![bot_id],
            |row| row.get(0),
        )
        .optional()?
        .flatten();
    Ok(parse_bot_repo(raw.as_deref()))
}

pub fn set_bot_repo(
    db: &Db,
    bot_id: &str,
    input: &serde_json::Value,
) -> rusqlite::Result<Option<RepoConfig>> {
    let clean = if input.is_null() {
        None
    } else {
        sanitize_bot_repo(input)
    };
    let stored = clean
        .as_ref()
        .map(serde_json::to_string)
        .transpose()
        .map_err(|e| rusqlite::Error::ToSqlConversionFailure(Box::new(e)))?;
    db.conn().execute(
        "UPDATE bots SET repo = ?1 WHERE id = ?2",
        rusqlite::params![stored, bot_id],
    )?;
    get_bot_repo(db, bot_id)
}

pub fn repo_host(url: &str) -> Option<String> {
    let trimmed = url.trim();
    if trimmed.starts_with("http://") || trimmed.starts_with("https://") {
        let rest = trimmed.split("//").nth(1)?;
        let authority = rest.split('/').next()?;
        let host_part = authority.rsplit('@').next()?;
        let host = host_part.split(':').next()?.trim();
        if host.is_empty() {
            None
        } else {
            Some(host.to_lowercase())
        }
    } else if let Some(at) = trimmed.find('@') {
        let after = &trimmed[at + 1..];
        let colon = after.find(':')?;
        let host = after[..colon].trim();
        if host.is_empty() {
            None
        } else {
            Some(host.to_lowercase())
        }
    } else {
        None
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EgressCheck {
    pub ok: bool,
    pub reason: String,
}

pub fn check_repo_egress(
    bot_egress: &BotEgressConfig,
    host: &str,
    egress_env: Option<&str>,
) -> EgressCheck {
    if !egress::egress_enabled(egress_env) {
        return EgressCheck {
            ok: false,
            reason: format!(
                "Sandbox internet is off for every bot on this server (BULLPEN_SANDBOX_EGRESS). \
Turn that on, then allow {host} in this bot's \"Internet from its sandbox\" card."
            ),
        };
    }
    if bot_egress.mode != EgressMode::Allowlist {
        return EgressCheck {
            ok: false,
            reason: format!(
                "This bot's sandbox has no internet. Open its \"Internet from its sandbox\" card, \
switch to Allow these hosts, and add {host}."
            ),
        };
    }
    let policy = EgressPolicy {
        allow: bot_egress.allow.clone(),
    };
    if !egress::host_allowed(host, &policy) {
        return EgressCheck {
            ok: false,
            reason: format!(
                "{host} is not on this bot's allow list. Open its \"Internet from its sandbox\" \
card and add {host} (or .{host})."
            ),
        };
    }
    EgressCheck {
        ok: true,
        reason: "allowed".to_string(),
    }
}

pub fn sh_quote(value: &str) -> String {
    format!("'{}'", value.replace('\'', "'\\''"))
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CheckoutResult {
    pub ok: bool,
    pub detail: String,
}

pub async fn ensure_checkout(
    sandbox: &dyn Sandbox,
    bot_id: &str,
    repo: &RepoConfig,
) -> CheckoutResult {
    let exists = sandbox
        .exec(bot_id, "test -d /work/repo/.git && echo yes || echo no")
        .await;
    if exists.stdout.trim() == "yes" {
        return CheckoutResult {
            ok: true,
            detail: "already checked out".to_string(),
        };
    }

    let clone_cmd = format!(
        "git clone --branch {} --single-branch {} /work/repo",
        sh_quote(&repo.branch),
        sh_quote(&repo.url)
    );
    let clone = sandbox.exec_timeout(bot_id, &clone_cmd, 120_000).await;
    if clone.exit_code != 0 {
        let msg = if !clone.stderr.trim().is_empty() {
            clone.stderr.trim()
        } else if !clone.stdout.trim().is_empty() {
            clone.stdout.trim()
        } else {
            "clone failed"
        };
        return CheckoutResult {
            ok: false,
            detail: format!("Could not clone {}: {msg}", repo.url),
        };
    }

    if let Some(setup) = &repo.setup_command {
        let setup_cmd = format!("cd /work/repo && {setup}");
        let setup = sandbox.exec_timeout(bot_id, &setup_cmd, 180_000).await;
        if setup.exit_code != 0 {
            let msg = if !setup.stderr.trim().is_empty() {
                setup.stderr.trim()
            } else {
                setup.stdout.trim()
            };
            return CheckoutResult {
                ok: false,
                detail: format!(
                    "Checked out, but setupCommand failed (exit {}): {msg}",
                    setup.exit_code
                ),
            };
        }
    }

    CheckoutResult {
        ok: true,
        detail: "cloned".to_string(),
    }
}

pub async fn prepare_repo(
    sandbox: &dyn Sandbox,
    bot_id: &str,
    repo: &RepoConfig,
    bot_egress: &BotEgressConfig,
    egress_env: Option<&str>,
) -> Result<(), String> {
    let host = repo_host(&repo.url)
        .ok_or_else(|| format!("\"{}\" is not a URL this can check out.", repo.url))?;
    let egress = check_repo_egress(bot_egress, &host, egress_env);
    if !egress.ok {
        return Err(egress.reason);
    }
    let checkout = ensure_checkout(sandbox, bot_id, repo).await;
    if !checkout.ok {
        return Err(checkout.detail);
    }
    Ok(())
}

pub fn repo_path(path: &str) -> Option<String> {
    let p = path.trim().trim_start_matches("./");
    if p.is_empty() || p.starts_with('/') || p.split('/').any(|seg| seg == "..") {
        return None;
    }
    Some(format!("/work/repo/{p}"))
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RepoReadResult {
    pub ok: bool,
    pub content: String,
    pub detail: String,
}

pub async fn read_repo_file(sandbox: &dyn Sandbox, bot_id: &str, path: &str) -> RepoReadResult {
    let Some(target) = repo_path(path) else {
        return RepoReadResult {
            ok: false,
            content: String::new(),
            detail: format!("\"{path}\" is outside the repo. Refused."),
        };
    };

    let cmd = format!("base64 {} 2>&1", sh_quote(&target));
    let result = sandbox.exec(bot_id, &cmd).await;
    if result.exit_code != 0 {
        let detail = if !result.stdout.trim().is_empty() {
            result.stdout.trim().to_string()
        } else if !result.stderr.trim().is_empty() {
            result.stderr.trim().to_string()
        } else {
            format!("exit {}", result.exit_code)
        };
        return RepoReadResult {
            ok: false,
            content: String::new(),
            detail,
        };
    }
    let content = BASE64
        .decode(result.stdout.trim())
        .ok()
        .and_then(|bytes| String::from_utf8(bytes).ok())
        .unwrap_or_default();
    RepoReadResult {
        ok: true,
        content,
        detail: String::new(),
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RepoBranchResult {
    pub ok: bool,
    pub detail: String,
}

pub async fn repo_branch(sandbox: &dyn Sandbox, bot_id: &str, name: &str) -> RepoBranchResult {
    let clean = name.trim();
    if clean.is_empty() {
        return RepoBranchResult {
            ok: false,
            detail: "No branch name was given.".to_string(),
        };
    }

    let check_cmd = format!(
        "cd /work/repo && git rev-parse --verify {} >/dev/null 2>&1 && echo exists || echo new",
        sh_quote(clean)
    );
    let check = sandbox.exec(bot_id, &check_cmd).await;
    let exists = check.stdout.trim() == "exists";

    let checkout_cmd = if exists {
        format!("cd /work/repo && git checkout {}", sh_quote(clean))
    } else {
        format!("cd /work/repo && git checkout -b {}", sh_quote(clean))
    };
    let result = sandbox.exec(bot_id, &checkout_cmd).await;
    if result.exit_code != 0 {
        let detail = if !result.stderr.trim().is_empty() {
            result.stderr.trim().to_string()
        } else if !result.stdout.trim().is_empty() {
            result.stdout.trim().to_string()
        } else {
            format!("exit {}", result.exit_code)
        };
        return RepoBranchResult { ok: false, detail };
    }
    RepoBranchResult {
        ok: true,
        detail: if exists {
            format!("Switched to {clean}.")
        } else {
            format!("Created and switched to {clean}.")
        },
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RepoEdit {
    pub find: String,
    pub replace: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RepoEditResult {
    pub ok: bool,
    pub detail: String,
}

fn count_occurrences(haystack: &str, needle: &str) -> usize {
    if needle.is_empty() {
        return 0;
    }
    haystack.match_indices(needle).count()
}

async fn write_repo_file(
    sandbox: &dyn Sandbox,
    bot_id: &str,
    path: &str,
    content: &str,
) -> crate::sandbox::ExecResult {
    let Some(target) = repo_path(path) else {
        return crate::sandbox::ExecResult {
            stdout: String::new(),
            stderr: format!("\"{path}\" is outside the repo. Refused."),
            exit_code: 1,
            timed_out: false,
            truncated: false,
            unavailable: false,
        };
    };
    let b64 = BASE64.encode(content.as_bytes());
    let cmd = format!(
        "printf '%s' {} | base64 -d > {}",
        sh_quote(&b64),
        sh_quote(&target)
    );
    sandbox.exec(bot_id, &cmd).await
}

pub async fn apply_repo_edits(
    sandbox: &dyn Sandbox,
    bot_id: &str,
    path: &str,
    edits: &[RepoEdit],
) -> RepoEditResult {
    if edits.is_empty() {
        return RepoEditResult {
            ok: false,
            detail: "No edits were given.".to_string(),
        };
    }

    let read = read_repo_file(sandbox, bot_id, path).await;
    if !read.ok {
        return RepoEditResult {
            ok: false,
            detail: format!("Could not read {path}: {}", read.detail),
        };
    }

    let mut content = read.content;
    for (i, edit) in edits.iter().enumerate() {
        let occurrences = count_occurrences(&content, &edit.find);
        if occurrences == 0 {
            return RepoEditResult {
                ok: false,
                detail: format!(
                    "Edit {}: that text was not found in {path}. Nothing was changed.",
                    i + 1
                ),
            };
        }
        if occurrences > 1 {
            return RepoEditResult {
                ok: false,
                detail: format!(
                    "Edit {}: that text appears {occurrences} times in {path} - ambiguous. \
Make it unique with more surrounding context. Nothing was changed.",
                    i + 1
                ),
            };
        }
        content = content.replace(&edit.find, &edit.replace);
    }

    let written = write_repo_file(sandbox, bot_id, path, &content).await;
    if written.exit_code != 0 {
        let msg = if !written.stderr.is_empty() {
            written.stderr
        } else {
            written.stdout
        };
        return RepoEditResult {
            ok: false,
            detail: format!("Read ok, but the write failed: {msg}"),
        };
    }
    RepoEditResult {
        ok: true,
        detail: format!(
            "{} edit{} applied to {path}.",
            edits.len(),
            if edits.len() == 1 { "" } else { "s" }
        ),
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GithubRepo {
    pub owner: String,
    pub repo: String,
}

pub fn parse_github_repo(url: &str) -> Option<GithubRepo> {
    let cleaned = url.trim().trim_end_matches(".git");
    if let Some(cap) = regex::Regex::new(r"^https?://[^/]+/([^/]+)/([^/]+)/?$")
        .ok()?
        .captures(cleaned)
    {
        return Some(GithubRepo {
            owner: cap.get(1)?.as_str().to_string(),
            repo: cap.get(2)?.as_str().to_string(),
        });
    }
    if let Some(cap) = regex::Regex::new(r"^[\w.-]+@[\w.-]+:([^/]+)/([^/]+)$")
        .ok()?
        .captures(cleaned)
    {
        return Some(GithubRepo {
            owner: cap.get(1)?.as_str().to_string(),
            repo: cap.get(2)?.as_str().to_string(),
        });
    }
    None
}

const ACTION_KEYWORDS: &[(&str, &[&[&str]])] = &[
    (
        "create",
        &[&["create", "pull"], &["create", "pull", "request"]],
    ),
    ("update", &[&["update", "pull"], &["edit", "pull"]]),
    ("comment", &[&["comment"]]),
    ("labels", &[&["label"]]),
    ("ci_status", &[&["status"], &["check"], &["workflow"]]),
];

pub fn pick_github_tool<'a>(
    tools: &'a [crate::mcp::ConnectorTool],
    action: &str,
) -> Option<&'a crate::mcp::ConnectorTool> {
    let groups = ACTION_KEYWORDS
        .iter()
        .find(|(a, _)| *a == action)
        .map(|(_, g)| *g)?;
    for words in groups {
        if let Some(hit) = tools.iter().find(|t| {
            let name = t.name.to_lowercase();
            words.iter().all(|w| name.contains(w))
        }) {
            return Some(hit);
        }
    }
    None
}

pub struct PushInput<'a> {
    pub repo_url: &'a str,
    pub base: &'a str,
    pub branch: &'a str,
    pub patch_text: &'a str,
    pub token: Option<&'a str>,
}

pub struct PushResult {
    pub ok: bool,
    pub detail: String,
}

#[async_trait::async_trait]
pub trait GitRunner: Send + Sync {
    async fn run(
        &self,
        args: &[&str],
        cwd: &str,
        env: &[(String, String)],
    ) -> Result<(String, String), String>;
}

struct RealGit;

#[async_trait::async_trait]
impl GitRunner for RealGit {
    async fn run(
        &self,
        args: &[&str],
        cwd: &str,
        env: &[(String, String)],
    ) -> Result<(String, String), String> {
        use std::process::Stdio;
        use tokio::process::Command;
        let mut cmd = Command::new("git");
        cmd.args(args)
            .current_dir(cwd)
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        for (k, v) in env {
            cmd.env(k, v);
        }
        let output = cmd.output().await.map_err(|e| e.to_string())?;
        let stdout = String::from_utf8_lossy(&output.stdout).to_string();
        let stderr = String::from_utf8_lossy(&output.stderr).to_string();
        if output.status.success() {
            Ok((stdout, stderr))
        } else {
            Err(if stderr.is_empty() { stdout } else { stderr })
        }
    }
}

fn with_username(url: &str, username: &str) -> String {
    let trimmed = url.trim();
    if let Some(rest) = trimmed.strip_prefix("https://")
        && !rest.contains('@')
    {
        return format!("https://{username}@{rest}");
    }
    if let Some(rest) = trimmed.strip_prefix("http://")
        && !rest.contains('@')
    {
        return format!("http://{username}@{rest}");
    }
    url.to_string()
}

pub async fn push_patch(input: PushInput<'_>, git: &dyn GitRunner) -> PushResult {
    if input.patch_text.trim().is_empty() {
        return PushResult {
            ok: false,
            detail: format!(
                "No commits ahead of {}. Commit your changes with repo_run first.",
                input.base
            ),
        };
    }
    let Some(token) = input.token.filter(|t| !t.is_empty()) else {
        return PushResult {
            ok: false,
            detail: "GitHub is not authorized. Tell Josh to open Connectors and press Connect on GitHub.".to_string(),
        };
    };

    let dir_path = std::env::temp_dir().join(format!("bullpen-repo-push-{}", uuid::Uuid::new_v4()));
    if std::fs::create_dir_all(&dir_path).is_err() {
        return PushResult {
            ok: false,
            detail: "Could not create a temp directory for git.".to_string(),
        };
    }
    let askpass_path = dir_path.join(if cfg!(windows) {
        "askpass.cmd"
    } else {
        "askpass.sh"
    });
    let patch_path = dir_path.join("changes.patch");
    let remote_url = with_username(input.repo_url, "x-access-token");

    let askpass_body = if cfg!(windows) {
        "@echo off\r\necho %BULLPEN_GIT_TOKEN%\r\n".to_string()
    } else {
        "#!/bin/sh\nprintf '%s' \"$BULLPEN_GIT_TOKEN\"\n".to_string()
    };
    if let Err(e) = std::fs::write(&askpass_path, askpass_body) {
        return PushResult {
            ok: false,
            detail: format!("Could not write askpass script: {e}"),
        };
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(&askpass_path, std::fs::Permissions::from_mode(0o700));
    }
    if let Err(e) = std::fs::write(&patch_path, input.patch_text) {
        return PushResult {
            ok: false,
            detail: format!("Could not write patch file: {e}"),
        };
    }

    let mut env: Vec<(String, String)> = std::env::vars().collect();
    env.push(("GIT_TERMINAL_PROMPT".into(), "0".into()));
    env.push((
        "GIT_ASKPASS".into(),
        askpass_path.to_string_lossy().into_owned(),
    ));
    env.push(("BULLPEN_GIT_TOKEN".into(), token.to_string()));
    let cwd = dir_path.to_string_lossy().into_owned();
    let dir_arg = cwd.as_str();
    let patch_arg = patch_path.to_string_lossy().into_owned();
    let branch_ref = format!("HEAD:refs/heads/{}", input.branch);

    if let Err(e) = git.run(&["init", "-q"], &cwd, &env).await {
        return scrub_push_err(e, token);
    }
    if let Err(e) = git
        .run(
            &["-C", dir_arg, "remote", "add", "origin", &remote_url],
            &cwd,
            &env,
        )
        .await
    {
        return scrub_push_err(e, token);
    }
    if let Err(e) = git
        .run(
            &[
                "-C", dir_arg, "fetch", "--depth", "50", "origin", input.base,
            ],
            &cwd,
            &env,
        )
        .await
    {
        return scrub_push_err(e, token);
    }
    if let Err(e) = git
        .run(
            &["-C", dir_arg, "checkout", "-B", input.branch, "FETCH_HEAD"],
            &cwd,
            &env,
        )
        .await
    {
        return scrub_push_err(e, token);
    }
    if let Err(e) = git
        .run(
            &[
                "-C",
                dir_arg,
                "-c",
                "user.email=bullpen@rainmade.local",
                "-c",
                "user.name=Bullpen",
                "am",
                &patch_arg,
            ],
            &cwd,
            &env,
        )
        .await
    {
        let _ = git.run(&["-C", dir_arg, "am", "--abort"], &cwd, &env).await;
        return scrub_push_err(e, token);
    }
    if let Err(e) = git
        .run(&["-C", dir_arg, "push", "origin", &branch_ref], &cwd, &env)
        .await
    {
        return scrub_push_err(e, token);
    }
    let _ = std::fs::remove_dir_all(&dir_path);

    PushResult {
        ok: true,
        detail: format!("Pushed {} (base {}).", input.branch, input.base),
    }
}

fn scrub_push_err(message: String, token: &str) -> PushResult {
    let scrubbed = message.split(token).collect::<Vec<_>>().join("[redacted]");
    let first = scrubbed.lines().next().unwrap_or(&scrubbed).to_string();
    PushResult {
        ok: false,
        detail: format!("git failed: {first}"),
    }
}

pub async fn push_patch_default(input: PushInput<'_>) -> PushResult {
    push_patch(input, &RealGit).await
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::egress::{BotEgressConfig, EgressMode};

    #[test]
    fn parse_bot_repo_rejects_garbage() {
        assert!(parse_bot_repo(None).is_none());
        assert!(parse_bot_repo(Some("")).is_none());
        assert!(parse_bot_repo(Some("not json")).is_none());
        assert!(parse_bot_repo(Some(r#""just a string""#)).is_none());
    }

    #[test]
    fn parse_bot_repo_defaults_branch() {
        let cfg = parse_bot_repo(Some(r#"{"url":"https://github.com/a/b"}"#)).unwrap();
        assert_eq!(cfg.branch, "main");
        assert!(cfg.setup_command.is_none());
    }

    #[test]
    fn sanitize_requires_url() {
        assert!(sanitize_bot_repo(&serde_json::json!({})).is_none());
        assert!(sanitize_bot_repo(&serde_json::json!({"url": "  "})).is_none());
    }

    #[test]
    fn repo_host_and_path() {
        assert_eq!(
            repo_host("https://github.com/rainmade/bullpen").as_deref(),
            Some("github.com")
        );
        assert_eq!(
            repo_host("git@github.com:rainmade/bullpen.git").as_deref(),
            Some("github.com")
        );
        assert_eq!(
            repo_path("src/index.ts").as_deref(),
            Some("/work/repo/src/index.ts")
        );
        assert!(repo_path("/etc/passwd").is_none());
        assert!(repo_path("src/../../etc/passwd").is_none());
    }

    #[test]
    fn check_repo_egress_names_the_off_switch() {
        let off = check_repo_egress(
            &BotEgressConfig {
                mode: EgressMode::Off,
                allow: vec![],
            },
            "github.com",
            None,
        );
        assert!(!off.ok);
        assert!(off.reason.contains("BULLPEN_SANDBOX_EGRESS"));

        let bot_off = check_repo_egress(
            &BotEgressConfig {
                mode: EgressMode::Off,
                allow: vec![],
            },
            "github.com",
            Some("on"),
        );
        assert!(!bot_off.ok);
        assert!(bot_off.reason.contains("Internet from its sandbox"));

        let ok = check_repo_egress(
            &BotEgressConfig {
                mode: EgressMode::Allowlist,
                allow: vec![".github.com".into()],
            },
            "github.com",
            Some("on"),
        );
        assert!(ok.ok);
    }

    #[test]
    fn parse_github_repo_reads_owner_repo() {
        let g = parse_github_repo("https://github.com/rainmade/bullpen").unwrap();
        assert_eq!(g.owner, "rainmade");
        assert_eq!(g.repo, "bullpen");
        assert!(parse_github_repo("not a url").is_none());
    }

    #[test]
    fn pick_github_tool_matches_connector_names() {
        use crate::mcp::ConnectorTool;
        let tools = vec![
            ConnectorTool {
                name: "create_pull_request".into(),
                description: String::new(),
                input_schema: serde_json::json!({}),
            },
            ConnectorTool {
                name: "add_issue_comment".into(),
                description: String::new(),
                input_schema: serde_json::json!({}),
            },
        ];
        assert_eq!(
            pick_github_tool(&tools, "create").map(|t| t.name.as_str()),
            Some("create_pull_request")
        );
        assert_eq!(
            pick_github_tool(&tools, "comment").map(|t| t.name.as_str()),
            Some("add_issue_comment")
        );
    }
}
