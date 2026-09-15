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
    /// S6L-F-01/F5: true ONLY for `UnavailableSandbox`'s own sentinel result.
    /// `shell::format_result` reads this directly instead of shape-sniffing
    /// exit code + stderr prefix, which a bot could forge with
    /// `sh -c 'printf "No sandbox is available, so nothing was run. X" >&2; exit 127'`.
    /// A real `DockerSandbox` run - however it exits - always sets this `false`.
    pub unavailable: bool,
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
    /// Passed as `--runtime` on every container when non-empty (F3). Empty
    /// means "let docker pick", same convention `network` already uses for
    /// `--network`.
    pub runtime: String,
    pub docker_host: String,
    /// Empty means `--network none`, which is the default.
    pub network: String,
    /// S6-W-02: egress proxy address (e.g., "127.0.0.1:12345"), or empty
    /// if the proxy is not running. When non-empty and a bot has egress mode
    /// "allowlist", the container gets HTTP_PROXY/HTTPS_PROXY env vars pointing
    /// to this address.
    pub egress_proxy_addr: String,
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
            egress_proxy_addr: String::new(),
        }
    }
}

/// Returns the volume name for a bot's work directory.
fn volume_for(bot_id: &str) -> String {
    format!("bullpen-work-{}", sanitize_for_docker(bot_id))
}

/// Replaces anything that is not alphanumeric/`_`/`.`/`-` with `_`. Shared by
/// `volume_for` and `container_name` so a bot id can never inject a docker
/// flag through either.
fn sanitize_for_docker(id: &str) -> String {
    id.chars()
        .map(|c| match c {
            'a'..='z' | 'A'..='Z' | '0'..='9' | '_' | '.' | '-' => c,
            _ => '_',
        })
        .collect::<String>()
}

/// A per-call container name, so a timed-out run leaves something
/// `docker rm -f` can find (F6) - the docker CLI's own `kill_on_drop` only
/// reclaims the CLI process, not the container it started, since `--rm`
/// only fires when the CLI exits cleanly.
fn container_name(bot_id: &str) -> String {
    format!(
        "bullpen-{}-{}",
        sanitize_for_docker(bot_id),
        uuid::Uuid::new_v4().simple()
    )
}

/// Network arguments for a sandbox container.
/// With network off, returns `["--network", "none"]`.
/// 🔴 This ALWAYS returns "none" for S6-W-02 - network ONLY through the proxy.
fn network_args(cfg: &SandboxConfig) -> Vec<String> {
    if cfg.network.is_empty() {
        vec!["--network".to_string(), "none".to_string()]
    } else {
        vec!["--network".to_string(), cfg.network.clone()]
    }
}

/// Proxy environment variables for a bot with egress enabled.
/// Returns docker `-e` flag pairs for HTTP_PROXY, HTTPS_PROXY, NO_PROXY.
/// 🔴 These env vars ONLY configure the proxy path; the container still has
/// `--network none`. Traffic ONLY flows through the proxy.
fn proxy_args(proxy_addr: &str) -> Vec<String> {
    let proxy_url = format!("http://{}", proxy_addr);
    vec![
        "-e".to_string(),
        format!("HTTP_PROXY={}", proxy_url),
        "-e".to_string(),
        format!("HTTPS_PROXY={}", proxy_url),
        "-e".to_string(),
        "NO_PROXY=127.0.0.1,localhost".to_string(),
    ]
}

/// F2: the full hardened flag block shared by BOTH `exec` and `read_file` -
/// `--name` through `--user`, everything after `docker run --rm` and before
/// the `--mount`/`-w`/`--label`/image tail that differs per call. Before
/// this, `read_file` built its own `docker run` from scratch and had none
/// of this - no network isolation, no resource caps, no capability drop, no
/// `--read-only`. ONE function, used by both, the same reasoning
/// `sandbox.ts:126-135` gives for `networkArgs`: a flag added here can never
/// again be forgotten on one of the two call sites.
fn isolation_args(cfg: &SandboxConfig, name: &str) -> Vec<String> {
    let mut args = vec!["--name".to_string(), name.to_string()];

    // F3: `--runtime` (gVisor `runsc` in production) was declared on
    // `SandboxConfig` and never passed - every container silently ran on
    // the host kernel instead.
    if !cfg.runtime.is_empty() {
        args.push("--runtime".to_string());
        args.push(cfg.runtime.clone());
    }

    args.extend(network_args(cfg));

    args.extend(vec![
        "--memory".to_string(),
        cfg.memory.clone(),
        "--memory-swap".to_string(),
        cfg.memory.clone(),
        "--cpus".to_string(),
        cfg.cpus.clone(),
        "--pids-limit".to_string(),
        cfg.pids_limit.to_string(),
        "--read-only".to_string(),
        // F10: matches `--read-only` with a writable `/tmp`, same as
        // `sandbox.ts:288` - without this, anything that needs a temp file
        // (`sort`, `mktemp`, `tar -x`, most interpreters) fails outright.
        "--tmpfs".to_string(),
        "/tmp:rw,noexec,nosuid,size=64m".to_string(),
        "--cap-drop".to_string(),
        "ALL".to_string(),
        "--security-opt".to_string(),
        "no-new-privileges".to_string(),
        // F3: root-in-container on the host kernel is a different threat
        // model than the gVisor + non-root pairing this sandbox promises.
        "--user".to_string(),
        "1000:1000".to_string(),
    ]);

    args
}

/// Signals a timeout that was detected by OUR OWN timer (F7), never by
/// inspecting the command's stderr - a bot cannot forge this by printing
/// the word "timeout".
#[derive(Debug)]
pub enum RunError {
    Timeout,
    Other(String),
}

/// Trait for running commands. Allows tests to use a fake without docker.
#[async_trait::async_trait]
pub trait CommandRunner: Send + Sync {
    /// Runs a command with the given arguments and stdin.
    /// Returns (stdout, stderr, exit_code). `max_output_bytes` bounds how
    /// much of the child's output this call will buffer host-side (F4) -
    /// independent of the caller's own truncation, which only trims the
    /// string AFTER the whole thing was already read into memory.
    async fn run(
        &self,
        argv: Vec<String>,
        stdin: Vec<u8>,
        timeout: Duration,
        max_output_bytes: usize,
    ) -> Result<(String, String, i32), RunError>;
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

/// Reads `reader` into a `Vec<u8>`, stopping (and reporting `true`) once
/// `cap` bytes have been read. F4: without a cap here, `cmd.output()`
/// buffers the ENTIRE child stdout in this process's heap before the
/// caller's own `max_output_bytes` truncation ever runs - `sh -c 'yes'`
/// fills the host process, not just the container, until the systemd unit's
/// `MemoryMax` OOM-kills bullpen-rs itself.
async fn read_capped(mut reader: impl tokio::io::AsyncRead + Unpin, cap: usize) -> (Vec<u8>, bool) {
    use tokio::io::AsyncReadExt;
    let mut buf = Vec::new();
    let mut chunk = [0u8; 8192];
    loop {
        match reader.read(&mut chunk).await {
            Ok(0) => return (buf, false),
            Ok(n) => {
                buf.extend_from_slice(&chunk[..n]);
                if buf.len() >= cap {
                    return (buf, true);
                }
            }
            Err(_) => return (buf, false),
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
        max_output_bytes: usize,
    ) -> Result<(String, String, i32), RunError> {
        if argv.is_empty() {
            return Err(RunError::Other("empty command".to_string()));
        }

        let mut cmd = tokio::process::Command::new(&argv[0]);
        cmd.args(&argv[1..])
            .env("DOCKER_HOST", &self.env_docker_host)
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            // F6: a dropped `Command` future (e.g. our own timeout branch
            // below) otherwise leaves the docker CLI process running
            // forever - tokio's default is `kill_on_drop(false)`.
            .kill_on_drop(true);

        let mut child = match cmd.spawn() {
            Ok(child) => child,
            Err(e) => return Err(RunError::Other(format!("command failed to start: {e}"))),
        };

        let stdout = child.stdout.take().expect("stdout was piped");
        let stderr = child.stderr.take().expect("stderr was piped");
        // 4x the caller's cap, same margin `sandbox.ts:245-250` uses for
        // node's `maxBuffer` - room for the un-truncated result plus
        // whatever the caller's own truncation trims off afterwards.
        let cap = max_output_bytes.saturating_mul(4).max(1);

        let work = async {
            let (out, err) = tokio::join!(read_capped(stdout, cap), read_capped(stderr, cap));
            if out.1 || err.1 {
                // The child was still writing past our host-side bound;
                // reading stopped, but the process itself has not - kill it
                // before waiting, or a full pipe buffer wedges `wait()`.
                let _ = child.kill().await;
            }
            let status = child.wait().await;
            (out.0, err.0, status)
        };

        match tokio::time::timeout(timeout, work).await {
            Ok((stdout, stderr, status)) => {
                let exit_code = status.ok().and_then(|s| s.code()).unwrap_or(1);
                Ok((
                    String::from_utf8_lossy(&stdout).to_string(),
                    String::from_utf8_lossy(&stderr).to_string(),
                    exit_code,
                ))
            }
            Err(_) => {
                let _ = child.kill().await;
                Err(RunError::Timeout)
            }
        }
    }
}

/// Test helper: records commands and returns scripted outputs.
/// Public without `#[cfg(test)]` so integration tests can use it.
#[derive(Clone)]
pub struct FakeRunner {
    commands: std::sync::Arc<std::sync::Mutex<Vec<Vec<String>>>>,
    responses: std::sync::Arc<std::sync::Mutex<std::collections::VecDeque<FakeOutcome>>>,
}

#[derive(Clone)]
enum FakeOutcome {
    Ok(String, String, i32),
    Timeout,
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
            responses: std::sync::Arc::new(
                std::sync::Mutex::new(std::collections::VecDeque::new()),
            ),
        }
    }

    pub fn push_response(
        &self,
        stdout: impl Into<String>,
        stderr: impl Into<String>,
        exit_code: i32,
    ) {
        let mut responses = self.responses.lock().unwrap();
        responses.push_back(FakeOutcome::Ok(stdout.into(), stderr.into(), exit_code));
    }

    /// Scripts the NEXT call to fail with `RunError::Timeout` - the real
    /// signal `TokioRunner` produces from its own timer (F7), used to prove
    /// `DockerSandbox::exec` reports a timeout from this instead of from
    /// stderr content.
    pub fn push_timeout(&self) {
        let mut responses = self.responses.lock().unwrap();
        responses.push_back(FakeOutcome::Timeout);
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
        _max_output_bytes: usize,
    ) -> Result<(String, String, i32), RunError> {
        let mut commands = self.commands.lock().unwrap();
        commands.push(argv.clone());

        // F18: FIFO (scripted in call order), not LIFO - and an exhausted
        // queue panics with the argv that had nothing to answer it, rather
        // than returning a plausible-looking `Err` that `DockerSandbox`
        // turns into an ordinary `exit code 1` a test could mistake for a
        // real assertion.
        let mut responses = self.responses.lock().unwrap();
        match responses.pop_front() {
            Some(FakeOutcome::Ok(stdout, stderr, exit_code)) => Ok((stdout, stderr, exit_code)),
            Some(FakeOutcome::Timeout) => Err(RunError::Timeout),
            None => panic!("FakeRunner: no response scripted for {argv:?}"),
        }
    }
}

/// Cuts `text` to at most `max` BYTES without ever splitting a multi-byte
/// UTF-8 character (F1) - `text[..max]` panics when `max` lands mid-char;
/// `sh -c "printf '€%.0s' $(seq 1 30000)"` hits this deterministically at
/// the default 64 000-byte cap (64000 % 3 == 1). Backs off to the nearest
/// EARLIER char boundary so the result never exceeds `max` bytes.
fn truncate_char_boundary(text: &str, max: usize) -> (String, bool) {
    if text.len() <= max {
        return (text.to_string(), false);
    }
    let end = (0..=max)
        .rev()
        .find(|i| text.is_char_boundary(*i))
        .unwrap_or(0);
    (text[..end].to_string(), true)
}

/// Helper to cap output and track truncation.
fn capped(
    stdout: String,
    stderr: String,
    exit_code: i32,
    timed_out: bool,
    max: usize,
) -> ExecResult {
    let (stdout, stdout_cut) = truncate_char_boundary(&stdout, max);
    let (stderr, stderr_cut) = truncate_char_boundary(&stderr, max);
    ExecResult {
        stdout,
        stderr,
        exit_code,
        timed_out,
        truncated: stdout_cut || stderr_cut,
        unavailable: false,
    }
}

/// F4: `read_file`'s result went into the run's `messages` JSON and then
/// `bullpen.db` with NO cap at all. Runs the same char-boundary-safe cut
/// `capped` uses for `shell`, with a trailing note since `read_file`'s
/// `Result<String, String>` has no separate `truncated` flag to carry one.
fn cap_text(text: &str, max: usize) -> String {
    let (text, was_truncated) = truncate_char_boundary(text, max);
    if was_truncated {
        format!("{text}\n[truncated at {max} bytes]")
    } else {
        text
    }
}

/// Validates that a path is under `/work` and not an absolute path or contains `..`.
/// F17: the `..` check is scoped to path COMPONENTS via the loop below (the
/// `Component::ParentDir` arm) - a blanket `path.contains("..")` used to
/// also reject legitimate names like `build..old.log`.
fn validate_read_path(path: &str) -> Result<(), String> {
    // Reject absolute paths
    if path.starts_with('/') {
        return Err("Path must be under /work and cannot be absolute.".to_string());
    }

    // Use Path to resolve and check
    let mut final_path = PathBuf::from("/work");
    for component in Path::new(path).components() {
        match component {
            Component::Normal(c) => final_path.push(c),
            Component::RootDir => {
                return Err("Path must be under /work.".to_string());
            }
            Component::ParentDir => {
                return Err("Path cannot escape /work (`..` is not allowed).".to_string());
            }
            _ => {}
        }
    }

    // Ensure the resolved path is under /work
    if !final_path.starts_with("/work") {
        return Err("Path must be under /work.".to_string());
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

    /// F6: best-effort cleanup after our own timeout fires. The docker CLI
    /// process is already reclaimed (`kill_on_drop`), but `--rm` only runs
    /// when the CLI exits cleanly, so the CONTAINER it started is still up
    /// on the daemon until something names it and removes it.
    async fn force_remove(&self, name: &str) {
        let _ = self
            .runner
            .run(
                vec![
                    "docker".to_string(),
                    "rm".to_string(),
                    "-f".to_string(),
                    name.to_string(),
                ],
                vec![],
                Duration::from_secs(20),
                0,
            )
            .await;
    }
}

#[async_trait::async_trait]
impl Sandbox for DockerSandbox {
    async fn exec(&self, bot_id: &str, command: &str) -> ExecResult {
        let volume = volume_for(bot_id);
        let name = container_name(bot_id);
        let mut args = vec!["docker".to_string(), "run".to_string(), "--rm".to_string()];

        args.extend(isolation_args(&self.config, &name));

        // S6-W-02: add proxy environment variables if proxy is configured.
        // A per-bot allowlist check would happen here once per-bot proxies
        // are added. For now, the proxy address being non-empty means the
        // master switch is on and this bot should go through it.
        if !self.config.egress_proxy_addr.is_empty() {
            args.extend(proxy_args(&self.config.egress_proxy_addr));
        }

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

        match self
            .runner
            .run(args, vec![], timeout, self.config.max_output_bytes)
            .await
        {
            Ok((stdout, stderr, exit_code)) => capped(
                stdout,
                stderr,
                exit_code,
                false,
                self.config.max_output_bytes,
            ),
            // F7: timed_out comes ONLY from our own timer via `RunError`
            // now - never from grepping stderr for the word "timeout",
            // which both misreported ordinary failures (curl, pytest, npm)
            // and let a bot forge its own verdict with `echo timeout >&2`.
            Err(RunError::Timeout) => {
                self.force_remove(&name).await;
                capped(
                    String::new(),
                    String::new(),
                    124,
                    true,
                    self.config.max_output_bytes,
                )
            }
            Err(RunError::Other(err)) => {
                capped(String::new(), err, 1, false, self.config.max_output_bytes)
            }
        }
    }

    async fn read_file(&self, bot_id: &str, path: &str) -> Result<String, String> {
        validate_read_path(path)?;

        let volume = volume_for(bot_id);
        let name = container_name(bot_id);
        let mut args = vec!["docker".to_string(), "run".to_string(), "--rm".to_string()];

        args.extend(isolation_args(&self.config, &name));

        // S6-W-02: add proxy environment variables if proxy is configured.
        // A per-bot allowlist check would happen here once per-bot proxies
        // are added. For now, the proxy address being non-empty means the
        // master switch is on and this bot should go through it.
        if !self.config.egress_proxy_addr.is_empty() {
            args.extend(proxy_args(&self.config.egress_proxy_addr));
        }

        args.extend(vec![
            "--mount".to_string(),
            format!("type=volume,source={},target=/work,ro", volume),
            "-w".to_string(),
            "/work".to_string(),
            "--label".to_string(),
            format!("bullpen.bot={}", bot_id),
        ]);

        args.extend(vec![
            self.config.image.clone(),
            "cat".to_string(),
            path.to_string(),
        ]);

        let timeout = Duration::from_millis(self.config.timeout_ms);

        match self
            .runner
            .run(args, vec![], timeout, self.config.max_output_bytes)
            .await
        {
            Ok((stdout, _, 0)) => Ok(cap_text(&stdout, self.config.max_output_bytes)),
            Ok((_, stderr, _)) => Err(stderr),
            Err(RunError::Timeout) => {
                self.force_remove(&name).await;
                Err("The read was stopped for taking too long.".to_string())
            }
            Err(RunError::Other(e)) => Err(e),
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
            unavailable: true,
        }
    }

    async fn read_file(&self, _bot_id: &str, _path: &str) -> Result<String, String> {
        Err(format!(
            "No sandbox is available, so nothing was read. {}",
            self.reason
        ))
    }
}

/// Probes the sandbox daemon once at startup (F9): `docker version` against
/// `docker_host`. TS's `check()` (`sandbox.ts:253-273`) also verifies the
/// configured runtime is registered via `docker info`; this is the
/// narrower "is anything even listening" half of that, which is the half
/// that actually explains meridian's likeliest failure (daemon not up yet,
/// or rootless lingering not enabled) rather than a raw docker CLI error
/// reaching the model unfenced as if it were the tool's own text.
fn probe_docker(docker_host: &str) -> Result<(), String> {
    std::process::Command::new("docker")
        .arg("version")
        .env("DOCKER_HOST", docker_host)
        .output()
        .map_err(|e| format!("could not run the docker CLI: {e}"))
        .and_then(|output| {
            if output.status.success() {
                Ok(())
            } else {
                Err(format!(
                    "docker version exited {}: {}",
                    output.status.code().unwrap_or(-1),
                    String::from_utf8_lossy(&output.stderr).trim()
                ))
            }
        })
}

/// The sandbox for executing bot commands, chosen by `BULLPEN_SANDBOX` at
/// call time. S6L-01 put this logic in `lib.rs` as a private fn used only
/// by `AppState::build`; S6L-02 moved it here, public, so `RunManager`'s
/// no-sandbox-arg constructors (`new`, `with_backlog_ttl`) can resolve the
/// same default a real server would - every test in this crate that builds
/// a `RunManager` directly (most of the suite predates this ticket) keeps
/// compiling and keeps seeing the S2 Unavailable text unchanged, since
/// `BULLPEN_SANDBOX` is never `on` in a test process (the probe below is
/// therefore never reached by the test suite either).
pub fn default_sandbox() -> std::sync::Arc<dyn Sandbox> {
    let sandbox_mode = std::env::var("BULLPEN_SANDBOX").unwrap_or_default();
    if sandbox_mode != "on" {
        return std::sync::Arc::new(UnavailableSandbox::new(
            "Sandboxing is off here. Set BULLPEN_SANDBOX=on where it is wanted.",
        ));
    }

    let config = SandboxConfig::default();
    // F9: the startup probe the ticket specified, and three doc comments
    // already claimed existed. Without it, a daemon that is not up yet (or
    // an image never pulled) meant every `shell` call returned raw `docker`
    // CLI error text fenced as tool output, instead of the Unavailable
    // sentence written for exactly this case.
    match probe_docker(&config.docker_host) {
        Ok(()) => {
            let runner = std::sync::Arc::new(TokioRunner::new(config.docker_host.clone()));
            std::sync::Arc::new(DockerSandbox::new(config, runner))
        }
        Err(reason) => {
            tracing::error!("sandbox startup probe failed: {reason}");
            std::sync::Arc::new(UnavailableSandbox::new(format!(
                "The sandbox daemon did not answer at startup. {reason}"
            )))
        }
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

    #[test]
    fn isolation_args_include_runtime_when_set() {
        let cfg = SandboxConfig::default();
        let args = isolation_args(&cfg, "bullpen-arthur-abc");
        assert!(
            args.windows(2)
                .any(|w| w[0] == "--runtime" && w[1] == "runc")
        );
    }

    #[test]
    fn isolation_args_omit_runtime_when_empty() {
        let cfg = SandboxConfig {
            runtime: String::new(),
            ..Default::default()
        };
        let args = isolation_args(&cfg, "bullpen-arthur-abc");
        assert!(!args.iter().any(|a| a == "--runtime"));
    }

    #[tokio::test]
    async fn unavailable_sandbox_refuses_loudly() {
        let sandbox = UnavailableSandbox::new("no daemon here");
        let result = sandbox.exec("arthur", "echo hi").await;

        assert_eq!(result.exit_code, 127);
        assert!(result.stderr.contains("nothing was run"));
        assert_eq!(result.stdout, "");
        assert!(result.unavailable);
    }

    #[tokio::test]
    async fn docker_sandbox_result_is_never_marked_unavailable() {
        let config = SandboxConfig::default();
        let runner = std::sync::Arc::new(FakeRunner::new());
        runner.push_response("hi\n", "", 0);
        let runner_dyn: std::sync::Arc<dyn CommandRunner> = runner.clone();
        let sandbox = DockerSandbox::new(config, runner_dyn);

        let result = sandbox.exec("arthur", "echo hi").await;
        assert!(!result.unavailable);
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

    /// F17: a blanket `path.contains("..")` used to reject this legitimate
    /// filename too; only a real `..` path COMPONENT should be rejected.
    #[test]
    fn validate_path_accepts_dotdot_inside_a_filename() {
        assert!(validate_read_path("build..old.log").is_ok());
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

    /// F1 bite: a byte-index slice (`text[..max]`) panics here because byte
    /// 50 lands mid-character in a 3-byte UTF-8 sequence (`€` = `\xE2\x82\xAC`).
    /// `truncate_char_boundary` must back off to a real char boundary
    /// instead of panicking.
    #[test]
    fn capped_output_does_not_panic_on_multibyte_boundary() {
        let euros = "\u{20AC}".repeat(30_000); // 90 000 bytes, 3 per char
        let result = capped(euros, String::new(), 0, false, 64_000);
        assert!(result.truncated);
        assert!(result.stdout.len() <= 64_000);
        assert!(String::from_utf8(result.stdout.into_bytes()).is_ok());
    }

    #[test]
    fn cap_text_notes_truncation() {
        let capped = cap_text(&"a".repeat(100), 10);
        assert!(capped.starts_with(&"a".repeat(10)));
        assert!(capped.contains("truncated"));
    }

    #[test]
    fn cap_text_leaves_short_text_alone() {
        assert_eq!(cap_text("hello", 100), "hello");
    }
}
