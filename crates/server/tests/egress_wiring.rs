//! Integration tests for egress proxy wiring in sandbox.rs.
//!
//! 🔴 **THE BITE (S6-W-02).** Proof that:
//! (a) When a bot has egress proxy configured (non-empty `egress_proxy_addr`),
//!     the container arguments include HTTP_PROXY/HTTPS_PROXY env vars.
//! (b) The `--network none` flag survives in the recorded arguments - the
//!     container NEVER gets bare network access, only proxy access.
//! (c) When proxy is not configured (empty `egress_proxy_addr`), no proxy
//!     env vars are added.

use server::sandbox::{DockerSandbox, FakeRunner, Sandbox, SandboxConfig};
use std::sync::Arc;

/// Helper to extract an argument from the command line.
fn find_arg(args: &[String], needle: &str) -> Option<usize> {
    args.iter().position(|a| a == needle)
}

/// Helper to check if a flag-value pair exists anywhere in the args.
/// For -e flags with VAR=VALUE, pass the full string "VAR=VALUE" as the expected_value.
fn has_flag_with_value(args: &[String], flag: &str, expected_value: &str) -> bool {
    let mut i = 0;
    while i < args.len() {
        if args[i] == flag && i + 1 < args.len() && args[i + 1] == expected_value {
            return true;
        }
        i += 1;
    }
    false
}

/// Helper to count occurrences of an argument (for env vars passed with -e).
fn count_env_vars(args: &[String], var_name: &str) -> usize {
    let mut count = 0;
    let mut i = 0;
    while i < args.len() {
        if args[i] == "-e" && i + 1 < args.len() {
            if args[i + 1].starts_with(&format!("{}=", var_name)) {
                count += 1;
            }
            i += 2;
        } else {
            i += 1;
        }
    }
    count
}

#[tokio::test]
async fn proxy_env_vars_added_when_proxy_configured() {
    let config = SandboxConfig {
        egress_proxy_addr: "127.0.0.1:12345".to_string(),
        ..Default::default()
    };

    let runner = FakeRunner::new();
    runner.push_response("ok\n", "", 0);

    let sandbox = DockerSandbox::new(config, Arc::new(runner.clone()));
    let _ = sandbox.exec("test-bot", "echo ok").await;

    let commands = runner.commands();
    assert_eq!(commands.len(), 1, "should have run one command");

    let args = &commands[0];

    // Should have HTTP_PROXY and HTTPS_PROXY env vars
    assert!(
        has_flag_with_value(args, "-e", "HTTP_PROXY=http://127.0.0.1:12345"),
        "args should include -e HTTP_PROXY=http://127.0.0.1:12345, got: {args:?}"
    );
    assert!(
        has_flag_with_value(args, "-e", "HTTPS_PROXY=http://127.0.0.1:12345"),
        "args should include -e HTTPS_PROXY=http://127.0.0.1:12345, got: {args:?}"
    );

    // Verify NO_PROXY is also set
    assert!(
        find_arg(args, "-e").is_some(),
        "should have at least one -e flag for NO_PROXY"
    );
    let has_no_proxy = args.iter().any(|a| a.starts_with("NO_PROXY="));
    assert!(has_no_proxy, "should have NO_PROXY env var");
}

/// **The bite (b) target.** `--network none` MUST survive even with proxy.
#[tokio::test]
async fn network_none_survives_with_proxy() {
    let config = SandboxConfig {
        egress_proxy_addr: "127.0.0.1:12345".to_string(),
        ..Default::default()
    };

    let runner = FakeRunner::new();
    runner.push_response("ok\n", "", 0);

    let sandbox = DockerSandbox::new(config, Arc::new(runner.clone()));
    let _ = sandbox.exec("test-bot", "echo ok").await;

    let commands = runner.commands();
    let args = &commands[0];

    // Check for `--network none`
    assert!(
        has_flag_with_value(args, "--network", "none"),
        "args must include --network none even with proxy, got: {args:?}"
    );
}

/// **The bite (c) target.** No proxy env vars when proxy is not configured.
#[tokio::test]
async fn no_proxy_env_vars_when_proxy_not_configured() {
    let config = SandboxConfig::default(); // egress_proxy_addr is empty by default

    let runner = FakeRunner::new();
    runner.push_response("ok\n", "", 0);

    let sandbox = DockerSandbox::new(config, Arc::new(runner.clone()));
    let _ = sandbox.exec("test-bot", "echo ok").await;

    let commands = runner.commands();
    let args = &commands[0];

    // Should NOT have HTTP_PROXY or HTTPS_PROXY
    let http_proxy_count = count_env_vars(args, "HTTP_PROXY");
    let https_proxy_count = count_env_vars(args, "HTTPS_PROXY");

    assert_eq!(
        http_proxy_count, 0,
        "should not have HTTP_PROXY when egress_proxy_addr is empty, got: {args:?}"
    );
    assert_eq!(
        https_proxy_count, 0,
        "should not have HTTPS_PROXY when egress_proxy_addr is empty, got: {args:?}"
    );

    // But `--network none` should still be there
    assert!(
        has_flag_with_value(args, "--network", "none"),
        "args must include --network none, got: {args:?}"
    );
}

/// read_file should also get proxy env vars when configured.
#[tokio::test]
async fn read_file_includes_proxy_env_vars() {
    let config = SandboxConfig {
        egress_proxy_addr: "127.0.0.1:12345".to_string(),
        ..Default::default()
    };

    let runner = FakeRunner::new();
    runner.push_response("file contents\n", "", 0);

    let sandbox = DockerSandbox::new(config, Arc::new(runner.clone()));
    let _ = sandbox.read_file("test-bot", "test.txt").await;

    let commands = runner.commands();
    let args = &commands[0];

    // Should have HTTP_PROXY env var
    assert!(
        has_flag_with_value(args, "-e", "HTTP_PROXY=http://127.0.0.1:12345"),
        "read_file args should include HTTP_PROXY, got: {args:?}"
    );

    // And still have --network none
    assert!(
        has_flag_with_value(args, "--network", "none"),
        "read_file args must include --network none, got: {args:?}"
    );
}
