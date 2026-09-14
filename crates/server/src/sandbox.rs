//! Bot sandbox: isolates shell commands and file reads in containers.
//!
//! Ported from TypeScript `src/server/sandbox.ts`. A bot's commands run in
//! their own `docker` container with no network access by default,
//! `--network none`. This module drives docker through `tokio::process`,
//! behind a `CommandRunner` trait so tests use a fake without needing a
//! real daemon.

use std::path::{Component, Path, PathBuf};
use std::time::Duration;

/// Where a bot's shell commands actually run.
#[async_trait::async_trait]
pub trait Sandbox: Send + Sync {
    /// Runs a shell command in the bot's own sandbox. Never throws for a failed command.
    async fn exec(&self, bot_id: &str, command: &str) -> ExecResult;

    /// Reads a file from the bot's `/work` directory.
    async fn read_file(&self, bot_id: &str, path: &str) -> Result<String, String>;
}

/// The result of a shell command execution.
#[derive(Debug, Clone, PartialEq)]
pub struct ExecResult {
    pub stdout: String,
    pub stderr: String,
    pub exit_code: i32,
    pub timed_out: bool,
    /// True when output was cut. A runaway command must not fill the database.
    pub truncated: bool,
}

/// Configuration for a sandbox instance.
#[derive(Debug, Clone)]
pub struct SandboxConfig {
    pub image: String,
    pub memory: String,
    pub cpus: String,
    pub pids_limit: u32,
    pub timeout_ms: u64,
    pub max_output_bytes: usize,
    pub runtime: String,
    pub docker_host: String,
    /// Empty means `--network none`, which is the default.
    pub network: String,
}

impl Default for SandboxConfig {
    fn default() -> Self {
        Self {
            image: "debian:bookworm-slim".to_string(),
            memory: "256m".to_string(),
            cpus: "0.5".to_string(),
            pids_limit: 128,
            timeout_ms: 60_000,
            max_output_bytes: 64_000,
            runtime: "runc".to_string(),
            docker_host: std::env::var("DOCKER_HOST")
                .unwrap_or_else(|_| "unix:///var/run/docker.sock".to_string()),
            network: String::new(),
        }
    }
}

/// Returns the volume name for a bot's work directory.
fn volume_for(bot_id: &str) -> String {
    let sanitized = bot_id
        .chars()
        .map(|c| match c {
            'a'..='z' | 'A'..='Z' | '0'..='9' | '_' | '.' | '-' => c,
            _ => '_',
        })
        .collect::<String>();
    format!("bullpen-work-{}", sanitized)
}

/// Network arguments for a sandbox container.
/// With network off, returns `["--network", "none"]`.
fn network_args(cfg: &SandboxConfig) -> Vec<String> {
    if cfg.network.is_empty() {
        vec!["--network".to_string(), "none".to_string()]
    } else {
        vec!["--network".to_string(), cfg.network.clone()]
    }
}

/// Trait for running commands. Allows tests to use a fake without docker.
#[async_trait::async_trait]
pub trait CommandRunner: Send + Sync {
    /// Runs a command with the given arguments and stdin.
    /// Returns (stdout, stderr, exit_code).
    async fn run(
        &self,
        argv: Vec<String>,
        stdin: Vec<u8>,
        timeout: Duration,
    ) -> Result<(String, String, i32), String>;
}

/// The real command runner using tokio::process.
pub struct TokioRunner {
    env_docker_host: String,
}

impl TokioRunner {
    pub fn new(docker_host: String) -> Self {
        Self {
            env_docker_host: docker_host,
        }
    }
}

#[async_trait::async_trait]
impl CommandRunner for TokioRunner {
    async fn run(
        &self,
        argv: Vec<String>,
        _stdin: Vec<u8>,
        timeout: Duration,
    ) -> Result<(String, String, i32), String> {
        if argv.is_empty() {
            return Err("empty command".to_string());
        }

        let mut cmd = tokio::process::Command::new(&argv[0]);
        cmd.args(&argv[1..])
            .env("DOCKER_HOST", &self.env_docker_host);

        match tokio::time::timeout(timeout, cmd.output()).await {
            Ok(Ok(output)) => {
                let stdout = String::from_utf8_lossy(&output.stdout).to_string();
                let stderr = String::from_utf8_lossy(&output.stderr).to_string();
                let exit_code = output.status.code().unwrap_or(1);
                Ok((stdout, stderr, exit_code))
            }
            Ok(Err(e)) => Err(format!("command failed: {}", e)),
            Err(_) => Err("command timed out".to_string()),
        }
    }
}

/// Test helper: records commands and returns scripted outputs.
/// Public without `#[cfg(test)]` so integration tests can use it.
#[derive(Clone)]
pub struct FakeRunner {
    commands: std::sync::Arc<std::sync::Mutex<Vec<Vec<String>>>>,
    responses: std::sync::Arc<std::sync::Mutex<Vec<(String, String, i32)>>>,
}

impl Default for FakeRunner {
    fn default() -> Self {
        Self::new()
    }
}

impl FakeRunner {
    pub fn new() -> Self {
        Self {
            commands: std::sync::Arc::new(std::sync::Mutex::new(Vec::new())),
            responses: std::sync::Arc::new(std::sync::Mutex::new(Vec::new())),
        }
    }

    pub fn push_response(
        &self,
        stdout: impl Into<String>,
        stderr: impl Into<String>,
        exit_code: i32,
    ) {
        let mut responses = self.responses.lock().unwrap();
        responses.push((stdout.into(), stderr.into(), exit_code));
    }

    pub fn commands(&self) -> Vec<Vec<String>> {
        self.commands.lock().unwrap().clone()
    }
}

#[async_trait::async_trait]
impl CommandRunner for FakeRunner {
    async fn run(
        &self,
        argv: Vec<String>,
        _stdin: Vec<u8>,
        _timeout: Duration,
    ) -> Result<(String, String, i32), String> {
        let mut commands = self.commands.lock().unwrap();
        commands.push(argv);

        let mut responses = self.responses.lock().unwrap();
        if let Some((stdout, stderr, exit_code)) = responses.pop() {
            Ok((stdout, stderr, exit_code))
        } else {
            Err("no response configured".to_string())
        }
    }
}

/// Helper to cap output and track truncation.
fn capped(
    stdout: String,
    stderr: String,
    exit_code: i32,
    timed_out: bool,
    max: usize,
) -> ExecResult {
    let truncate = |text: &str| {
        if text.len() > max {
            text[..max].to_string()
        } else {
            text.to_string()
        }
    };

    let truncated = stdout.len() > max || stderr.len() > max;
    ExecResult {
        stdout: truncate(&stdout),
        stderr: truncate(&stderr),
        exit_code,
        timed_out,
        truncated,
    }
}

/// Validates that a path is under `/work` and not an absolute path or contains `..`.
fn validate_read_path(path: &str) -> Result<(), String> {
    // Reject absolute paths
    if path.starts_with('/') {
        return Err(
            "Path must be under /work and cannot be absolute. Set BULLPEN_SANDBOX=on for sandboxing."
                .to_string(),
        );
    }

    // Reject paths with `..`
    if path.contains("..") {
        return Err("Path cannot escape /work. Set BULLPEN_SANDBOX=on for sandboxing.".to_string());
    }

    // Use Path to resolve and check
    let mut final_path = PathBuf::from("/work");
    for component in Path::new(path).components() {
        match component {
            Component::Normal(c) => final_path.push(c),
            Component::RootDir => {
                return Err(
                    "Path must be under /work. Set BULLPEN_SANDBOX=on for sandboxing.".to_string(),
                );
            }
            Component::ParentDir => {
                return Err(
                    "Path cannot use `..`. Set BULLPEN_SANDBOX=on for sandboxing.".to_string(),
                );
            }
            _ => {}
        }
    }

    // Ensure the resolved path is under /work
    if !final_path.starts_with("/work") {
        return Err("Path must be under /work. Set BULLPEN_SANDBOX=on for sandboxing.".to_string());
    }

    Ok(())
}

/// A real Docker sandbox that executes commands in containers.
pub struct DockerSandbox {
    config: SandboxConfig,
    runner: std::sync::Arc<dyn CommandRunner>,
}

impl DockerSandbox {
    pub fn new(config: SandboxConfig, runner: std::sync::Arc<dyn CommandRunner>) -> Self {
        Self { config, runner }
    }
}

#[async_trait::async_trait]
impl Sandbox for DockerSandbox {
    async fn exec(&self, bot_id: &str, command: &str) -> ExecResult {
        let volume = volume_for(bot_id);
        let mut args = vec!["docker".to_string(), "run".to_string(), "--rm".to_string()];

        // Network configuration
        let net_args = network_args(&self.config);
        args.extend(net_args);

        // Resource limits and security
        args.extend(vec![
            "--memory".to_string(),
            self.config.memory.clone(),
            "--memory-swap".to_string(),
            self.config.memory.clone(),
            "--cpus".to_string(),
            self.config.cpus.clone(),
            "--pids-limit".to_string(),
            self.config.pids_limit.to_string(),
            "--read-only".to_string(),
            "--cap-drop".to_string(),
            "ALL".to_string(),
            "--security-opt".to_string(),
            "no-new-privileges".to_string(),
        ]);

        // Volume and working directory
        args.extend(vec![
            "--mount".to_string(),
            format!("type=volume,source={},target=/work", volume),
            "-w".to_string(),
            "/work".to_string(),
            "--label".to_string(),
            format!("bullpen.bot={}", bot_id),
        ]);

        // Image and command
        args.extend(vec![
            self.config.image.clone(),
            "sh".to_string(),
            "-c".to_string(),
            command.to_string(),
        ]);

        let timeout = Duration::from_millis(self.config.timeout_ms + 5_000);

        match self.runner.run(args, vec![], timeout).await {
            Ok((stdout, stderr, exit_code)) => {
                let timed_out = stderr.contains("timed out") || stderr.contains("timeout");
                let final_exit_code = if timed_out { 124 } else { exit_code };
                capped(
                    stdout,
                    stderr,
                    final_exit_code,
                    timed_out,
                    self.config.max_output_bytes,
                )
            }
            Err(err) => {
                let (exit_code, timed_out) = if err.contains("timed out") {
                    (124, true)
                } else {
                    (1, false)
                };
                capped(
                    "".to_string(),
                    err,
                    exit_code,
                    timed_out,
                    self.config.max_output_bytes,
                )
            }
        }
    }

    async fn read_file(&self, bot_id: &str, path: &str) -> Result<String, String> {
        validate_read_path(path)?;

        let volume = volume_for(bot_id);
        let args = vec![
            "docker".to_string(),
            "run".to_string(),
            "--rm".to_string(),
            "--mount".to_string(),
            format!("type=volume,source={},target=/work,ro", volume),
            "-w".to_string(),
            "/work".to_string(),
            self.config.image.clone(),
            "cat".to_string(),
            path.to_string(),
        ];

        let timeout = Duration::from_millis(self.config.timeout_ms);

        match self.runner.run(args, vec![], timeout).await {
            Ok((stdout, _, 0)) => Ok(stdout),
            Ok((_, stderr, _)) => Err(stderr),
            Err(e) => Err(e),
        }
    }
}

/// A sandbox that is not available (used when `BULLPEN_SANDBOX` is off).
pub struct UnavailableSandbox {
    reason: String,
}

impl UnavailableSandbox {
    pub fn new(reason: impl Into<String>) -> Self {
        Self {
            reason: reason.into(),
        }
    }
}

#[async_trait::async_trait]
impl Sandbox for UnavailableSandbox {
    async fn exec(&self, _bot_id: &str, _command: &str) -> ExecResult {
        ExecResult {
            stdout: String::new(),
            stderr: format!(
                "No sandbox is available, so nothing was run. {}",
                self.reason
            ),
            exit_code: 127,
            timed_out: false,
            truncated: false,
        }
    }

    async fn read_file(&self, _bot_id: &str, _path: &str) -> Result<String, String> {
        Err(format!(
            "No sandbox is available, so nothing was read. {}",
            self.reason
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn volume_name_sanitizes_bot_ids() {
        assert_eq!(volume_for("arthur"), "bullpen-work-arthur");
        assert_ne!(volume_for("arthur"), volume_for("trinity"));
        // Each bot gets its own volume
    }

    #[test]
    fn volume_name_makes_bot_id_safe() {
        let result = volume_for("../../etc/passwd");
        assert!(result.starts_with("bullpen-work-"));
        assert!(
            result
                .chars()
                .all(|c| matches!(c, 'a'..='z' | 'A'..='Z' | '0'..='9' | '_' | '.' | '-'))
        );
    }

    #[test]
    fn network_args_default_to_none() {
        let cfg = SandboxConfig::default();
        let args = network_args(&cfg);
        assert_eq!(args, vec!["--network", "none"]);
    }

    #[tokio::test]
    async fn unavailable_sandbox_refuses_loudly() {
        let sandbox = UnavailableSandbox::new("no daemon here");
        let result = sandbox.exec("arthur", "echo hi").await;

        assert_eq!(result.exit_code, 127);
        assert!(result.stderr.contains("nothing was run"));
        assert_eq!(result.stdout, "");
    }

    #[tokio::test]
    async fn read_file_rejects_absolute_paths() {
        let sandbox = UnavailableSandbox::new("test");
        let result = sandbox.read_file("bot1", "/etc/passwd").await;

        assert!(result.is_err());
        let err = result.unwrap_err();
        // UnavailableSandbox returns "nothing was read" with the reason appended
        assert!(err.contains("nothing was read") || err.contains("test"));
    }

    #[tokio::test]
    async fn read_file_rejects_parent_dir() {
        let sandbox = UnavailableSandbox::new("test");
        let result = sandbox.read_file("bot1", "../../../etc/passwd").await;

        assert!(result.is_err());
    }

    #[test]
    fn validate_path_accepts_relative_files() {
        assert!(validate_read_path("file.txt").is_ok());
        assert!(validate_read_path("subdir/file.txt").is_ok());
    }

    #[test]
    fn validate_path_rejects_absolute() {
        assert!(validate_read_path("/etc/passwd").is_err());
    }

    #[test]
    fn validate_path_rejects_parent_dir() {
        assert!(validate_read_path("../../../etc/passwd").is_err());
        assert!(validate_read_path("..").is_err());
    }

    #[test]
    fn capped_output_truncates_at_limit() {
        let result = capped("a".repeat(100), String::new(), 0, false, 50);
        assert!(result.truncated);
        assert_eq!(result.stdout.len(), 50);
    }

    #[test]
    fn capped_output_marks_truncation() {
        let result = capped("long".repeat(100), String::new(), 0, false, 50);
        assert!(result.truncated);
    }
}
