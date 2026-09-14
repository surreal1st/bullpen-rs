//! The OpenRouter key: where it comes from, and how it never leaks. Port of
//! `secrets.ts`'s `getOpenRouterKey`/`requireOpenRouterKey`/`redact` (the
//! settings-table encryption half of that file is out of scope here - no
//! `store` access from this ticket).
//!
//! Rules this file exists to enforce:
//! - The value never appears on a command line or in a process argument.
//! - The value never appears in a log line, an error message, or a response.
//!   `redact` is applied to everything that leaves the model boundary on the
//!   error path.
//! - A missing or unreadable key file means "no key", never a panic: the
//!   caller reports it through a `ModelEvent::Error`, the same way a bad
//!   upstream response would.

use std::path::{Path, PathBuf};

/// Env var naming a key file outside the repo. Checked before `KEY_VAR`.
pub const KEY_FILE_VAR: &str = "BULLPEN_OPENROUTER_KEY_FILE";
/// Env var carrying the key value inline. Checked when `KEY_FILE_VAR` is unset.
pub const KEY_VAR: &str = "BULLPEN_OPENROUTER_KEY";

/// Where the OpenRouter key comes from. Resolved fresh on every call - unlike
/// the TS original's module-level memo (`cached`/`loaded`/`resetSecretsForTest`),
/// there is no global cache to reset between tests, since env vars set with
/// `std::env::set_var` are already visible to the very next `resolve()`.
#[derive(Debug, Clone, Default)]
pub enum KeySource {
    /// Resolve from `BULLPEN_OPENROUTER_KEY_FILE` then `BULLPEN_OPENROUTER_KEY`
    /// at call time. The default, and what production runs with.
    #[default]
    Env,
    /// Read this file every time `resolve()` is called.
    File(PathBuf),
    /// Use this exact value. For tests only - production never hardcodes one.
    Inline(String),
}

impl KeySource {
    /// The key, or `None` if nothing is configured or the file could not be
    /// read.
    pub fn resolve(&self) -> Option<String> {
        match self {
            KeySource::Inline(v) => non_empty(v.clone()),
            KeySource::File(path) => read_trimmed(path),
            KeySource::Env => {
                if let Ok(file) = std::env::var(KEY_FILE_VAR)
                    && !file.is_empty()
                {
                    return read_trimmed(Path::new(&file));
                }
                std::env::var(KEY_VAR).ok().and_then(non_empty)
            }
        }
    }
}

fn non_empty(v: String) -> Option<String> {
    let v = v.trim().to_string();
    if v.is_empty() { None } else { Some(v) }
}

fn read_trimmed(path: &Path) -> Option<String> {
    // A missing or unreadable key file means "no key", never a crash - the
    // TS original learned this the hard way: the first deploy pointed at a
    // file that did not exist yet and threw at import time, restart-looping
    // the unit with no way to see the health page that would have explained
    // it. The error itself is never surfaced: on some platforms it can quote
    // file content.
    std::fs::read_to_string(path).ok().and_then(non_empty)
}

/// Strips the live key from any string before it can reach a log, an error
/// message, or a client. `key` is the value currently resolved (if any);
/// pass `None` when no key is configured. Belt and braces: any
/// OpenRouter-shaped token is stripped too, ours or not, since a captured
/// upstream error body can carry someone else's key.
pub fn redact(text: &str, key: Option<&str>) -> String {
    let stripped = match key {
        Some(k) if !k.is_empty() => text.replace(k, "[redacted]"),
        _ => text.to_string(),
    };
    redact_sk_or_tokens(&stripped)
}

/// Replaces every `sk-or-<8+ url-safe chars>` run with `[redacted]`. Written
/// by hand instead of pulling in a regex crate for the one pattern the TS
/// original covers with `/sk-or-[A-Za-z0-9\-_]{8,}/g`.
fn redact_sk_or_tokens(text: &str) -> String {
    const PREFIX: &str = "sk-or-";
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    loop {
        match rest.find(PREFIX) {
            None => {
                out.push_str(rest);
                break;
            }
            Some(pos) => {
                out.push_str(&rest[..pos]);
                let after_prefix = &rest[pos + PREFIX.len()..];
                let token_len = after_prefix
                    .char_indices()
                    .take_while(|&(_, c)| is_token_char(c))
                    .last()
                    .map(|(i, c)| i + c.len_utf8())
                    .unwrap_or(0);
                if token_len >= 8 {
                    out.push_str("[redacted]");
                    rest = &after_prefix[token_len..];
                } else {
                    // Not a long enough token to count as a key: keep the
                    // prefix literally and keep scanning right after it, so
                    // a later occurrence of `sk-or-` further in the same
                    // string is still found.
                    out.push_str(PREFIX);
                    rest = after_prefix;
                }
            }
        }
    }
    out
}

fn is_token_char(c: char) -> bool {
    c.is_ascii_alphanumeric() || c == '-' || c == '_'
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    /// `KeySource::Env` reads process-wide env vars, and `cargo test` runs
    /// test functions on multiple threads within this one binary - without
    /// this, the two env-mutating tests below could interleave their
    /// set/remove calls and read each other's state.
    static ENV_LOCK: Mutex<()> = Mutex::new(());

    #[test]
    fn redact_strips_a_key_shaped_token_with_no_key_configured() {
        let out = redact("before sk-or-v1-abcdefgh12345 after", None);
        assert!(!out.contains("sk-or-v1-abcdefgh12345"));
        assert_eq!(out, "before [redacted] after");
    }

    #[test]
    fn redact_strips_the_configured_key_even_when_not_sk_or_shaped() {
        let out = redact(
            "Authorization: Bearer not-sk-shaped-secret",
            Some("not-sk-shaped-secret"),
        );
        assert!(!out.contains("not-sk-shaped-secret"));
    }

    #[test]
    fn redact_leaves_a_short_sk_or_prefix_alone() {
        // Fewer than 8 token chars after the prefix: not treated as a key.
        let out = redact("sk-or-ab and more text", None);
        assert_eq!(out, "sk-or-ab and more text");
    }

    #[test]
    fn key_source_env_prefers_the_file_over_the_inline_var() {
        let _guard = ENV_LOCK.lock().unwrap();
        let dir =
            std::env::temp_dir().join(format!("bullpen-rs-secrets-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let file = dir.join("key-file-precedence.txt");
        std::fs::write(&file, "from-file\n").unwrap();

        // SAFETY: this process does not read these vars from any other
        // thread concurrently with this test (no async runtime spawned yet).
        unsafe {
            std::env::set_var(KEY_FILE_VAR, &file);
            std::env::set_var(KEY_VAR, "from-inline-var");
        }
        let resolved = KeySource::Env.resolve();
        unsafe {
            std::env::remove_var(KEY_FILE_VAR);
            std::env::remove_var(KEY_VAR);
        }
        let _ = std::fs::remove_file(&file);
        let _ = std::fs::remove_dir(&dir);

        assert_eq!(resolved.as_deref(), Some("from-file"));
    }

    #[test]
    fn key_source_env_is_none_when_the_file_does_not_exist() {
        let _guard = ENV_LOCK.lock().unwrap();
        // SAFETY: same single-threaded-at-this-point reasoning as above.
        unsafe {
            std::env::set_var(KEY_FILE_VAR, "/no/such/file/for/this/test");
            std::env::remove_var(KEY_VAR);
        }
        let resolved = KeySource::Env.resolve();
        unsafe {
            std::env::remove_var(KEY_FILE_VAR);
        }
        assert_eq!(resolved, None);
    }
}
