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
}
