//! Integration test for S8a-01: proof that `desk::ensure_desk_tables`
//! (`desk.rs:214`) is actually REACHED by `AppState::build`, not merely
//! correct in isolation.
//!
//! `tests/desk.rs`'s own tests call `ensure_desk_tables` directly before
//! touching `desk_windows` - useful for proving the module's own behaviour,
//! useless as proof of PRODUCTION wiring, since `desk.rs:210-213`'s own doc
//! comment says nothing in production called it before this ticket. This
//! file instead opens a `Db` the way the real server does (`store::Db::
//! open`, no `ensure_desk_tables` call anywhere in this file), builds the
//! real `AppState::new`, and drives the exact entry point the `browse` tool
//! dispatch uses (`tools/mod.rs:328`, `tools::browse::run_browse`) - which
//! reaches `desk.rs:259`'s `existing_window` query against `desk_windows`
//! by way of `tools/browse.rs`'s `window_for_locked`.
//!
//! **The bite.** Guard-present world: `AppState::build` calls
//! `desk::ensure_desk_tables` before handing back a state, so a bot's first
//! ever `browse` call finds the table and gets the page back. Guard-removed
//! world (comment out this ticket's added call in `crates/server/src/
//! lib.rs`'s `AppState::build`): the exact same call sequence below hits
//! sqlite's own "no such table: desk_windows" instead - a real error this
//! file never manufactures, not merely "returned false".

use async_trait::async_trait;
use server::AppState;
use server::desk::Cdp;
use server::egress::Resolver;
use server::tools::browse::run_browse;
use std::collections::VecDeque;
use std::sync::Mutex;

/// Bare-minimum `Cdp` fake - just enough for `run_browse` to sail past
/// `window_for_locked` (create the window, since a fresh bot has none) into
/// `Page.enable`/`Page.navigate`/the three `Runtime.evaluate` reads
/// `read_page` makes. This ticket is not about the desk protocol, only about
/// whether the `desk_windows` SELECT ahead of it survives a fresh db.
#[derive(Default)]
struct FakeCdp {
    call_responses: Mutex<VecDeque<Result<serde_json::Value, String>>>,
}

impl FakeCdp {
    fn new() -> Self {
        Self::default()
    }

    fn push_call_ok(&self, value: serde_json::Value) {
        self.call_responses.lock().unwrap().push_back(Ok(value));
    }
}

#[async_trait]
impl Cdp for FakeCdp {
    async fn create_window(&self, _url: &str) -> Result<String, String> {
        Ok("target-1".to_string())
    }

    async fn has_target(&self, _target_id: &str) -> bool {
        true
    }

    async fn call(
        &self,
        _target_id: &str,
        method: &str,
        _params: serde_json::Value,
    ) -> Result<serde_json::Value, String> {
        self.call_responses
            .lock()
            .unwrap()
            .pop_front()
            .unwrap_or_else(|| panic!("FakeCdp: no response scripted for {method}"))
    }

    async fn close_target(&self, _target_id: &str) {}
}

/// Every host is public - `may_visit` is not what this ticket tests, and a
/// refusal here would never reach `desk_windows` at all.
struct AllowAllResolver;

#[async_trait]
impl Resolver for AllowAllResolver {
    async fn resolve(&self, _host: &str) -> Result<Vec<String>, String> {
        Ok(vec!["8.8.8.8".to_string()])
    }
}

#[tokio::test]
async fn a_bots_first_ever_browse_survives_a_freshly_opened_db() {
    // The way the real server opens a db - `main.rs` calls exactly this.
    // No `ensure_desk_tables` call anywhere in this test: if `desk_windows`
    // exists by the time `run_browse` runs, `AppState::new` put it there.
    let db = store::Db::open(":memory:").expect("open :memory: db");
    let state = AppState::new(db);
    let db_handle = state.db_handle();

    let cdp = FakeCdp::new();
    cdp.push_call_ok(serde_json::json!({})); // Page.enable
    cdp.push_call_ok(serde_json::json!({})); // Page.navigate
    cdp.push_call_ok(serde_json::json!({"result": {"value": "https://example.com/"}}));
    cdp.push_call_ok(serde_json::json!({"result": {"value": "Example"}}));
    cdp.push_call_ok(serde_json::json!({"result": {"value": "hello world"}}));

    let result = run_browse(
        &db_handle,
        &cdp,
        &AllowAllResolver,
        "arthur",
        r#"{"url":"https://example.com/"}"#,
        Some(0),
    )
    .await;

    assert!(
        !result.to_lowercase().contains("no such table"),
        "a fresh AppState must have already created desk_windows - \
         AppState::build's call to desk::ensure_desk_tables is missing or \
         not reached. Got: {result}"
    );
    assert!(
        result.contains("hello world"),
        "expected the page text to come through once the table exists, got: {result}"
    );
}
