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

/// Whether an IP address is private, loopback, link-local, or otherwise internal.
/// 🔴 `::ffff:127.0.0.1` is why this is not just a prefix check. An IPv4-mapped
/// IPv6 address is loopback wearing a hat.
fn is_private_address(ip: &str) -> bool {
    let raw = ip.trim().to_lowercase();
    let raw = raw.trim_start_matches('[').trim_end_matches(']');

    // Unwrap IPv4-mapped and IPv4-compatible IPv6 before anything else, in both
    // the dotted form (::ffff:127.0.0.1) and the hex form (::ffff:7f00:1).
    if let Some(caps) = regex::Regex::new(r"^::ffff:(\d+\.\d+\.\d+\.\d+)$")
        .ok()
        .and_then(|re| re.captures(raw))
        && let Some(m) = caps.get(1)
    {
        return is_private_address(m.as_str());
    }

    if let Some(caps) = regex::Regex::new(r"^::ffff:([0-9a-f]{1,4}):([0-9a-f]{1,4})$")
        .ok()
        .and_then(|re| re.captures(raw))
        && let (Some(m1), Some(m2)) = (caps.get(1), caps.get(2))
        && let (Ok(high), Ok(low)) = (
            u16::from_str_radix(m1.as_str(), 16),
            u16::from_str_radix(m2.as_str(), 16),
        )
    {
        let ipv4 = format!(
            "{}.{}.{}.{}",
            (high >> 8) & 255,
            high & 255,
            (low >> 8) & 255,
            low & 255
        );
        return is_private_address(&ipv4);
    }

    if raw == "::1" || raw == "::" {
        return true;
    }
    if raw.starts_with("fe80:") || raw.starts_with("fc") || raw.starts_with("fd") {
        return true;
    }

    let parts: Vec<&str> = raw.split('.').collect();
    if parts.len() != 4 {
        return false;
    }

    let nums: Result<Vec<u8>, _> = parts.iter().map(|p| p.parse::<u8>()).collect();
    if let Ok(nums) = nums {
        let a = nums[0];
        let b = nums[1];

        if a == 10 || a == 127 || a == 0 {
            return true;
        }
        if a == 169 && b == 254 {
            return true;
        }
        if a == 172 && (16..=31).contains(&b) {
            return true;
        }
        if a == 192 && b == 168 {
            return true;
        }
        if a == 100 && (64..=127).contains(&b) {
            return true;
        }
    }

    false
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
