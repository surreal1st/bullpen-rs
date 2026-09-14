//! Integration tests for the sandbox module.
//! Ported from TypeScript `test/sandbox.test.ts`.

use server::sandbox::{
    CommandRunner, DockerSandbox, FakeRunner, Sandbox, SandboxConfig, UnavailableSandbox,
};
use std::sync::Arc;

#[tokio::test]
async fn gives_each_bot_its_own_volume() {
    let config = SandboxConfig::default();
    let runner = Arc::new(FakeRunner::new());
    let runner_clone: Arc<dyn CommandRunner> = runner.clone();
    let sandbox = DockerSandbox::new(config, runner_clone);

    // FakeRunner will track the commands
    runner.push_response("", "", 0);
    let _result = sandbox.exec("arthur", "echo hi").await;

    // Verify the volume name is correct
    let commands = runner.commands();
    assert!(!commands.is_empty());
    let cmd_line = commands[0].join(" ");
    assert!(cmd_line.contains("bullpen-work-arthur"));
}

#[tokio::test]
async fn makes_bot_id_safe_as_volume_name() {
    let config = SandboxConfig::default();
    let runner = Arc::new(FakeRunner::new());
    let runner_clone: Arc<dyn CommandRunner> = runner.clone();
    let sandbox = DockerSandbox::new(config, runner_clone);

    runner.push_response("", "", 0);
    let _result = sandbox.exec("../../etc/passwd", "echo hi").await;

    let commands = runner.commands();
    assert!(!commands.is_empty());
    let cmd_line = commands[0].join(" ");
    // Should sanitize special characters - the volume name should contain only safe chars
    // The bot ID ../../etc/passwd should become bullpen-work-.._.._etc_passwd
    // (dots and underscores are allowed, special chars become underscores)
    assert!(cmd_line.contains("bullpen-work-"));
    // Extract the volume name to verify it only contains allowed characters
    let volume_safe = "bullpen-work-";
    if let Some(start) = cmd_line.find(volume_safe) {
        let after_prefix = &cmd_line[start + volume_safe.len()..];
        let volume_end = after_prefix.find(',').unwrap_or(after_prefix.len());
        let volume_name = &after_prefix[..volume_end];
        // Should only contain alphanumeric, underscore, dot, or dash
        assert!(
            volume_name
                .chars()
                .all(|c| matches!(c, 'a'..='z' | 'A'..='Z' | '0'..='9' | '_' | '.' | '-'))
        );
    }
}

#[tokio::test]
async fn refuses_loudly_when_no_sandbox() {
    let sandbox = UnavailableSandbox::new("no daemon here");
    let result = sandbox.exec("arthur", "echo hi").await;

    assert_eq!(result.exit_code, 127);
    assert!(result.stderr.contains("nothing was run"));
    assert_eq!(result.stdout, "");
}

#[tokio::test]
async fn network_args_are_network_none() {
    let config = SandboxConfig::default();
    let runner = Arc::new(FakeRunner::new());
    let runner_clone: Arc<dyn CommandRunner> = runner.clone();
    let sandbox = DockerSandbox::new(config, runner_clone);

    runner.push_response("", "", 0);
    let _result = sandbox.exec("bot1", "echo hi").await;

    let commands = runner.commands();
    assert!(!commands.is_empty());
    let cmd_line = commands[0].join(" ");
    // Should contain --network none
    assert!(cmd_line.contains("--network"));
    assert!(cmd_line.contains("none"));
}

#[tokio::test]
async fn working_directory_is_work() {
    let config = SandboxConfig::default();
    let runner = Arc::new(FakeRunner::new());
    let runner_clone: Arc<dyn CommandRunner> = runner.clone();
    let sandbox = DockerSandbox::new(config, runner_clone);

    runner.push_response("", "", 0);
    let _result = sandbox.exec("bot1", "echo hi").await;

    let commands = runner.commands();
    assert!(!commands.is_empty());
    let cmd_line = commands[0].join(" ");
    // Should contain -w /work
    assert!(cmd_line.contains("-w"));
    assert!(cmd_line.contains("/work"));
}

#[tokio::test]
async fn exec_passes_command_correctly() {
    let config = SandboxConfig::default();
    let runner = Arc::new(FakeRunner::new());
    runner.push_response("hello\n", "", 0);
    let runner_clone: Arc<dyn CommandRunner> = runner.clone();
    let sandbox = DockerSandbox::new(config, runner_clone);

    let result = sandbox.exec("bot1", "echo hello").await;

    assert_eq!(result.stdout.trim(), "hello");
    assert_eq!(result.exit_code, 0);

    let commands = runner.commands();
    assert!(!commands.is_empty());
    // The command should be present in the docker run args
    let cmd_line = commands[0].join(" ");
    assert!(
        cmd_line.contains("echo hello") || cmd_line.contains("echo") && cmd_line.contains("hello")
    );
}

// F7 bite: a timeout is decided ONLY by the runner's own `RunError::Timeout`
// signal now - `DockerSandbox::exec` must never derive it by grepping
// stderr, which both misreported ordinary failures and let a bot forge its
// own verdict.
#[tokio::test]
async fn real_timeout_signal_sets_exit_code_124() {
    let config = SandboxConfig::default();
    let runner = Arc::new(FakeRunner::new());
    let runner_clone: Arc<dyn CommandRunner> = runner.clone();
    let sandbox = DockerSandbox::new(config, runner_clone);

    runner.push_timeout();
    runner.push_response("", "", 0); // answers the F6 cleanup `docker rm -f` call
    let result = sandbox.exec("bot1", "sleep 1000").await;

    assert_eq!(result.exit_code, 124);
    assert!(result.timed_out);

    // F6: the timeout branch must clean up the container by name, so a
    // second `docker rm -f` call is expected alongside the original exec.
    let commands = runner.commands();
    assert_eq!(commands.len(), 2, "expected the exec plus a cleanup call");
    assert!(commands[1].iter().any(|a| a == "rm"));
    assert!(commands[1].iter().any(|a| a == "-f"));
}

// F7 regression: stderr merely CONTAINING the word "timeout" (an ordinary
// curl/pytest/npm/git failure) must not be reported as a timeout, and the
// real exit code must survive - this is the exact false positive the old
// `stderr.contains("timeout")` heuristic produced.
#[tokio::test]
async fn stderr_mentioning_timeout_is_not_treated_as_one() {
    let config = SandboxConfig::default();
    let runner = Arc::new(FakeRunner::new());
    let runner_clone: Arc<dyn CommandRunner> = runner.clone();
    let sandbox = DockerSandbox::new(config, runner_clone);

    runner.push_response("", "curl: (28) Operation timeout after 30001 ms", 28);
    let result = sandbox.exec("bot1", "curl https://example.invalid").await;

    assert_eq!(result.exit_code, 28, "the real exit code must survive");
    assert!(!result.timed_out, "stderr text must not fake a timeout");
}

#[tokio::test]
async fn output_truncation_at_cap() {
    let runner = Arc::new(FakeRunner::new());
    let runner_clone: Arc<dyn CommandRunner> = runner.clone();
    let sandbox = DockerSandbox::new(
        SandboxConfig {
            max_output_bytes: 50,
            ..Default::default()
        },
        runner_clone,
    );

    let long_output = "a".repeat(100);
    runner.push_response(&long_output, "", 0);
    let result = sandbox.exec("bot1", "yes").await;

    assert!(result.truncated);
    assert_eq!(result.stdout.len(), 50);
}

#[tokio::test]
async fn stderr_truncation_at_cap() {
    let runner = Arc::new(FakeRunner::new());
    let runner_clone: Arc<dyn CommandRunner> = runner.clone();
    let sandbox = DockerSandbox::new(
        SandboxConfig {
            max_output_bytes: 50,
            ..Default::default()
        },
        runner_clone,
    );

    let long_output = "error: ".to_string() + &"b".repeat(100);
    runner.push_response("", &long_output, 1);
    let result = sandbox.exec("bot1", "false").await;

    assert!(result.truncated);
    assert_eq!(result.stderr.len(), 50);
}

#[tokio::test]
async fn read_file_rejects_absolute_paths() {
    let sandbox = UnavailableSandbox::new("test");
    let result = sandbox.read_file("bot1", "/etc/passwd").await;

    assert!(result.is_err());
    let err = result.unwrap_err();
    assert!(err.contains("absolute") || err.contains("nothing was read"));
}

#[tokio::test]
async fn read_file_rejects_parent_dir() {
    let sandbox = UnavailableSandbox::new("test");
    let result = sandbox.read_file("bot1", "../../../etc/passwd").await;

    assert!(result.is_err());
}

#[tokio::test]
async fn read_file_accepts_relative_files() {
    let config = SandboxConfig::default();
    let runner = Arc::new(FakeRunner::new());
    let runner_clone: Arc<dyn CommandRunner> = runner.clone();
    let sandbox = DockerSandbox::new(config, runner_clone);

    runner.push_response("file content", "", 0);
    let result = sandbox.read_file("bot1", "file.txt").await;

    assert!(result.is_ok());
    assert_eq!(result.unwrap(), "file content");
}

// F4: `read_file`'s result must go through the same char-boundary-safe cap
// `shell` output does - before this fix it was returned with NO cap at all.
#[tokio::test]
async fn read_file_result_is_capped() {
    let runner = Arc::new(FakeRunner::new());
    let runner_clone: Arc<dyn CommandRunner> = runner.clone();
    let sandbox = DockerSandbox::new(
        SandboxConfig {
            max_output_bytes: 10,
            ..Default::default()
        },
        runner_clone,
    );

    runner.push_response("x".repeat(500), "", 0);
    let result = sandbox.read_file("bot1", "big.log").await.unwrap();

    assert!(result.len() < 500, "expected the content to be capped");
    assert!(result.contains("truncated"));
}

#[tokio::test]
async fn read_file_uses_docker_cat() {
    let config = SandboxConfig::default();
    let runner = Arc::new(FakeRunner::new());
    let runner_clone: Arc<dyn CommandRunner> = runner.clone();
    let sandbox = DockerSandbox::new(config, runner_clone);

    runner.push_response("content", "", 0);
    let _result = sandbox.read_file("bot1", "data.txt").await;

    let commands = runner.commands();
    assert!(!commands.is_empty());
    let cmd_line = commands[0].join(" ");
    // Should use 'cat' to read the file
    assert!(cmd_line.contains("cat"));
    assert!(cmd_line.contains("data.txt"));
}

#[tokio::test]
async fn unavailable_sandbox_says_nothing_was_run() {
    let sandbox = UnavailableSandbox::new("sandboxing is off");
    let result = sandbox.exec("bot1", "echo test").await;

    assert_eq!(result.exit_code, 127);
    assert!(result.stderr.contains("nothing was run"));
    assert!(result.stderr.contains("sandboxing is off"));
}

#[tokio::test]
async fn unavailable_sandbox_says_nothing_was_read() {
    let sandbox = UnavailableSandbox::new("sandboxing is off");
    let result = sandbox.read_file("bot1", "file.txt").await;

    assert!(result.is_err());
    let err = result.unwrap_err();
    assert!(err.contains("nothing was read") || err.contains("sandboxing is off"));
}

// Real Docker test - only runs if BULLPEN_TEST_DOCKER=1 and docker is
// available. F13: this used to self-skip even under `--ignored` (a silent
// PASS with nothing touched). It must now FAIL loudly instead of quietly
// reporting green when someone runs `--ignored` without opting in.
#[tokio::test]
#[ignore]
async fn real_docker_echo_roundtrip() {
    if std::env::var("BULLPEN_TEST_DOCKER").as_deref() != Ok("1") {
        panic!(
            "set BULLPEN_TEST_DOCKER=1 to run this test - it drives a real docker daemon and \
must not report a pass without touching one"
        );
    }

    let config = SandboxConfig::default();
    let runner = std::sync::Arc::new(server::sandbox::TokioRunner::new(
        config.docker_host.clone(),
    ));
    let sandbox = DockerSandbox::new(config, runner);

    let result = sandbox.exec("test-bot", "echo hello").await;

    assert_eq!(result.exit_code, 0, "stderr was: {}", result.stderr);
    assert!(result.stdout.contains("hello"));
}

// Bite test: verify that --network none is required
#[tokio::test]
async fn docker_args_include_network_none() {
    let config = SandboxConfig::default();
    let runner = Arc::new(FakeRunner::new());
    let runner_clone: Arc<dyn CommandRunner> = runner.clone();
    let sandbox = DockerSandbox::new(config, runner_clone);

    runner.push_response("", "", 0);
    let _result = sandbox.exec("bot1", "echo hi").await;

    let commands = runner.commands();
    assert!(!commands.is_empty());
    let cmd = &commands[0];

    // Find --network and verify the next element is "none"
    let mut found_network = false;
    for window in cmd.windows(2) {
        if window[0] == "--network" && window[1] == "none" {
            found_network = true;
            break;
        }
    }
    assert!(found_network, "Expected --network none in command");
}

/// Pulls the value docker would have received for `--name` out of a
/// recorded argv, so an exact-argv comparison can splice it back into the
/// expected vector - `container_name` mints a fresh one per call, so it can
/// never be hardcoded.
fn container_name_from(argv: &[String]) -> String {
    let idx = argv
        .iter()
        .position(|a| a == "--name")
        .expect("--name must be present");
    argv[idx + 1].clone()
}

fn strings(items: &[&str]) -> Vec<String> {
    items.iter().map(|s| s.to_string()).collect()
}

// F12: the container's ENTIRE security argv, compared as a whole `Vec<String>`
// rather than `contains`. Deleting any one flag from `isolation_args` - or
// dropping `--user`, `--runtime`, `--tmpfs` - fails this test.
#[tokio::test]
async fn exec_argv_matches_the_hardened_shape_exactly() {
    let config = SandboxConfig::default();
    let runner = Arc::new(FakeRunner::new());
    let runner_clone: Arc<dyn CommandRunner> = runner.clone();
    let sandbox = DockerSandbox::new(config, runner_clone);

    runner.push_response("hi\n", "", 0);
    let _ = sandbox.exec("bot1", "echo hi").await;

    let commands = runner.commands();
    assert_eq!(commands.len(), 1);
    let argv = &commands[0];
    let name = container_name_from(argv);

    let mut expected = strings(&["docker", "run", "--rm", "--name"]);
    expected.push(name);
    expected.extend(strings(&[
        "--runtime",
        "runc",
        "--network",
        "none",
        "--memory",
        "256m",
        "--memory-swap",
        "256m",
        "--cpus",
        "0.5",
        "--pids-limit",
        "128",
        "--read-only",
        "--tmpfs",
        "/tmp:rw,noexec,nosuid,size=64m",
        "--cap-drop",
        "ALL",
        "--security-opt",
        "no-new-privileges",
        "--user",
        "1000:1000",
        "--mount",
        "type=volume,source=bullpen-work-bot1,target=/work",
        "-w",
        "/work",
        "--label",
        "bullpen.bot=bot1",
        "debian:bookworm-slim",
        "sh",
        "-c",
        "echo hi",
    ]));

    assert_eq!(argv, &expected, "exec's hardened argv shape changed");
}

// F12/F2: the SAME assertion for `read_file` - this is how F2 shipped
// unhardened in the first place (only `cat` and the filename were ever
// checked).
#[tokio::test]
async fn read_file_argv_matches_the_hardened_shape_exactly() {
    let config = SandboxConfig::default();
    let runner = Arc::new(FakeRunner::new());
    let runner_clone: Arc<dyn CommandRunner> = runner.clone();
    let sandbox = DockerSandbox::new(config, runner_clone);

    runner.push_response("content", "", 0);
    let _ = sandbox.read_file("bot1", "data.txt").await;

    let commands = runner.commands();
    assert_eq!(commands.len(), 1);
    let argv = &commands[0];
    let name = container_name_from(argv);

    let mut expected = strings(&["docker", "run", "--rm", "--name"]);
    expected.push(name);
    expected.extend(strings(&[
        "--runtime",
        "runc",
        "--network",
        "none",
        "--memory",
        "256m",
        "--memory-swap",
        "256m",
        "--cpus",
        "0.5",
        "--pids-limit",
        "128",
        "--read-only",
        "--tmpfs",
        "/tmp:rw,noexec,nosuid,size=64m",
        "--cap-drop",
        "ALL",
        "--security-opt",
        "no-new-privileges",
        "--user",
        "1000:1000",
        "--mount",
        "type=volume,source=bullpen-work-bot1,target=/work,ro",
        "-w",
        "/work",
        "--label",
        "bullpen.bot=bot1",
        "debian:bookworm-slim",
        "cat",
        "data.txt",
    ]));

    assert_eq!(argv, &expected, "read_file's hardened argv shape changed");
}
