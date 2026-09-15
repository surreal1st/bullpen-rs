//! Integration tests for S6-W-02b: proof that the egress path is
//! REACHABLE by construction, not just "the env vars exist".
//!
//! 🔴 S6-W-02's own 4 tests asserted exactly that - HTTP_PROXY/HTTPS_PROXY
//! were present and `--network none` survived - and passed while the
//! feature could not work at all: a container with `--network none` has no
//! network stack, so it can never reach a proxy regardless of what env
//! vars it is handed. See the ticket header in
//! `.scratch/bullpen-rs/tickets/S6-W-tickets.md` ("S6-W-02 shipped an
//! INERT egress path").
//!
//! **THE BITE.** Proof, from the RECORDED argv, of two branches mutated
//! SEPARATELY:
//! (a) egress off (`network` empty) -> `--network none`, and NOT ONE of
//!     the four proxy spellings anywhere in argv - even with a stray
//!     `egress_proxy_addr` set, so a regression that moves the proxy vars
//!     back onto the `none` branch cannot hide behind "well nobody sets
//!     that combination".
//! (b) egress on (`network` non-empty) -> the named network, all four
//!     proxy spellings (`http_proxy`/`https_proxy`/`HTTP_PROXY`/
//!     `HTTPS_PROXY`) pointing at the same address, and `--network none`
//!     is GONE - not merely "also present"; a container cannot be joined
//!     to both.

use server::sandbox::{DockerSandbox, FakeRunner, Sandbox, SandboxConfig};
use std::sync::Arc;

/// True when `flag value` appears as an adjacent pair anywhere in `args`.
fn has_flag_with_value(args: &[String], flag: &str, expected_value: &str) -> bool {
    args.windows(2)
        .any(|w| w[0] == flag && w[1] == expected_value)
}

/// The value of `VAR=...` for `var_name`, wherever it appears in `args` -
/// docker `-e VAR=value` pairs are self-describing, so this does not need
/// to also check the preceding `-e`.
fn env_value<'a>(args: &'a [String], var_name: &str) -> Option<&'a str> {
    let prefix = format!("{var_name}=");
    args.iter().find_map(|a| a.strip_prefix(prefix.as_str()))
}

const FOUR_PROXY_SPELLINGS: [&str; 4] = ["http_proxy", "https_proxy", "HTTP_PROXY", "HTTPS_PROXY"];

/// **Bite (a).** Egress off yields `--network none` and NO proxy env vars.
#[tokio::test]
async fn egress_off_yields_network_none_and_no_proxy_vars() {
    let config = SandboxConfig {
        network: String::new(),
        // Stray on purpose: even a leftover/misconfigured proxy address
        // must not leak onto the `--network none` branch.
        egress_proxy_addr: "10.0.0.9:3128".to_string(),
        ..Default::default()
    };

    let runner = FakeRunner::new();
    runner.push_response("ok\n", "", 0);
    let sandbox = DockerSandbox::new(config, Arc::new(runner.clone()));
    let _ = sandbox.exec("test-bot", "echo ok").await;

    let commands = runner.commands();
    assert_eq!(commands.len(), 1);
    let args = &commands[0];

    assert!(
        has_flag_with_value(args, "--network", "none"),
        "egress off must still get --network none, got: {args:?}"
    );
    for var in FOUR_PROXY_SPELLINGS {
        assert!(
            env_value(args, var).is_none(),
            "{var} must not appear with --network none, got: {args:?}"
        );
    }
}

/// **Bite (b).** Egress on yields the named network, all four proxy
/// spellings, and `--network none` is gone.
#[tokio::test]
async fn egress_on_yields_named_network_and_all_four_proxy_spellings() {
    let config = SandboxConfig {
        network: "bullpen-egress".to_string(),
        egress_proxy_addr: "10.0.0.9:3128".to_string(),
        ..Default::default()
    };

    let runner = FakeRunner::new();
    runner.push_response("ok\n", "", 0);
    let sandbox = DockerSandbox::new(config, Arc::new(runner.clone()));
    let _ = sandbox.exec("test-bot", "echo ok").await;

    let commands = runner.commands();
    assert_eq!(commands.len(), 1);
    let args = &commands[0];

    assert!(
        has_flag_with_value(args, "--network", "bullpen-egress"),
        "egress on must join the named network, got: {args:?}"
    );
    assert!(
        !has_flag_with_value(args, "--network", "none"),
        "must NOT also carry --network none - a container cannot join both, got: {args:?}"
    );
    for var in FOUR_PROXY_SPELLINGS {
        assert_eq!(
            env_value(args, var),
            Some("http://10.0.0.9:3128"),
            "{var} missing or wrong, got: {args:?}"
        );
    }
}

/// `read_file` shares `isolation_args` with `exec` - the whole point of
/// folding the proxy vars into `network_args`/`isolation_args` instead of
/// duplicating them per call site. Prove the egress-on branch reaches it
/// too, not just `exec`.
#[tokio::test]
async fn read_file_gets_the_same_egress_wiring_as_exec() {
    let config = SandboxConfig {
        network: "bullpen-egress".to_string(),
        egress_proxy_addr: "10.0.0.9:3128".to_string(),
        ..Default::default()
    };

    let runner = FakeRunner::new();
    runner.push_response("contents\n", "", 0);
    let sandbox = DockerSandbox::new(config, Arc::new(runner.clone()));
    let _ = sandbox.read_file("test-bot", "f.txt").await;

    let commands = runner.commands();
    assert_eq!(commands.len(), 1);
    let args = &commands[0];

    assert!(
        has_flag_with_value(args, "--network", "bullpen-egress"),
        "read_file must join the named network too, got: {args:?}"
    );
    for var in FOUR_PROXY_SPELLINGS {
        assert_eq!(
            env_value(args, var),
            Some("http://10.0.0.9:3128"),
            "read_file: {var} missing or wrong, got: {args:?}"
        );
    }
}

/// `read_file` on the egress-off branch also gets neither a network nor
/// proxy vars - the same bite (a) shape, on the other call site.
#[tokio::test]
async fn read_file_egress_off_yields_network_none_and_no_proxy_vars() {
    let config = SandboxConfig::default(); // network and egress_proxy_addr both empty

    let runner = FakeRunner::new();
    runner.push_response("contents\n", "", 0);
    let sandbox = DockerSandbox::new(config, Arc::new(runner.clone()));
    let _ = sandbox.read_file("test-bot", "f.txt").await;

    let commands = runner.commands();
    let args = &commands[0];

    assert!(
        has_flag_with_value(args, "--network", "none"),
        "got: {args:?}"
    );
    for var in FOUR_PROXY_SPELLINGS {
        assert!(
            env_value(args, var).is_none(),
            "{var} leaked, got: {args:?}"
        );
    }
}
