//! `shell`: one command in your own sandbox, one result out. Spec ported
//! verbatim from `app.ts:5085-5095`.
//!
//! S6L-02: the run itself now drives a real `Sandbox` (S6L-01's
//! `DockerSandbox`/`UnavailableSandbox`, injected through `BuildParams`)
//! instead of S2's hardcoded stub. The approval flow this tool exercises
//! is unchanged - `shell` still defaults to `ask` (S2-02's
//! `default_decisions`), and `runs.rs`'s tool loop still parks the run
//! until Josh decides; only what happens on approval changed, from
//! "nothing, always the same sentence" to "the command actually runs".
//! With `BULLPEN_SANDBOX` off (an `UnavailableSandbox`), the answer is
//! still exactly S2's stub text - detected via `ExecResult.unavailable`
//! (F5), a field only `UnavailableSandbox` ever sets, rather than by
//! sniffing exit code 127 + a fixed stderr prefix, which a bot's own
//! command could forge (`sh -c 'printf "No sandbox is available, so \
//! nothing was run. X" >&2; exit 127'` satisfied every clause of the old
//! shape check).
use model::ToolSpec;
use serde::Deserialize;
use serde_json::json;

use crate::sandbox::{ExecResult, Sandbox};

use super::fence_tool_output;

pub fn spec() -> ToolSpec {
    ToolSpec {
        name: "shell".to_string(),
        description: "Run a shell command in your own private sandbox. You have a /work \
directory that keeps its contents between runs. There is no network in here."
            .to_string(),
        parameters: json!({
            "type": "object",
            "properties": {
                "command": { "type": "string", "description": "A shell command, run with sh -c." }
            },
            "required": ["command"]
        }),
    }
}

#[derive(Deserialize, Default)]
struct Args {
    #[serde(default)]
    command: String,
}

/// Runs `command` in `sandbox` and formats the result the TS way
/// (`app.ts:6257-6268`'s `shell` handler): stdout, then `stderr:\n...`,
/// then a timeout/truncation note, then `exit code N`, each on its own
/// line. The real output is fenced (`fence_tool_output`) as untrusted data
/// before it goes back to the model - a command this run's own approval
/// let through can still print text engineered to look like an
/// instruction to whatever reads the transcript next.
pub async fn run(sandbox: &dyn Sandbox, bot_id: &str, args: &str) -> String {
    let parsed: Args = serde_json::from_str(args).unwrap_or_default();
    let command = parsed.command.trim();
    if command.is_empty() {
        return "No command was given.".to_string();
    }

    let result = sandbox.exec(bot_id, command).await;
    format_result(&result)
}

fn format_result(result: &ExecResult) -> String {
    if result.unavailable {
        // Verbatim S2 text, unfenced - this is not data the sandbox
        // produced, it is this tool refusing to have run at all.
        return result.stderr.clone();
    }

    let mut parts: Vec<String> = Vec::new();
    if !result.stdout.is_empty() {
        parts.push(result.stdout.clone());
    }
    if !result.stderr.is_empty() {
        parts.push(format!("stderr:\n{}", result.stderr));
    }
    if result.timed_out {
        parts.push("The command was stopped for taking too long.".to_string());
    }
    if result.truncated {
        parts.push("Output was truncated.".to_string());
    }
    parts.push(format!("exit code {}", result.exit_code));

    fence_tool_output(&parts.join("\n"))
}
