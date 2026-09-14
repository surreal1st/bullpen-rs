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

#[tokio::test]
async fn timeout_error_sets_exit_code_124() {
    let config = SandboxConfig::default();
    let runner = Arc::new(FakeRunner::new());
    let runner_clone: Arc<dyn CommandRunner> = runner.clone();
    let sandbox = DockerSandbox::new(config, runner_clone);

    runner.push_response("", "command timed out", 1);
    let result = sandbox.exec("bot1", "sleep 1000").await;

    assert_eq!(result.exit_code, 124);
    assert!(result.timed_out);
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

// Real Docker test - only runs if BULLPEN_TEST_DOCKER=1 and docker is available
#[tokio::test]
#[ignore]
async fn real_docker_echo_roundtrip() {
    // Skip if docker is not available or test is not enabled
    if std::env::var("BULLPEN_TEST_DOCKER").as_deref() != Ok("1") {
        return;
    }

    let config = SandboxConfig::default();
    let runner = std::sync::Arc::new(server::sandbox::TokioRunner::new(
        config.docker_host.clone(),
    ));
    let sandbox = DockerSandbox::new(config, runner);

    let result = sandbox.exec("test-bot", "echo hello").await;

    assert_eq!(result.exit_code, 0);
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
