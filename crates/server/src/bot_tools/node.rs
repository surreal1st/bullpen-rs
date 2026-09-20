//! Invokes `crates/server/w5/run.mjs` against bullpen-night's bot-tools.ts.

use std::path::PathBuf;
use std::process::Stdio;

use serde::Deserialize;
use store::ToolProposal;

fn night_root() -> PathBuf {
    if let Ok(root) = std::env::var("BULLPEN_NIGHT_ROOT") {
        return PathBuf::from(root);
    }
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../../bullpen-night")
}

fn run_script() -> PathBuf {
    if let Ok(path) = std::env::var("BULLPEN_W5_SCRIPT") {
        return PathBuf::from(path);
    }
    if let Ok(exe) = std::env::current_exe()
        && let Some(dir) = exe.parent()
    {
        let beside_binary = dir.join("w5/run.mjs");
        if beside_binary.is_file() {
            return beside_binary;
        }
    }
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("w5/run.mjs")
}

fn invoke_sync(op: &str, payload: serde_json::Value) -> Result<serde_json::Value, String> {
    let night = night_root();
    let script = run_script();
    let shell_cmd = format!(
        "cd '{}' && npx --yes tsx '{}' {}",
        night.display(),
        script.display(),
        op
    );
    let body = payload.to_string();
    let output = std::process::Command::new("bash")
        .arg("-lc")
        .arg(&shell_cmd)
        .env("BULLPEN_NIGHT_ROOT", &night)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .and_then(|mut child| {
            if let Some(mut stdin) = child.stdin.take() {
                use std::io::Write;
                stdin
                    .write_all(body.as_bytes())
                    .map_err(std::io::Error::other)?;
            }
            child.wait_with_output()
        })
        .map_err(|e| format!("spawn w5 runner: {e}"))?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(format!(
            "w5 runner exited {}: {}",
            output.status,
            stderr.trim()
        ));
    }
    let stdout = String::from_utf8_lossy(&output.stdout);
    let value: serde_json::Value =
        serde_json::from_str(stdout.trim()).map_err(|e| format!("parse runner output: {e}"))?;
    if let Some(err) = value.get("error").and_then(|v| v.as_str()) {
        return Err(err.to_string());
    }
    Ok(value)
}

async fn invoke(op: &str, payload: serde_json::Value) -> Result<serde_json::Value, String> {
    let op = op.to_string();
    tokio::task::spawn_blocking(move || invoke_sync(&op, payload))
        .await
        .map_err(|e| format!("w5 worker join: {e}"))?
}

pub async fn prepare_proposal(
    db_path: &str,
    data_dir: &str,
    bot_id: &str,
    args: &serde_json::Value,
) -> Result<ToolProposal, String> {
    let value = invoke(
        "prepare-persist",
        serde_json::json!({
            "dbPath": db_path,
            "dataDir": data_dir,
            "botId": bot_id,
            "args": args,
        }),
    )
    .await?;
    let proposal = value
        .get("proposal")
        .cloned()
        .ok_or_else(|| "runner returned no proposal".to_string())?;
    serde_json::from_value(proposal).map_err(|e| format!("decode proposal: {e}"))
}

pub async fn describe_proposal(proposal: &ToolProposal) -> Result<String, String> {
    let value = invoke("describe", serde_json::json!({ "proposal": proposal })).await?;
    Ok(value
        .get("text")
        .and_then(|v| v.as_str())
        .unwrap_or_default()
        .to_string())
}

pub async fn run_bot_tool(
    db_path: &str,
    bot_id: &str,
    name: &str,
    args: &serde_json::Value,
) -> Result<String, String> {
    let value = invoke(
        "run-tool",
        serde_json::json!({
            "dbPath": db_path,
            "botId": bot_id,
            "name": name,
            "args": args,
        }),
    )
    .await?;
    Ok(value
        .get("result")
        .and_then(|v| v.as_str())
        .unwrap_or_default()
        .to_string())
}

#[derive(Debug, Deserialize)]
pub struct GuardVerdict {
    pub ok: bool,
    pub refusal: String,
}

#[allow(dead_code)]
pub fn guard_source_sync(source: &str) -> Result<GuardVerdict, String> {
    let value = invoke_sync("guard", serde_json::json!({ "source": source }))?;
    serde_json::from_value(value).map_err(|e| format!("decode guard verdict: {e}"))
}

pub async fn guard_source(source: &str) -> Result<GuardVerdict, String> {
    let value = invoke("guard", serde_json::json!({ "source": source })).await?;
    serde_json::from_value(value).map_err(|e| format!("decode guard verdict: {e}"))
}
