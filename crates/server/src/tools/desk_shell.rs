//! `desk_shell`: a command on the calling bot's own persistent machine, in
//! `/workspace`. Port of TS `deskShell` (`bullpen-night/src/server/desk.ts:402-434`,
//! `MAX_SHELL_BYTES`/`cap()` at `:390`/`:436`).
//!
//! 🔴 **NOT the gVisor sandbox** (`crate::sandbox`, the `shell` tool). This
//! machine is persistent, has a real route to the internet, and carries the
//! shared browser profile - `permissions.rs:268` keeps this `Ask`, never
//! `Allow`, for exactly that reason. Do not change that row from here.
//!
//! Routing (S8b-02's own decision, following `desk::cdp_for_bot`'s
//! established pattern rather than reinventing one): the CALLING bot's own
//! `DeskConfig` is resolved by `desk::desk_config_for_bot` in the
//! `tools/mod.rs` dispatch arm, exactly the way `browse`/`read_page` resolve
//! their `Cdp` there - this module never touches the database or `vm::`
//! itself, it only runs a command against a `DeskConfig` it is handed.

use model::ToolSpec;
use serde::Deserialize;
use serde_json::json;
use store::vms::DockerResult;

use crate::desk::DeskConfig;
use crate::vm::DockerRun;

use super::fence_tool_output;

/// Spec text ported VERBATIM from TS `app.ts:5214-5222` - the wording is
/// what a model reads to decide when to call this tool, not this ticket's
/// to reword.
pub fn desk_shell_spec() -> ToolSpec {
    ToolSpec {
        name: "desk_shell".to_string(),
        description: "Run a command on the shared computer, in /workspace. This machine has \
the internet and keeps its files between runs, unlike `shell` which is a throwaway sandbox with \
no network. Use it to save what you find so another bot can pick it up."
            .to_string(),
        parameters: json!({
            "type": "object",
            "properties": {
                "command": { "type": "string", "description": "A shell command." }
            },
            "required": ["command"]
        }),
    }
}

#[derive(Deserialize, Default)]
struct DeskShellArgs {
    #[serde(default)]
    command: String,
}

/// TS `MAX_SHELL_BYTES` (`desk.ts:390`). Named for the TS constant it
/// mirrors, but - same reasoning `desk::MAX_PAGE_CHARS`'s own doc gives, and
/// for the identical reason - this counts Unicode scalar values (`char`),
/// never bytes: shell output routinely carries multi-byte UTF-8 (a `git
/// log` author name, a linter's curly quotes), and a byte-based cap either
/// panics slicing mid-character or truncates short of what this promises.
/// TS's own `text.length` counts UTF-16 code units, which is a different
/// number again for anything outside the Basic Multilingual Plane - not
/// reproduced here on purpose, for the same char-safety reason.
pub const MAX_SHELL_BYTES: usize = 20_000;

/// TS `deskShell`'s own default (`desk.ts:405`).
const DEFAULT_TIMEOUT_MS: u64 = 60_000;

/// Port of TS `cap()` (`desk.ts:436-440`): untruncated output is trimmed;
/// truncated output gets the marker appended with no trim (matches TS's
/// ternary exactly - trimming only happens on the branch that does not
/// truncate).
fn cap(text: &str) -> String {
    let char_count = text.chars().count();
    if char_count <= MAX_SHELL_BYTES {
        return text.trim().to_string();
    }
    let kept: String = text.chars().take(MAX_SHELL_BYTES).collect();
    format!("{kept}\n[\u{2026}output truncated]")
}

/// Runs `desk_shell` against an ALREADY-RESOLVED `DeskConfig` - the calling
/// bot's own machine, resolved by `desk::desk_config_for_bot` at the
/// `tools/mod.rs` call site (see this module's own header doc for why the
/// resolution does not happen in here).
///
/// Shape: `docker exec -u abc -w /workspace -e HOME=/config <container>
/// bash -lc <command>`, exactly TS's own argv (`desk.ts:409-422`).
/// `vm::DockerRun::call` takes `&[&str]` and hands each element straight to
/// `tokio::process::Command` as one literal argv entry (`sandbox.rs`'s
/// `TokioRunner::run` -> `cmd.args(&argv[1..])`) - no shell re-parses this
/// slice, so `command` reaches `bash -lc` as ONE argument regardless of the
/// spaces, quotes or embedded newlines inside it, the same as any other
/// multi-word argument this crate already passes through `CommandRunner`
/// (`sandbox.rs`'s own `command.to_string()` as the tail of `sh -c`
/// argv). No change to `DockerRun` was needed - see this ticket's Result
/// for the full answer to Decision 1.
///
/// 🔴 Bite (b) target: both branches of the match below read `result.stdout`
/// AND `result.stderr` into the same combined string, exactly like TS's
/// `${stdout}${stderr}` (success) and `${e.stdout ?? ""}${e.stderr ?? ""}`
/// (failure) - `vm::DockerRun::call` never throws (unlike TS's underlying
/// `run()`, modelled on a promisified `child_process.exec`), it always hands
/// back a `DockerResult{ok, stdout, stderr}`, so there is no separate
/// catch-and-swallow branch to get wrong: a failing command's stderr is
/// already sitting in the same struct field a successful command's would be.
pub async fn run_desk_shell(docker: &dyn DockerRun, config: &DeskConfig, args: &str) -> String {
    let parsed: DeskShellArgs = serde_json::from_str(args).unwrap_or_default();
    let command = parsed.command.trim();
    if command.is_empty() {
        return "No command was given.".to_string();
    }

    let DockerResult { stdout, stderr, .. } = docker
        .call(
            &[
                "exec",
                "-u",
                "abc",
                "-w",
                "/workspace",
                "-e",
                "HOME=/config",
                &config.container,
                "bash",
                "-lc",
                command,
            ],
            DEFAULT_TIMEOUT_MS,
        )
        .await;

    let combined = cap(&format!("{stdout}{stderr}"));
    if combined.is_empty() {
        // TS `app.ts:7120`: `result.output === "" ? "(no output)" : ...`.
        // Server-generated, not machine-derived - unfenced, same as every
        // other refusal string in this module.
        return "(no output)".to_string();
    }

    // Decision 3: arbitrary text off a machine with a network route MUST be
    // marked as data before it ever reaches a prompt - same fence
    // `browse`/`read_page`/`click`/`type_text` already put around page text
    // (`tools/browse.rs`'s own header doc).
    fence_tool_output(&combined)
}

#[cfg(test)]
mod tests {
    use super::*;
    use async_trait::async_trait;
    use std::sync::Mutex;

    fn config() -> DeskConfig {
        DeskConfig {
            cdp: "http://127.0.0.1:9400".to_string(),
            view: "http://127.0.0.1:6400".to_string(),
            container: "bullpen-vm-arthur".to_string(),
            docker_host: "unix:///test.sock".to_string(),
        }
    }

    /// Records the exact argv it was called with and returns one scripted
    /// `DockerResult`.
    struct ScriptedDocker {
        calls: Mutex<Vec<Vec<String>>>,
        response: DockerResult,
    }

    impl ScriptedDocker {
        fn new(ok: bool, stdout: &str, stderr: &str) -> Self {
            Self {
                calls: Mutex::new(Vec::new()),
                response: DockerResult {
                    ok,
                    stdout: stdout.to_string(),
                    stderr: stderr.to_string(),
                },
            }
        }

        fn calls(&self) -> Vec<Vec<String>> {
            self.calls.lock().unwrap().clone()
        }
    }

    #[async_trait]
    impl DockerRun for ScriptedDocker {
        async fn call(&self, args: &[&str], _timeout_ms: u64) -> DockerResult {
            self.calls
                .lock()
                .unwrap()
                .push(args.iter().map(|s| s.to_string()).collect());
            DockerResult {
                ok: self.response.ok,
                stdout: self.response.stdout.clone(),
                stderr: self.response.stderr.clone(),
            }
        }
    }

    #[tokio::test]
    async fn runs_the_command_in_the_bots_own_container_and_fences_the_output() {
        let docker = ScriptedDocker::new(true, "hello\n", "");
        let cfg = config();

        let result = run_desk_shell(&docker, &cfg, r#"{"command":"echo hello"}"#).await;

        assert!(result.contains("<<<TOOL_OUTPUT_DATA>>>"));
        assert!(result.contains("hello"));
        let calls = docker.calls();
        assert_eq!(calls.len(), 1);
        assert!(calls[0].contains(&"bullpen-vm-arthur".to_string()));
        assert!(calls[0].contains(&"echo hello".to_string()));
        assert_eq!(calls[0][0], "exec");
    }

    /// 🔴 BITE (b) target - see this ticket's Result for the literal
    /// red/green. GUARD-PRESENT (this test): a non-zero exit's stderr
    /// reaches the model. GUARD-REMOVED: `run_desk_shell` hands back a
    /// generic failure string instead of `combined`.
    #[tokio::test]
    async fn a_failing_command_returns_its_own_stderr() {
        let docker = ScriptedDocker::new(false, "", "bash: nope: command not found\n");
        let cfg = config();

        let result = run_desk_shell(&docker, &cfg, r#"{"command":"nope"}"#).await;

        assert!(
            result.contains("bash: nope: command not found"),
            "expected the command's own stderr to reach the model, got {result:?}"
        );
        assert!(result.contains("<<<TOOL_OUTPUT_DATA>>>"));
    }

    #[tokio::test]
    async fn empty_output_reads_as_no_output_and_is_not_fenced() {
        let docker = ScriptedDocker::new(true, "", "");
        let cfg = config();

        let result = run_desk_shell(&docker, &cfg, r#"{"command":"true"}"#).await;

        assert_eq!(result, "(no output)");
    }

    #[tokio::test]
    async fn empty_command_is_refused_before_touching_docker() {
        let docker = ScriptedDocker::new(true, "should not run", "");
        let cfg = config();

        let result = run_desk_shell(&docker, &cfg, r#"{"command":""}"#).await;

        assert_eq!(result, "No command was given.");
        assert!(docker.calls().is_empty());
    }

    #[tokio::test]
    async fn output_past_the_cap_is_truncated_with_a_marker() {
        let long = "a".repeat(MAX_SHELL_BYTES + 500);
        let docker = ScriptedDocker::new(true, &long, "");
        let cfg = config();

        let result = run_desk_shell(&docker, &cfg, r#"{"command":"yes a"}"#).await;

        assert!(result.contains("[\u{2026}output truncated]"));
    }

    #[tokio::test]
    async fn multiline_and_quoted_commands_reach_docker_as_one_argument() {
        let docker = ScriptedDocker::new(true, "ok\n", "");
        let cfg = config();
        let command = "echo \"a 'b' c\"\nls -la";

        run_desk_shell(
            &docker,
            &cfg,
            &serde_json::json!({ "command": command }).to_string(),
        )
        .await;

        let calls = docker.calls();
        assert_eq!(
            calls[0].last().map(|s| s.as_str()),
            Some(command),
            "the multi-line, quoted command must reach docker unmangled, as one argv element"
        );
    }
}
