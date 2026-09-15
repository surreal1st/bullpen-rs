//! Integration tests for the egress module.
//! Ported from TypeScript `test/egress.test.ts`.
//! Two bites are required (both must turn red when guards are removed):
//! (a) allow a port outside `ALLOWED_PORTS`, show test goes red
//! (b) make `decide_connect` skip resolved-address check, show DNS-rebinding test goes red

use server::egress::{
    BotEgressConfig, EgressMode, EgressPolicy, Resolver, allowed_ports, decide_connect,
    effective_egress_policy, egress_enabled, host_allowed, parse_bot_egress, parse_connect,
    parse_policy, sanitize_bot_egress,
};
use std::sync::{Arc, Mutex};

#[test]
fn test_egress_enabled_off_by_default() {
    assert!(!egress_enabled(None));
    assert!(!egress_enabled(Some("")));
    assert!(!egress_enabled(Some("off")));
}

#[test]
fn test_egress_enabled_on() {
    assert!(egress_enabled(Some("on")));
    assert!(egress_enabled(Some("ON")));
    assert!(egress_enabled(Some("On")));
}

#[test]
fn test_allowed_ports_only_80_and_443() {
    let ports = allowed_ports();
    assert!(ports.contains(&80));
    assert!(ports.contains(&443));
    assert_eq!(ports.len(), 2);
}

#[test]
fn test_parse_policy_comma_separated() {
    let policy = parse_policy(Some("example.com,another.com"));
    assert_eq!(policy.allow, vec!["example.com", "another.com"]);
}

#[test]
fn test_parse_policy_whitespace_separated() {
    let policy = parse_policy(Some("example.com  another.com\n.sub.example.org"));
    assert_eq!(
        policy.allow,
        vec!["example.com", "another.com", ".sub.example.org"]
    );
}

#[test]
fn test_parse_policy_normalizes_case() {
    let policy = parse_policy(Some("EXAMPLE.COM"));
    assert_eq!(policy.allow, vec!["example.com"]);
}

#[test]
fn test_parse_policy_empty() {
    let policy = parse_policy(None);
    assert!(policy.allow.is_empty());
}

#[test]
fn test_host_allowed_exact_match() {
    let policy = EgressPolicy {
        allow: vec!["example.com".to_string()],
    };
    assert!(host_allowed("example.com", &policy));
    assert!(host_allowed("EXAMPLE.COM", &policy));
    assert!(host_allowed("example.com.", &policy));
}

#[test]
fn test_host_allowed_dot_prefix_matches_subdomains() {
    let policy = EgressPolicy {
        allow: vec![".example.com".to_string()],
    };
    assert!(host_allowed("example.com", &policy));
    assert!(host_allowed("sub.example.com", &policy));
    assert!(host_allowed("deep.sub.example.com", &policy));
}

#[test]
fn test_host_allowed_dot_prefix_does_not_match_suffix() {
    let policy = EgressPolicy {
        allow: vec![".example.com".to_string()],
    };
    assert!(!host_allowed("notexample.com", &policy));
    assert!(!host_allowed("example.com.attacker.net", &policy));
}

#[test]
fn test_host_allowed_empty_host() {
    let policy = EgressPolicy {
        allow: vec!["example.com".to_string()],
    };
    assert!(!host_allowed("", &policy));
    assert!(!host_allowed("   ", &policy));
}

#[test]
fn test_parse_connect_basic() {
    let result = parse_connect("CONNECT example.com:443 HTTP/1.1\r\nHost: example.com");
    assert_eq!(result, Some(("example.com".to_string(), 443)));
}

#[test]
fn test_parse_connect_http_1_0() {
    let result = parse_connect("CONNECT example.com:443 HTTP/1.0\r\n");
    assert_eq!(result, Some(("example.com".to_string(), 443)));
}

#[test]
fn test_parse_connect_ipv6() {
    let result = parse_connect("CONNECT [2001:db8::1]:443 HTTP/1.1\r\n");
    assert_eq!(result, Some(("2001:db8::1".to_string(), 443)));
}

#[test]
fn test_parse_connect_not_connect() {
    let result = parse_connect("GET / HTTP/1.1\r\nHost: example.com");
    assert_eq!(result, None);
}

#[test]
fn test_parse_connect_missing_port() {
    let result = parse_connect("CONNECT example.com HTTP/1.1\r\n");
    assert_eq!(result, None);
}

#[test]
fn test_parse_connect_invalid_port() {
    let result = parse_connect("CONNECT example.com:abcd HTTP/1.1\r\n");
    assert_eq!(result, None);
}

#[test]
fn test_parse_connect_ipv6_invalid() {
    let result = parse_connect("CONNECT [2001:db8::1 HTTP/1.1\r\n");
    assert_eq!(result, None);
}

#[test]
fn test_parse_bot_egress_valid() {
    let config = parse_bot_egress(Some(r#"{"mode":"allowlist","allow":["example.com"]}"#));
    assert_eq!(config.mode, EgressMode::Allowlist);
    assert_eq!(config.allow, vec!["example.com"]);
}

#[test]
fn test_parse_bot_egress_defaults() {
    let config = parse_bot_egress(None);
    assert_eq!(config.mode, EgressMode::Off);
    assert!(config.allow.is_empty());

    let config = parse_bot_egress(Some(""));
    assert_eq!(config.mode, EgressMode::Off);
    assert!(config.allow.is_empty());
}

#[test]
fn test_parse_bot_egress_normalizes_hosts() {
    let config = parse_bot_egress(Some(
        r#"{"mode":"allowlist","allow":["EXAMPLE.COM","  spaces.com  "]}"#,
    ));
    assert_eq!(config.allow, vec!["example.com", "spaces.com"]);
}

#[test]
fn test_sanitize_bot_egress() {
    let input = serde_json::json!({"mode":"allowlist","allow":["example.com"]});
    let config = sanitize_bot_egress(&input);
    assert_eq!(config.mode, EgressMode::Allowlist);
    assert_eq!(config.allow, vec!["example.com"]);
}

#[test]
fn test_sanitize_bot_egress_invalid() {
    let input = serde_json::json!("not an object");
    let config = sanitize_bot_egress(&input);
    assert_eq!(config.mode, EgressMode::Off);
    assert!(config.allow.is_empty());
}

#[test]
fn test_effective_egress_policy_master_switch_off() {
    let config = BotEgressConfig {
        mode: EgressMode::Allowlist,
        allow: vec!["example.com".to_string()],
    };
    let policy = effective_egress_policy(&config, false);
    assert!(policy.allow.is_empty());
}

#[test]
fn test_effective_egress_policy_mode_off() {
    let config = BotEgressConfig {
        mode: EgressMode::Off,
        allow: vec!["example.com".to_string()],
    };
    let policy = effective_egress_policy(&config, true);
    assert!(policy.allow.is_empty());
}

#[test]
fn test_effective_egress_policy_both_on() {
    let config = BotEgressConfig {
        mode: EgressMode::Allowlist,
        allow: vec!["example.com".to_string()],
    };
    let policy = effective_egress_policy(&config, true);
    assert_eq!(policy.allow, vec!["example.com"]);
}

// ============================================================================
// BITE (a): Port validation - test goes RED when port check is removed
// ============================================================================

struct FakeResolverSuccess;

#[async_trait::async_trait]
impl Resolver for FakeResolverSuccess {
    async fn resolve(&self, _host: &str) -> Result<Vec<String>, String> {
        Ok(vec!["1.2.3.4".to_string()])
    }
}

#[tokio::test]
async fn bite_a_port_outside_allowed_ports_is_refused() {
    // When decide_connect skips the port check, this test will fail
    let policy = EgressPolicy {
        allow: vec!["example.com".to_string()],
    };

    let resolver = FakeResolverSuccess;

    // Port 22 (SSH) is NOT in ALLOWED_PORTS
    let verdict = decide_connect("example.com", 22, &policy, &resolver).await;
    assert!(!verdict.ok);
    assert!(
        verdict.reason.contains("22"),
        "verdict should mention port 22: {}",
        verdict.reason
    );
    assert!(
        verdict.reason.contains("not allowed"),
        "verdict should say port is not allowed: {}",
        verdict.reason
    );
}

#[tokio::test]
async fn bite_a_allowed_ports_are_accepted() {
    let policy = EgressPolicy {
        allow: vec!["example.com".to_string()],
    };

    let resolver = FakeResolverSuccess;

    // Port 80 and 443 should pass the port check
    let verdict_80 = decide_connect("example.com", 80, &policy, &resolver).await;
    assert!(verdict_80.ok, "port 80 should be allowed");

    let verdict_443 = decide_connect("example.com", 443, &policy, &resolver).await;
    assert!(verdict_443.ok, "port 443 should be allowed");
}

// ============================================================================
// BITE (b): DNS-rebinding - test goes RED when resolved-address check removed
// ============================================================================

/// A recording fake resolver that tracks all calls it receives.
struct RecordingResolver {
    calls: Arc<Mutex<Vec<String>>>,
    response: Vec<String>,
}

impl RecordingResolver {
    fn new(response: Vec<String>) -> Self {
        RecordingResolver {
            calls: Arc::new(Mutex::new(Vec::new())),
            response,
        }
    }

    fn call_count(&self) -> usize {
        self.calls.lock().unwrap().len()
    }

    fn was_called_with(&self, host: &str) -> bool {
        self.calls.lock().unwrap().iter().any(|c| c == host)
    }
}

#[async_trait::async_trait]
impl Resolver for RecordingResolver {
    async fn resolve(&self, host: &str) -> Result<Vec<String>, String> {
        self.calls.lock().unwrap().push(host.to_string());
        Ok(self.response.clone())
    }
}

#[tokio::test]
async fn bite_b_dns_rebinding_allowed_name_to_loopback_is_refused() {
    // This test verifies that even when a hostname is on the allow list,
    // if it resolves to a private address (like 127.0.0.1), the connection is refused.
    // When the resolved-address check is removed, this test will fail.

    let policy = EgressPolicy {
        allow: vec!["allowed.example.com".to_string()],
    };

    let resolver = RecordingResolver::new(vec!["127.0.0.1".to_string()]);

    // allowed.example.com is on the list, but resolves to 127.0.0.1
    let verdict = decide_connect("allowed.example.com", 443, &policy, &resolver).await;

    // The connection should be refused because the resolved address is private
    assert!(
        !verdict.ok,
        "DNS-rebinding attack should be refused, verdict: {:?}",
        verdict
    );
    assert!(
        verdict.reason.contains("127.0.0.1"),
        "verdict should mention the private address: {}",
        verdict.reason
    );
    assert!(
        verdict.reason.contains("private"),
        "verdict should say the address is private: {}",
        verdict.reason
    );

    // 🔴 Most important: verify the resolver was actually called
    // A refusal that never resolved at all looks identical from the outside
    // to a correct refusal - so we MUST assert the mechanism, not just outcome
    assert_eq!(
        resolver.call_count(),
        1,
        "resolver should have been called exactly once"
    );
    assert!(
        resolver.was_called_with("allowed.example.com"),
        "resolver should have been called with the hostname"
    );
}

#[tokio::test]
async fn bite_b_allowed_name_to_public_address_is_accepted() {
    // Verify that an allowed name resolving to a public address is accepted
    let policy = EgressPolicy {
        allow: vec!["allowed.example.com".to_string()],
    };

    let resolver = RecordingResolver::new(vec!["1.2.3.4".to_string()]);

    let verdict = decide_connect("allowed.example.com", 443, &policy, &resolver).await;

    // The connection should be allowed
    assert!(
        verdict.ok,
        "public address should be allowed: {:?}",
        verdict
    );

    // Verify the resolver was called
    assert_eq!(resolver.call_count(), 1);
}

#[tokio::test]
async fn bite_b_dns_rebinding_multiple_addresses_any_private_refused() {
    // Even if most addresses are public, if any are private, refuse
    let policy = EgressPolicy {
        allow: vec!["mixed.example.com".to_string()],
    };

    let resolver = RecordingResolver::new(vec![
        "8.8.8.8".to_string(),
        "127.0.0.1".to_string(),
        "1.2.3.4".to_string(),
    ]);

    let verdict = decide_connect("mixed.example.com", 443, &policy, &resolver).await;

    // Should be refused because one address is private
    assert!(
        !verdict.ok,
        "should refuse if any address is private: {:?}",
        verdict
    );

    // Verify the resolver was called
    assert_eq!(resolver.call_count(), 1);
}

struct FakeResolverFailure;

#[async_trait::async_trait]
impl Resolver for FakeResolverFailure {
    async fn resolve(&self, _host: &str) -> Result<Vec<String>, String> {
        Err("DNS failure".to_string())
    }
}

#[tokio::test]
async fn bite_b_resolver_failure_is_refused() {
    let policy = EgressPolicy {
        allow: vec!["example.com".to_string()],
    };

    let resolver = FakeResolverFailure;

    let verdict = decide_connect("example.com", 443, &policy, &resolver).await;

    assert!(!verdict.ok);
    assert!(verdict.reason.contains("could not be resolved"));
}

struct FakeResolverEmpty;

#[async_trait::async_trait]
impl Resolver for FakeResolverEmpty {
    async fn resolve(&self, _host: &str) -> Result<Vec<String>, String> {
        Ok(Vec::new())
    }
}

#[tokio::test]
async fn bite_b_resolver_empty_response_is_refused() {
    let policy = EgressPolicy {
        allow: vec!["example.com".to_string()],
    };

    let resolver = FakeResolverEmpty;

    let verdict = decide_connect("example.com", 443, &policy, &resolver).await;

    assert!(!verdict.ok);
    assert!(verdict.reason.contains("resolved to nothing"));
}

#[tokio::test]
async fn bite_b_private_address_in_request_is_refused_before_resolve() {
    // A private address in the CONNECT itself should be refused
    // without even calling the resolver
    let policy = EgressPolicy {
        allow: vec!["127.0.0.1".to_string()], // even if on the list
    };

    let resolver = RecordingResolver::new(vec!["8.8.8.8".to_string()]);

    let verdict = decide_connect("127.0.0.1", 443, &policy, &resolver).await;

    // Should be refused for being a private address
    assert!(!verdict.ok);
    assert!(verdict.reason.contains("private address"));

    // The resolver should NOT have been called, because the request itself
    // contained a private address
    assert_eq!(resolver.call_count(), 0);
}

// ============================================================================
// S6-F-01: is_private_address is private, so decide_connect is the public
// seam that proves the fix reached production behaviour, not just the
// inline unit table in src/egress.rs. A literal address that IS private
// stops before host_allowed/resolve ever run (see the bracket test above);
// a literal that used to be treated as private (F3/F4) must now reach the
// resolver and be judged like any other allowed host.
// ============================================================================

#[tokio::test]
async fn s6_f_01_previously_missed_ipv6_forms_are_now_refused_as_private() {
    let literals = [
        "fe90::1",                  // F3: fe80::/10, quarter the old check missed
        "fea9::1",                  // F3: fe80::/10
        "febf::1",                  // F3: fe80::/10, top of the range
        "0:0:0:0:0:ffff:127.0.0.1", // F3: IPv4-mapped loopback, expanded
        "::127.0.0.1",              // F3: IPv4-compatible loopback, dotted
        "::7f00:1",                 // F3: IPv4-compatible loopback, hex
        "::ffff:0:127.0.0.1",       // F3: IPv4-translated loopback
        "0::1",                     // F3: loopback, not literal "::1"
        "0:0:0:0:0:0:0:1",          // F3: loopback, fully expanded
    ];

    for literal in literals {
        let policy = EgressPolicy {
            allow: vec![literal.to_string()],
        };
        let resolver = RecordingResolver::new(vec!["8.8.8.8".to_string()]);

        let verdict = decide_connect(literal, 443, &policy, &resolver).await;

        assert!(
            !verdict.ok,
            "{literal:?} must now be refused as a private address, verdict: {verdict:?}"
        );
        assert!(
            verdict.reason.contains("private address"),
            "{literal:?}: expected a private-address refusal, got: {}",
            verdict.reason
        );
        assert_eq!(
            resolver.call_count(),
            0,
            "{literal:?}: a literal private address must be refused before the resolver runs"
        );
    }
}

#[tokio::test]
async fn s6_f_01_fcc_gov_is_no_longer_treated_as_a_private_address() {
    // F4: a bare `starts_with("fc")`/`starts_with("fd")` on unvalidated text
    // treated any "fc"/"fd"-prefixed hostname as a ULA literal. fcc.gov must
    // reach the ordinary allow-list/resolve path like any other hostname.
    let policy = EgressPolicy {
        allow: vec!["fcc.gov".to_string()],
    };
    let resolver = RecordingResolver::new(vec!["23.1.2.3".to_string()]);

    let verdict = decide_connect("fcc.gov", 443, &policy, &resolver).await;

    assert!(
        verdict.ok,
        "fcc.gov must not be refused as a private address, verdict: {verdict:?}"
    );
    assert_eq!(
        resolver.call_count(),
        1,
        "fcc.gov should have reached the resolver, not been refused as an address literal"
    );
}

#[tokio::test]
async fn bite_b_not_on_allow_list_is_refused_before_resolve() {
    // A hostname not on the allow list should be refused
    // without even calling the resolver
    let policy = EgressPolicy {
        allow: vec!["allowed.com".to_string()],
    };

    let resolver = RecordingResolver::new(vec!["8.8.8.8".to_string()]);

    let verdict = decide_connect("notallowed.com", 443, &policy, &resolver).await;

    // Should be refused for not being on the list
    assert!(!verdict.ok);
    assert!(verdict.reason.contains("not on the allow list"));

    // The resolver should NOT have been called
    assert_eq!(resolver.call_count(), 0);
}
