//! Integration tests for the desk module.
//! Ported from TypeScript `test/desk.test.ts` where a case exists; several
//! cases here have no TS counterpart because TS never had to prove a
//! browser-recording fake's CALL SEQUENCE the way this file does (S6
//! header, S6-lite `DEFERRED.md` F18).
//!
//! Two required bites (both proven red in this ticket's Results, restored
//! by re-editing `crates/server/src/desk.rs`, never `git checkout`):
//! (a) `read_page` truncating by CHARACTER count, not byte count -
//!     `read_page_truncates_by_character_count_not_byte_count` below.
//! (b) `may_visit` actually refusing (not just claiming to refuse) a host
//!     that resolves to a private address -
//!     `may_visit_refuses_dns_rebinding_and_actually_resolved` below.

use async_trait::async_trait;
use server::desk::{
    Cdp, MAX_PAGE_CHARS, browse, click_text, desk_config, ensure_desk_tables, may_visit, read_page,
    screenshot, type_into, window_for,
};
use server::egress::Resolver;
use std::collections::{HashMap, VecDeque};
use std::sync::Mutex;

/* ------------------------------------------------------------- fakes */

/// Records every call it receives (create_window/has_target/call/close_target)
/// into one ordered `sequence`, plus a detailed `call_log` for `call()`
/// specifically - so a test can assert the exact SEQUENCE of DevTools
/// interactions, not just that some call eventually returned the right
/// value. S6-lite's own review (F18) found a fake that popped responses in
/// an order hiding a second call; this fake would have panicked instead
/// (an exhausted queue panics with the call that had nothing scripted for
/// it), same discipline as `crate::sandbox::FakeRunner`.
#[derive(Default)]
struct FakeCdp {
    sequence: Mutex<Vec<String>>,
    create_window_responses: Mutex<VecDeque<Result<String, String>>>,
    has_target_responses: Mutex<VecDeque<bool>>,
    call_responses: Mutex<VecDeque<Result<serde_json::Value, String>>>,
    call_log: Mutex<Vec<(String, String, serde_json::Value)>>,
}

impl FakeCdp {
    fn new() -> Self {
        Self::default()
    }

    fn push_create_window(&self, target_id: impl Into<String>) {
        self.create_window_responses
            .lock()
            .unwrap()
            .push_back(Ok(target_id.into()));
    }

    fn push_has_target(&self, exists: bool) {
        self.has_target_responses.lock().unwrap().push_back(exists);
    }

    fn push_call_ok(&self, value: serde_json::Value) {
        self.call_responses.lock().unwrap().push_back(Ok(value));
    }

    /// Queues the three responses `read_page` consumes in order:
    /// `location.href`, `document.title`, then the page's `innerText`.
    fn push_read_page(&self, url: &str, title: &str, text: &str) {
        self.push_call_ok(serde_json::json!({"result": {"value": url}}));
        self.push_call_ok(serde_json::json!({"result": {"value": title}}));
        self.push_call_ok(serde_json::json!({"result": {"value": text}}));
    }

    fn sequence(&self) -> Vec<String> {
        self.sequence.lock().unwrap().clone()
    }

    fn call_log(&self) -> Vec<(String, String, serde_json::Value)> {
        self.call_log.lock().unwrap().clone()
    }
}

#[async_trait]
impl Cdp for FakeCdp {
    async fn create_window(&self, url: &str) -> Result<String, String> {
        self.sequence
            .lock()
            .unwrap()
            .push(format!("create_window({url})"));
        self.create_window_responses
            .lock()
            .unwrap()
            .pop_front()
            .unwrap_or_else(|| panic!("FakeCdp: no create_window response scripted for {url}"))
    }

    async fn has_target(&self, target_id: &str) -> bool {
        self.sequence
            .lock()
            .unwrap()
            .push(format!("has_target({target_id})"));
        self.has_target_responses
            .lock()
            .unwrap()
            .pop_front()
            .unwrap_or_else(|| panic!("FakeCdp: no has_target response scripted for {target_id}"))
    }

    async fn call(
        &self,
        target_id: &str,
        method: &str,
        params: serde_json::Value,
    ) -> Result<serde_json::Value, String> {
        self.sequence
            .lock()
            .unwrap()
            .push(format!("call({target_id},{method})"));
        self.call_log
            .lock()
            .unwrap()
            .push((target_id.to_string(), method.to_string(), params));
        self.call_responses
            .lock()
            .unwrap()
            .pop_front()
            .unwrap_or_else(|| panic!("FakeCdp: no call response scripted for {method}"))
    }

    async fn close_target(&self, target_id: &str) {
        self.sequence
            .lock()
            .unwrap()
            .push(format!("close_target({target_id})"));
    }
}

/// A resolver that records every host it was asked to resolve - the same
/// "prove the mechanism, not just the outcome" shape `tests/egress.rs`'s
/// own `RecordingResolver` uses (S6-01's bite (b)), duplicated here rather
/// than shared because this ticket owns no file `egress.rs`'s test helpers
/// could live in without touching `tests/common/mod.rs`, which this ticket
/// does not own either.
struct RecordingResolver {
    calls: Mutex<Vec<String>>,
    addrs: Vec<String>,
}

impl RecordingResolver {
    fn new(addrs: Vec<String>) -> Self {
        Self {
            calls: Mutex::new(Vec::new()),
            addrs,
        }
    }

    fn call_count(&self) -> usize {
        self.calls.lock().unwrap().len()
    }
}

#[async_trait]
impl Resolver for RecordingResolver {
    async fn resolve(&self, host: &str) -> Result<Vec<String>, String> {
        self.calls.lock().unwrap().push(host.to_string());
        Ok(self.addrs.clone())
    }
}

fn db() -> store::Db {
    store::Db::open(":memory:").expect("open in-memory db")
}

/* ------------------------------------------------------------- desk_config */

#[test]
fn desk_config_has_sensible_defaults() {
    let env = HashMap::new();
    let cfg = desk_config(&env);
    assert_eq!(cfg.cdp, "http://127.0.0.1:9223");
    assert_eq!(cfg.view, "http://127.0.0.1:6101");
    assert_eq!(cfg.container, "bullpen-desk");
    assert_eq!(cfg.docker_host, "unix:///run/user/1004/docker.sock");
}

#[test]
fn desk_config_reads_overrides() {
    let mut env = HashMap::new();
    env.insert(
        "BULLPEN_DESK_CDP".to_string(),
        "http://desk:9223".to_string(),
    );
    env.insert("BULLPEN_DESK_CONTAINER".to_string(), "my-desk".to_string());
    let cfg = desk_config(&env);
    assert_eq!(cfg.cdp, "http://desk:9223");
    assert_eq!(cfg.container, "my-desk");
    // Untouched keys keep their default.
    assert_eq!(cfg.view, "http://127.0.0.1:6101");
}

/* ------------------------------------------------------------- may_visit */

#[tokio::test]
async fn may_visit_allows_a_public_host_and_actually_resolved() {
    let resolver = RecordingResolver::new(vec!["8.8.8.8".to_string()]);
    let result = may_visit("https://example.com/page", &resolver).await;
    assert!(result.is_ok(), "expected ok, got {result:?}");
    assert_eq!(result.unwrap().as_str(), "https://example.com/page");
    assert_eq!(resolver.call_count(), 1);
}

#[tokio::test]
async fn may_visit_refuses_a_non_http_scheme() {
    let resolver = RecordingResolver::new(vec!["8.8.8.8".to_string()]);
    let result = may_visit("ftp://example.com/", &resolver).await;
    let refusal = result.expect_err("ftp must be refused");
    assert!(refusal.error.contains("http"));
    // Refused before ever reaching the resolver.
    assert_eq!(resolver.call_count(), 0);
}

#[tokio::test]
async fn may_visit_refuses_an_invalid_url() {
    let resolver = RecordingResolver::new(vec!["8.8.8.8".to_string()]);
    let result = may_visit("not a url", &resolver).await;
    let refusal = result.expect_err("garbage input must be refused");
    assert!(refusal.error.contains("Not a URL"));
    assert_eq!(resolver.call_count(), 0);
}

#[tokio::test]
async fn may_visit_refuses_a_literal_private_address_without_resolving() {
    let resolver = RecordingResolver::new(vec!["8.8.8.8".to_string()]);
    let result = may_visit("http://127.0.0.1/admin", &resolver).await;
    assert!(result.is_err());
    // decide_connect refuses a private literal before ever calling resolve.
    assert_eq!(resolver.call_count(), 0);
}

/// 🔴 BITE (b) target: proves `may_visit` actually acts on
/// `decide_connect`'s verdict for a DNS-rebinding host (a name that LOOKS
/// public but resolves to a private address), and that the resolver was
/// genuinely called - a refusal that never resolved looks identical to a
/// correct refusal from the outside (S6-lite F18 / this ticket's own
/// instructions).
#[tokio::test]
async fn may_visit_refuses_dns_rebinding_and_actually_resolved() {
    let resolver = RecordingResolver::new(vec!["127.0.0.1".to_string()]);
    let result = may_visit("http://looks-public.example.com/", &resolver).await;

    let refusal = result.expect_err("a host resolving to 127.0.0.1 must be refused");
    assert!(
        refusal.error.contains("127.0.0.1") || refusal.error.contains("private"),
        "refusal should mention the private address: {}",
        refusal.error
    );
    assert_eq!(
        resolver.call_count(),
        1,
        "resolver must actually have been called exactly once"
    );
}

/* ------------------------------------------------------------- window_for */

#[tokio::test]
async fn window_for_opens_a_new_window_when_none_exists() {
    let db = db();
    ensure_desk_tables(&db).unwrap();
    let cdp = FakeCdp::new();
    cdp.push_create_window("target-new");

    let target_id = window_for(&db, &cdp, "arthur").await.unwrap();
    assert_eq!(target_id, "target-new");
    assert_eq!(
        cdp.sequence(),
        vec!["create_window(about:blank)".to_string()]
    );

    // Persisted: a second call with the target still present reuses it.
    cdp.push_has_target(true);
    let again = window_for(&db, &cdp, "arthur").await.unwrap();
    assert_eq!(again, "target-new");
}

#[tokio::test]
async fn window_for_reuses_an_existing_target() {
    let db = db();
    ensure_desk_tables(&db).unwrap();
    let cdp = FakeCdp::new();
    cdp.push_create_window("target-1");
    let first = window_for(&db, &cdp, "trinity").await.unwrap();

    cdp.push_has_target(true);
    let second = window_for(&db, &cdp, "trinity").await.unwrap();

    assert_eq!(first, second);
    assert_eq!(
        cdp.sequence(),
        vec![
            "create_window(about:blank)".to_string(),
            "has_target(target-1)".to_string(),
        ]
    );
}

#[tokio::test]
async fn window_for_reopens_when_the_old_target_is_gone() {
    let db = db();
    ensure_desk_tables(&db).unwrap();
    let cdp = FakeCdp::new();
    cdp.push_create_window("target-old");
    let first = window_for(&db, &cdp, "grok").await.unwrap();
    assert_eq!(first, "target-old");

    // The remembered window closed underneath us (Chromium restarted, tab
    // closed by hand, etc.) - has_target says no, so a fresh one opens.
    cdp.push_has_target(false);
    cdp.push_create_window("target-fresh");
    let second = window_for(&db, &cdp, "grok").await.unwrap();
    assert_eq!(second, "target-fresh");

    // The database now remembers the NEW target, not the old one.
    cdp.push_has_target(true);
    let third = window_for(&db, &cdp, "grok").await.unwrap();
    assert_eq!(third, "target-fresh");
}

#[tokio::test]
async fn window_for_gives_each_bot_its_own_window() {
    let db = db();
    ensure_desk_tables(&db).unwrap();
    let cdp = FakeCdp::new();
    cdp.push_create_window("target-arthur");
    cdp.push_create_window("target-trinity");

    let arthur = window_for(&db, &cdp, "arthur").await.unwrap();
    let trinity = window_for(&db, &cdp, "trinity").await.unwrap();
    assert_ne!(arthur, trinity);
}

#[test]
fn ensure_desk_tables_is_idempotent() {
    let db = db();
    ensure_desk_tables(&db).unwrap();
    // Calling it again (a second server start against the same db) must
    // not error, same as `CREATE TABLE IF NOT EXISTS` promises.
    ensure_desk_tables(&db).unwrap();
}

/* ------------------------------------------------------------- read_page */

#[tokio::test]
async fn read_page_returns_url_title_and_text_untruncated() {
    let cdp = FakeCdp::new();
    cdp.push_read_page("https://example.com/", "Example Domain", "hello world");

    let view = read_page(&cdp, "target-1").await.unwrap();
    assert_eq!(view.url, "https://example.com/");
    assert_eq!(view.title, "Example Domain");
    assert_eq!(view.text, "hello world");
    assert!(!view.truncated);

    // All three reads go through Runtime.evaluate, on the same target, in
    // the fixed order read_page asks for them.
    assert_eq!(
        cdp.sequence(),
        vec![
            "call(target-1,Runtime.evaluate)".to_string(),
            "call(target-1,Runtime.evaluate)".to_string(),
            "call(target-1,Runtime.evaluate)".to_string(),
        ]
    );
}

/// 🔴 BITE (a) target. 20 000 repetitions of '€' (U+20AC, 3 UTF-8 bytes
/// each): 20 000 CHARACTERS, 60 000 BYTES. Correct (char-based) truncation
/// keeps exactly MAX_PAGE_CHARS (12 000) characters (36 000 bytes). A
/// byte-based bug instead stops after 12 000 BYTES = 4 000 characters - a
/// completely different, silently wrong answer. An ASCII fixture cannot
/// distinguish the two (for ASCII, chars and bytes are the same number),
/// which is exactly why this fixture is multi-byte.
#[tokio::test]
async fn read_page_truncates_by_character_count_not_byte_count() {
    let cdp = FakeCdp::new();
    let raw = "\u{20AC}".repeat(20_000);
    cdp.push_read_page("https://example.com/long", "Long page", &raw);

    let view = read_page(&cdp, "target-1").await.unwrap();

    assert!(
        view.truncated,
        "20 000 characters must be reported truncated"
    );
    let euro_count = view.text.chars().filter(|c| *c == '\u{20AC}').count();
    assert_eq!(
        euro_count, MAX_PAGE_CHARS,
        "must keep exactly MAX_PAGE_CHARS CHARACTERS, not bytes (got {euro_count})"
    );
    assert!(view.text.ends_with("[\u{2026}page continues]"));
}

#[tokio::test]
async fn read_page_leaves_short_multibyte_text_alone() {
    let cdp = FakeCdp::new();
    // 100 euro signs: well under MAX_PAGE_CHARS in characters, but a
    // byte-based check (100 * 3 = 300 bytes) would ALSO stay under 12 000 -
    // this case exists so the truncated-short-text path is exercised with
    // multi-byte text too, not just the over-the-limit path above.
    let raw = "\u{20AC}".repeat(100);
    cdp.push_read_page("https://example.com/short", "Short page", &raw);

    let view = read_page(&cdp, "target-1").await.unwrap();
    assert!(!view.truncated);
    assert_eq!(view.text.chars().count(), 100);
}

/* ------------------------------------------------------------- browse */

#[tokio::test]
async fn browse_navigates_and_reads_the_page() {
    let db = db();
    ensure_desk_tables(&db).unwrap();
    let cdp = FakeCdp::new();
    let resolver = RecordingResolver::new(vec!["8.8.8.8".to_string()]);

    cdp.push_create_window("target-1");
    cdp.push_call_ok(serde_json::json!({})); // Page.enable
    cdp.push_call_ok(serde_json::json!({})); // Page.navigate
    cdp.push_read_page("https://example.com/", "Example", "the content");

    let view = browse(
        &db,
        &cdp,
        "arthur",
        "https://example.com/",
        &resolver,
        Some(0),
    )
    .await
    .unwrap();

    assert_eq!(view.text, "the content");
    assert_eq!(
        cdp.sequence(),
        vec![
            "create_window(about:blank)".to_string(),
            "call(target-1,Page.enable)".to_string(),
            "call(target-1,Page.navigate)".to_string(),
            "call(target-1,Runtime.evaluate)".to_string(),
            "call(target-1,Runtime.evaluate)".to_string(),
            "call(target-1,Runtime.evaluate)".to_string(),
        ],
        "browse must enable, then navigate, then read - in that order"
    );

    // Page.navigate must have been given the actual URL, not a placeholder.
    let log = cdp.call_log();
    let navigate = log
        .iter()
        .find(|(_, method, _)| method == "Page.navigate")
        .expect("Page.navigate was called");
    assert_eq!(
        navigate.2.get("url").and_then(|v| v.as_str()),
        Some("https://example.com/")
    );
}

#[tokio::test]
async fn browse_refuses_without_ever_touching_cdp() {
    let db = db();
    ensure_desk_tables(&db).unwrap();
    let cdp = FakeCdp::new();
    let resolver = RecordingResolver::new(vec!["127.0.0.1".to_string()]);

    let result = browse(
        &db,
        &cdp,
        "arthur",
        "http://looks-public.example.com/",
        &resolver,
        Some(0),
    )
    .await;

    assert!(result.is_err());
    // The refusal must happen before window_for/Page.enable/Page.navigate -
    // a fake that recorded ANY call here would mean browse tried to drive
    // the browser toward a host it had just refused.
    assert!(
        cdp.sequence().is_empty(),
        "browse must not touch Cdp at all once may_visit refuses: {:?}",
        cdp.sequence()
    );
    // The resolver was still genuinely consulted - the refusal is real, not
    // a check that got skipped for an unrelated reason.
    assert_eq!(resolver.call_count(), 1);
}

/* ------------------------------------------------------------- clickText / typeInto / screenshot */

#[tokio::test]
async fn click_text_evaluates_with_the_lowercased_search_text() {
    let cdp = FakeCdp::new();
    cdp.push_call_ok(serde_json::json!({"result": {"value": "clicked: Sign In"}}));

    let result = click_text(&cdp, "target-1", "Sign In").await.unwrap();
    assert_eq!(result, "clicked: Sign In");

    let log = cdp.call_log();
    assert_eq!(log.len(), 1);
    let expression = log[0].2.get("expression").and_then(|v| v.as_str()).unwrap();
    assert!(
        expression.contains("\"sign in\""),
        "expression was: {expression}"
    );
}

#[tokio::test]
async fn type_into_json_encodes_selector_and_value() {
    let cdp = FakeCdp::new();
    cdp.push_call_ok(serde_json::json!({"result": {"value": "typed into email"}}));

    // A value containing a quote and a backslash - if this were spliced in
    // raw instead of JSON-encoded, it would break out of the generated
    // script's string literal.
    let tricky = r#"a"b\c"#;
    let result = type_into(&cdp, "target-1", "#email", tricky).await.unwrap();
    assert_eq!(result, "typed into email");

    let log = cdp.call_log();
    let expression = log[0].2.get("expression").and_then(|v| v.as_str()).unwrap();
    assert!(expression.contains(&serde_json::to_string(tricky).unwrap()));
    assert!(expression.contains("#email"));
}

#[tokio::test]
async fn screenshot_returns_the_data_field() {
    let cdp = FakeCdp::new();
    cdp.push_call_ok(serde_json::json!({"data": "base64pngbytes"}));

    let result = screenshot(&cdp, "target-1").await.unwrap();
    assert_eq!(result, "base64pngbytes");

    let log = cdp.call_log();
    assert_eq!(log[0].1, "Page.captureScreenshot");
}

#[tokio::test]
async fn screenshot_returns_empty_string_when_data_missing() {
    let cdp = FakeCdp::new();
    cdp.push_call_ok(serde_json::json!({}));

    let result = screenshot(&cdp, "target-1").await.unwrap();
    assert_eq!(result, "");
}
