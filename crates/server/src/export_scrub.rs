//! S10-10: scrub secrets and credential-bearing URLs from bot export bodies.
//! TS export is still raw instructions; this closes the Grok-gap called out in
//! PLAN.md (template export with secret/URL scrub).

use model::{KeySource, redact};
use regex::Regex;
use std::sync::LazyLock;

static URL_WITH_CREDS: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?i)(https?://)[^/\s:@]+:[^@\s/]+@").expect("url cred scrub regex")
});

/// Scrub text before it leaves the server as an export attachment.
pub fn scrub_export_text(text: &str) -> String {
    let key = KeySource::Env.resolve();
    let key_ref = key.as_deref();
    let redacted = redact(text, key_ref);
    URL_WITH_CREDS
        .replace_all(&redacted, "${1}[redacted]@")
        .into_owned()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn scrubs_openrouter_shaped_tokens() {
        let out = scrub_export_text("key sk-or-abcdefghijklmnopqrstuvwxyz here");
        assert!(!out.contains("sk-or-abcdefghijklmnopqrstuvwxyz"));
        assert!(out.contains("[redacted]"));
    }

    #[test]
    fn scrubs_userinfo_in_urls() {
        let out = scrub_export_text("hook https://alice:sekret@example.com/path");
        assert!(!out.contains("alice:sekret"));
        assert!(out.contains("https://[redacted]@example.com/path"));
    }

    #[test]
    fn leaves_benign_urls_alone() {
        let url = "see https://example.com/docs";
        assert_eq!(scrub_export_text(url), url);
    }
}
