//! Network policy for sandboxes: DNS-checked allow-lists and CONNECT proxies.
//!
//! Ported from TypeScript `src/server/egress.ts` (lines 1-162, 240-436).
//! 🔴 SHIPS OFF. `BULLPEN_SANDBOX_EGRESS` is unset by default and the sandbox
//! keeps `--network none` exactly as before. Turning this on is the single
//! riskiest change available in this codebase: meridian's loopback carries SHOOT,
//! both CRMs, Brassrook, Switchboard and Cockpit, and "the sandbox has no network
//! at all" is currently the property that keeps a bot away from all of them.
//!
//! The design is Grok Bot's, read off its own schema rather than invented.
//! This module is the policy and validation only (not the proxy server itself).

use std::collections::HashSet;
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};

/// Whether egress is switched on at all. Off unless explicitly set.
pub fn egress_enabled(env: Option<&str>) -> bool {
    env.map(|s| s.to_lowercase())
        .map(|s| s.trim() == "on")
        .unwrap_or(false)
}

/// The only ports a sandbox may reach.
/// 🔴 Not "any port on an allowed host". `127.0.0.1:3014` opening Repull
/// must not also open sshd on 22, a Docker proxy on 2375 or a debugger on 9229.
pub fn allowed_ports() -> HashSet<u16> {
    [80, 443].iter().copied().collect()
}

/// A network egress policy: hostnames a sandbox may reach.
#[derive(Debug, Clone, PartialEq)]
pub struct EgressPolicy {
    /// Hostnames a sandbox may reach. Exact, or a leading dot for a subtree.
    pub allow: Vec<String>,
}

/// Parse a comma/whitespace-separated list of hostnames into a policy.
pub fn parse_policy(raw: Option<&str>) -> EgressPolicy {
    let allow = raw
        .unwrap_or("")
        .split(|c: char| c.is_whitespace() || c == ',')
        .map(|h| h.trim().to_lowercase())
        .filter(|h| !h.is_empty())
        .collect();
    EgressPolicy { allow }
}

/// Whether a hostname is on the policy list.
/// 🔴 The matching is the part that gets written wrong. A naive `ends_with`
/// matches "notexample.com" AND "example.com.attacker.net". A leading dot
/// means "this domain and its subdomains" and nothing else does.
pub fn host_allowed(host: &str, policy: &EgressPolicy) -> bool {
    let name = host.trim().to_lowercase().trim_end_matches('.').to_string();
    if name.is_empty() {
        return false;
    }

    policy.allow.iter().any(|entry| {
        if let Some(root) = entry.strip_prefix('.') {
            name == root || name.ends_with(entry)
        } else {
            name == *entry
        }
    })
}

/// A resolver that returns all addresses for a hostname.
#[async_trait::async_trait]
pub trait Resolver: Send + Sync {
    async fn resolve(&self, host: &str) -> Result<Vec<String>, String>;
}

/// The decision result for one CONNECT request.
#[derive(Debug, Clone, PartialEq)]
pub struct Verdict {
    pub ok: bool,
    pub reason: String,
}

/// Whether an IPv4 address is private, loopback, link-local, unspecified, in
/// the reserved `0.0.0.0/8` block, or CGNAT (`100.64.0.0/10`, RFC 6598 -
/// carrier-grade NAT space, not covered by `Ipv4Addr::is_private`).
fn is_private_ipv4(v4: Ipv4Addr) -> bool {
    v4.is_loopback()
        || v4.is_private()
        || v4.is_link_local()
        || v4.is_unspecified()
        || v4.octets()[0] == 0
        || is_cgnat(v4)
}

/// `100.64.0.0/10` - carrier-grade NAT space. Not "private" in RFC 1918's
/// sense, but not routable to a sandbox either.
fn is_cgnat(v4: Ipv4Addr) -> bool {
    let o = v4.octets();
    o[0] == 100 && (64..=127).contains(&o[1])
}

/// `fc00::/7` - unique local addresses. `std` has no stable check for this,
/// so it is a direct mask on the first segment, same shape as the link-local
/// check below.
fn is_ipv6_unique_local(v6: Ipv6Addr) -> bool {
    (v6.segments()[0] & 0xfe00) == 0xfc00
}

/// `fe80::/10` - link-local. `std::net::Ipv6Addr::is_unicast_link_local` was
/// unstable as of the toolchain this crate targets, so this is the same mask
/// check, done explicitly instead of the old `starts_with("fe80:")`, which
/// only matched `fe80::/16` - a quarter of the real range.
fn is_ipv6_link_local(v6: Ipv6Addr) -> bool {
    (v6.segments()[0] & 0xffc0) == 0xfe80
}

/// `fec0::/10` - site-local, deprecated by RFC 3879 but still parseable and
/// still configurable on a real internal network, which is the only thing
/// that matters to an SSRF guard. S6-F-01 left it classified public and
/// flagged the uncertainty; the orchestrator's call is to refuse it, on the
/// same reasoning every SSRF blocklist does: "deprecated" describes the
/// standards process, not what an attacker can reach.
fn is_ipv6_site_local(v6: Ipv6Addr) -> bool {
    (v6.segments()[0] & 0xffc0) == 0xfec0
}

/// IPv4-compatible IPv6 (RFC 4291 2.5.5.1, deprecated but still parseable):
/// `::a.b.c.d` - the high 96 bits are zero and the low 32 are an IPv4
/// address. Distinct from IPv4-*mapped* (`::ffff:a.b.c.d`, handled by
/// `to_ipv4_mapped` below): no `ffff` marker at all.
fn ipv4_compatible(v6: Ipv6Addr) -> Option<Ipv4Addr> {
    let s = v6.segments();
    (s[0..6] == [0, 0, 0, 0, 0, 0])
        .then(|| Ipv4Addr::new((s[6] >> 8) as u8, s[6] as u8, (s[7] >> 8) as u8, s[7] as u8))
}

/// IPv4-translated IPv6 (RFC 2765): `::ffff:0:a.b.c.d` - 64 zero bits, the
/// `ffff` marker, 16 *more* zero bits, then the IPv4 address. The extra zero
/// group is what makes this a different bit pattern from IPv4-mapped, and
/// why `to_ipv4_mapped` alone does not catch it.
fn ipv4_translated(v6: Ipv6Addr) -> Option<Ipv4Addr> {
    let s = v6.segments();
    (s[0..4] == [0, 0, 0, 0] && s[4] == 0xffff && s[5] == 0)
        .then(|| Ipv4Addr::new((s[6] >> 8) as u8, s[6] as u8, (s[7] >> 8) as u8, s[7] as u8))
}

/// Whether an IPv6 address is loopback, unspecified, link-local, unique
/// local, or wraps a private IPv4 address in any of the three textual forms
/// (mapped, compatible, translated) a resolver or a raw CONNECT host can
/// legally hand over.
fn is_private_ipv6(v6: Ipv6Addr) -> bool {
    if v6.is_loopback() || v6.is_unspecified() {
        return true;
    }
    if is_ipv6_link_local(v6) || is_ipv6_unique_local(v6) || is_ipv6_site_local(v6) {
        return true;
    }
    if let Some(v4) = v6.to_ipv4_mapped() {
        return is_private_ipv4(v4);
    }
    if let Some(v4) = ipv4_compatible(v6) {
        return is_private_ipv4(v4);
    }
    if let Some(v4) = ipv4_translated(v6) {
        return is_private_ipv4(v4);
    }
    false
}

/// Whether an IP address is private, loopback, link-local, or otherwise
/// internal. 🔴 Only classifies strings that actually PARSE as an IP address:
/// a bare hostname (`fcc.gov`, `fdns.example.com`) falls through to `false`
/// and is judged by `host_allowed` instead, never by a prefix guess on
/// unvalidated text.
pub fn is_private_address(ip: &str) -> bool {
    let raw = ip.trim();
    let raw = raw.strip_prefix('[').unwrap_or(raw);
    let raw = raw.strip_suffix(']').unwrap_or(raw);

    match raw.parse::<IpAddr>() {
        Ok(IpAddr::V4(v4)) => is_private_ipv4(v4),
        Ok(IpAddr::V6(v6)) => is_private_ipv6(v6),
        Err(_) => false,
    }
}

/// MCP connectors and OAuth discovery use the same private-address rule as
/// the web tool, but without a bot egress allow-list — any public MCP host
/// is reachable once authorized.
pub async fn refuse_if_resolves_private(host: &str, resolver: &dyn Resolver) -> Result<(), String> {
    if is_private_address(host) {
        return Err(format!("{host} is inside this network. Refused."));
    }

    let addresses = resolver
        .resolve(host)
        .await
        .map_err(|e| format!("could not resolve {host}: {e}"))?;

    if let Some(priv_addr) = addresses.iter().find(|a| is_private_address(a)) {
        return Err(format!(
            "{host} resolves to {priv_addr}, which is inside this network. Refused."
        ));
    }
    Ok(())
}

/// The whole decision for one CONNECT request.
/// 🔴 Name check AND address check, because neither covers the other. The name
/// check stops a bot reaching an arbitrary site; the address check stops an
/// allowed name that RESOLVES to meridian's own loopback, which is what a
/// DNS-rebinding entry in the allow-list would do.
pub async fn decide_connect(
    host: &str,
    port: u16,
    policy: &EgressPolicy,
    resolver: &dyn Resolver,
) -> Verdict {
    if !allowed_ports().contains(&port) {
        return Verdict {
            ok: false,
            reason: format!("port {} is not allowed", port),
        };
    }

    // A literal address can never be on a hostname allow-list.
    if is_private_address(host) {
        return Verdict {
            ok: false,
            reason: "a private address".to_string(),
        };
    }

    if !host_allowed(host, policy) {
        return Verdict {
            ok: false,
            reason: format!("{} is not on the allow list", host),
        };
    }

    let addresses = match resolver.resolve(host).await {
        Ok(addrs) => addrs,
        Err(_) => {
            // Refuse on a resolver failure rather than allow.
            return Verdict {
                ok: false,
                reason: format!("{} could not be resolved", host),
            };
        }
    };

    if addresses.is_empty() {
        return Verdict {
            ok: false,
            reason: format!("{} resolved to nothing", host),
        };
    }

    if let Some(priv_addr) = addresses.iter().find(|a| is_private_address(a)) {
        return Verdict {
            ok: false,
            reason: format!("{} resolves to {}, which is private", host, priv_addr),
        };
    }

    Verdict {
        ok: true,
        reason: "allowed".to_string(),
    }
}

/// Reads a CONNECT request line. Returns None for anything that is not one.
pub fn parse_connect(head: &str) -> Option<(String, u16)> {
    let line = head.lines().next().unwrap_or("");
    let line = line.trim();

    if let Some(caps) = regex::Regex::new(r"^CONNECT\s+(\S+)\s+HTTP/1\.[01]$")
        .ok()
        .and_then(|re| re.captures(line))
        && let Some(m) = caps.get(1)
    {
        let target = m.as_str();

        // IPv6 literals arrive bracketed: [::1]:443
        // Must have both opening and closing brackets
        if target.starts_with('[') {
            if let Some(caps) = regex::Regex::new(r"^\[([^\]]+)\]:(\d+)$")
                .ok()
                .and_then(|re| re.captures(target))
                && let (Some(h), Some(p)) = (caps.get(1), caps.get(2))
                && let Ok(port) = p.as_str().parse::<u16>()
            {
                return Some((h.as_str().to_string(), port));
            }
            // If it starts with [ but doesn't match the full pattern, it's invalid
            return None;
        }

        // IPv4 or hostname
        if let Some(at) = target.rfind(':') {
            let host = &target[..at];
            if let Ok(port) = target[at + 1..].parse::<u16>()
                && !host.is_empty()
            {
                return Some((host.to_string(), port));
            }
        }
    }

    None
}

/// Bot-level egress mode.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EgressMode {
    Off,
    Allowlist,
}

impl EgressMode {
    pub fn as_str(&self) -> &str {
        match self {
            EgressMode::Off => "off",
            EgressMode::Allowlist => "allowlist",
        }
    }
}

/// Per-bot egress configuration.
#[derive(Debug, Clone, PartialEq)]
pub struct BotEgressConfig {
    pub mode: EgressMode,
    pub allow: Vec<String>,
}

/// Default bot egress: no network, nothing on the list.
pub const DEFAULT_BOT_EGRESS: BotEgressConfig = BotEgressConfig {
    mode: EgressMode::Off,
    allow: Vec::new(),
};

/// Clean and normalize an allow list.
fn clean_allow_list(input: &serde_json::Value) -> Vec<String> {
    match input {
        serde_json::Value::Array(arr) => arr
            .iter()
            .filter_map(|v| v.as_str())
            .map(|s| s.trim().to_lowercase())
            .filter(|s| !s.is_empty())
            .collect(),
        _ => Vec::new(),
    }
}

/// Clean and normalize a mode string.
fn clean_mode(input: &serde_json::Value) -> EgressMode {
    match input {
        serde_json::Value::String(s) if s == "allowlist" => EgressMode::Allowlist,
        _ => EgressMode::Off,
    }
}

/// Parse a bot's `egress` column from the database.
/// 🔴 Anything that fails to parse comes back as DEFAULT_BOT_EGRESS.
pub fn parse_bot_egress(raw: Option<&str>) -> BotEgressConfig {
    if let Some(s) = raw {
        if s.trim().is_empty() {
            return BotEgressConfig {
                mode: DEFAULT_BOT_EGRESS.mode,
                allow: DEFAULT_BOT_EGRESS.allow.clone(),
            };
        }

        if let Ok(parsed) = serde_json::from_str::<serde_json::Value>(s)
            && let serde_json::Value::Object(obj) = parsed
        {
            let mode = clean_mode(obj.get("mode").unwrap_or(&serde_json::Value::Null));
            let allow = clean_allow_list(obj.get("allow").unwrap_or(&serde_json::Value::Null));
            return BotEgressConfig { mode, allow };
        }
    }

    BotEgressConfig {
        mode: DEFAULT_BOT_EGRESS.mode,
        allow: DEFAULT_BOT_EGRESS.allow.clone(),
    }
}

/// Validate and normalize user input before storage.
pub fn sanitize_bot_egress(input: &serde_json::Value) -> BotEgressConfig {
    match input {
        serde_json::Value::Object(obj) => {
            let mode = clean_mode(obj.get("mode").unwrap_or(&serde_json::Value::Null));
            let allow = clean_allow_list(obj.get("allow").unwrap_or(&serde_json::Value::Null));
            BotEgressConfig { mode, allow }
        }
        _ => BotEgressConfig {
            mode: DEFAULT_BOT_EGRESS.mode,
            allow: DEFAULT_BOT_EGRESS.allow.clone(),
        },
    }
}

/// The policy a bot's sandbox actually gets, given both the master switch and its own row.
/// 🔴 This is where "the master switch off wins" is a single, tested fact.
pub fn effective_egress_policy(
    config: &BotEgressConfig,
    egress_enabled_flag: bool,
) -> EgressPolicy {
    if !egress_enabled_flag {
        return EgressPolicy { allow: Vec::new() };
    }
    if config.mode != EgressMode::Allowlist {
        return EgressPolicy { allow: Vec::new() };
    }
    EgressPolicy {
        allow: config.allow.clone(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_egress_enabled() {
        assert!(!egress_enabled(None));
        assert!(!egress_enabled(Some("off")));
        assert!(egress_enabled(Some("on")));
        assert!(egress_enabled(Some("ON")));
    }

    #[test]
    fn test_parse_policy() {
        let policy = parse_policy(Some("example.com, .example.org"));
        assert_eq!(policy.allow, vec!["example.com", ".example.org"]);

        let policy = parse_policy(None);
        assert!(policy.allow.is_empty());
    }

    #[test]
    fn test_host_allowed() {
        let policy = EgressPolicy {
            allow: vec!["example.com".to_string(), ".sub.example.org".to_string()],
        };

        assert!(host_allowed("example.com", &policy));
        assert!(host_allowed("EXAMPLE.COM", &policy));
        assert!(!host_allowed("notexample.com", &policy));

        assert!(host_allowed("sub.example.org", &policy));
        assert!(host_allowed("foo.sub.example.org", &policy));
        assert!(!host_allowed("example.org", &policy));
    }

    #[test]
    fn test_parse_connect_ipv4() {
        let result = parse_connect("CONNECT example.com:443 HTTP/1.1\r\n");
        assert_eq!(result, Some(("example.com".to_string(), 443)));
    }

    #[test]
    fn test_parse_connect_ipv6() {
        let result = parse_connect("CONNECT [::1]:443 HTTP/1.1\r\n");
        assert_eq!(result, Some(("::1".to_string(), 443)));
    }

    #[test]
    fn test_parse_connect_invalid() {
        assert_eq!(parse_connect("GET / HTTP/1.1\r\n"), None);
        assert_eq!(parse_connect("CONNECT example.com\r\n"), None);
    }

    #[test]
    fn test_is_private_address_ipv4_loopback() {
        assert!(is_private_address("127.0.0.1"));
        assert!(is_private_address("127.0.0.2"));
    }

    #[test]
    fn test_is_private_address_ipv4_private() {
        assert!(is_private_address("10.0.0.1"));
        assert!(is_private_address("172.16.0.1"));
        assert!(is_private_address("172.31.255.255"));
        assert!(is_private_address("192.168.1.1"));
        assert!(is_private_address("169.254.1.1"));
        assert!(is_private_address("0.0.0.1"));
    }

    #[test]
    fn test_is_private_address_ipv4_public() {
        assert!(!is_private_address("8.8.8.8"));
        assert!(!is_private_address("1.1.1.1"));
    }

    #[test]
    fn test_is_private_address_ipv6_loopback() {
        assert!(is_private_address("::1"));
        assert!(is_private_address("::"));
    }

    #[test]
    fn test_is_private_address_ipv6_link_local() {
        assert!(is_private_address("fe80::1"));
    }

    #[test]
    fn test_is_private_address_ipv6_private() {
        assert!(is_private_address("fc00::1"));
        assert!(is_private_address("fd00::1"));
    }

    #[test]
    fn test_is_private_address_ipv6_mapped_ipv4_loopback() {
        assert!(is_private_address("::ffff:127.0.0.1"));
        assert!(is_private_address("::ffff:7f00:1"));
    }

    #[test]
    fn test_is_private_address_ipv6_mapped_ipv4_private() {
        assert!(is_private_address("::ffff:192.168.1.1"));
    }

    #[test]
    fn test_is_private_address_with_brackets() {
        assert!(is_private_address("[127.0.0.1]"));
        assert!(is_private_address("[::1]"));
    }

    /// S6-F-01: this is the SSRF boundary for the sensitive-data tier, so it
    /// gets a table, not three examples. Every literal the reviewer measured
    /// against the old regex-based function (S6-R.md Q1/F3/F4), plus the
    /// literals that were already correct and the boundary cases that prove
    /// the fe80::/10 and fc00::/7 masks and the 100.64.0.0/10 CGNAT check
    /// are not over- or under-inclusive.
    #[test]
    fn test_is_private_address_table() {
        let cases: &[(&str, bool, &str)] = &[
            // --- already refused before this rewrite; must stay refused ---
            ("::1", true, "loopback"),
            ("::", true, "unspecified"),
            ("fc00::1", true, "ULA, low end of fc00::/7"),
            ("fd12:3456::1", true, "ULA, fd half of fc00::/7"),
            (
                "fe80::1",
                true,
                "link-local, fe80::/16 (old code's only slice)",
            ),
            ("fe80:0:0:0:0:0:0:1", true, "link-local, expanded form"),
            ("0.0.0.0", true, "unspecified v4"),
            ("169.254.169.254", true, "link-local v4 (cloud metadata)"),
            ("::ffff:127.0.0.1", true, "IPv4-mapped loopback, dotted"),
            ("::ffff:7f00:1", true, "IPv4-mapped loopback, hex"),
            ("::ffff:a9fe:a9fe", true, "IPv4-mapped metadata, hex"),
            (
                "::ffff:169.254.169.254",
                true,
                "IPv4-mapped metadata, dotted",
            ),
            ("::FFFF:127.0.0.1", true, "IPv4-mapped loopback, uppercase"),
            ("::ffff:c0a8:1", true, "IPv4-mapped 192.168.0.1"),
            ("[::1]", true, "bracketed loopback"),
            (
                "[::ffff:127.0.0.1]",
                true,
                "bracketed mapped loopback, dotted",
            ),
            ("[::ffff:7f00:1]", true, "bracketed mapped loopback, hex"),
            ("100.64.0.1", true, "CGNAT, low end of 100.64.0.0/10"),
            ("10.1.2.3", true, "RFC 1918 10/8"),
            ("172.16.0.1", true, "RFC 1918 172.16/12, low end"),
            ("172.31.255.255", true, "RFC 1918 172.16/12, high end"),
            ("192.168.1.1", true, "RFC 1918 192.168/16"),
            // --- F3: NOT refused before this rewrite; must now be refused ---
            (
                "fe90::1",
                true,
                "F3: fe80::/10, quarter the old check missed",
            ),
            (
                "fea9::1",
                true,
                "F3: fe80::/10, quarter the old check missed",
            ),
            ("febf::1", true, "F3: fe80::/10, top of the range"),
            (
                "0:0:0:0:0:ffff:127.0.0.1",
                true,
                "F3: IPv4-mapped loopback, fully expanded",
            ),
            (
                "0000:0000:0000:0000:0000:ffff:7f00:0001",
                true,
                "F3: IPv4-mapped loopback, fully expanded hex",
            ),
            ("::127.0.0.1", true, "F3: IPv4-compatible loopback, dotted"),
            ("::7f00:1", true, "F3: IPv4-compatible loopback, hex"),
            ("::a9fe:a9fe", true, "F3: IPv4-compatible metadata, hex"),
            (
                "::ffff:0:127.0.0.1",
                true,
                "F3: IPv4-translated loopback (extra zero group)",
            ),
            ("0::1", true, "F3: loopback, not the literal string \"::1\""),
            (
                "0:0:0:0:0:0:0:1",
                true,
                "F3: loopback, fully expanded, not \"::1\"",
            ),
            ("::0.0.0.0", true, "F3: IPv4-compatible unspecified"),
            // --- F4: wrongly refused before this rewrite; must now be allowed ---
            ("fcc.gov", false, "F4: hostname, not an address at all"),
            ("fdns.example.com", false, "F4: hostname starting \"fd\""),
            ("fcbook.com", false, "F4: hostname starting \"fc\""),
            // --- boundary cases proving the masks are exact, not sloppy ---
            ("fe7f::1", false, "just below fe80::/10"),
            (
                "fec0::1",
                true,
                "fec0::/10 site-local: RFC 3879 deprecated it, an internal                  network can still use it, so an SSRF guard refuses it",
            ),
            ("feff:ffff::1", true, "top of fec0::/10"),
            ("fe40::1", false, "below fe80::/10, not site-local either"),
            ("fdff:ffff::1", true, "top of fc00::/7"),
            ("fe00::1", false, "just above fc00::/7"),
            ("100.63.255.255", false, "just below CGNAT 100.64.0.0/10"),
            ("100.127.255.255", true, "top of CGNAT 100.64.0.0/10"),
            ("100.128.0.0", false, "just above CGNAT 100.64.0.0/10"),
            // --- ordinary public addresses that must be ALLOWED ---
            ("8.8.8.8", false, "public v4"),
            ("1.1.1.1", false, "public v4"),
            ("93.184.215.14", false, "public v4 (example.com)"),
            ("2001:4860:4860::8888", false, "public v6 (Google DNS)"),
            ("2606:2800:220:1:248:1893:25c8:1946", false, "public v6"),
            // --- not an address at all ---
            ("example.com", false, "ordinary hostname"),
            ("not-an-ip", false, "garbage"),
            ("", false, "empty string"),
        ];

        let mut failures = Vec::new();
        for (input, expected, why) in cases {
            let actual = is_private_address(input);
            if actual != *expected {
                failures.push(format!(
                    "{input:?} ({why}): expected {expected}, got {actual}"
                ));
            }
        }
        assert!(
            failures.is_empty(),
            "is_private_address table mismatches:\n{}",
            failures.join("\n")
        );
    }

    #[test]
    fn test_parse_bot_egress() {
        let config = parse_bot_egress(Some(r#"{"mode":"allowlist","allow":["example.com"]}"#));
        assert_eq!(config.mode, EgressMode::Allowlist);
        assert_eq!(config.allow, vec!["example.com"]);

        let config = parse_bot_egress(None);
        assert_eq!(config.mode, EgressMode::Off);
        assert!(config.allow.is_empty());

        let config = parse_bot_egress(Some(""));
        assert_eq!(config.mode, EgressMode::Off);
    }

    #[test]
    fn test_sanitize_bot_egress() {
        let input = serde_json::json!({"mode":"allowlist","allow":["example.com"]});
        let config = sanitize_bot_egress(&input);
        assert_eq!(config.mode, EgressMode::Allowlist);
        assert_eq!(config.allow, vec!["example.com"]);
    }

    #[test]
    fn test_effective_egress_policy_off() {
        let config = BotEgressConfig {
            mode: EgressMode::Allowlist,
            allow: vec!["example.com".to_string()],
        };
        let policy = effective_egress_policy(&config, false);
        assert!(policy.allow.is_empty());
    }

    #[test]
    fn test_effective_egress_policy_on() {
        let config = BotEgressConfig {
            mode: EgressMode::Allowlist,
            allow: vec!["example.com".to_string()],
        };
        let policy = effective_egress_policy(&config, true);
        assert_eq!(policy.allow, vec!["example.com"]);
    }
}
