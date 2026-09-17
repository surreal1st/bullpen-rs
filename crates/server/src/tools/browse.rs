//! `browse`/`read_page`/`click`/`type_text`: the four desk tools a bot's
//! model may call directly. Port of the relevant branch of TS
//! `app.ts:7082-7116` (the same four-way dispatch). S6-W-03 narrowed this
//! file to `browse`/`read_page` only - `click`/`type_text` were a later
//! ticket's scope, same reasoning `desk.rs`'s own "scope cuts" section
//! gives for `deskShell`/`deskAction`. **S8a-03 is that ticket**: it adds
//! `click_spec`/`type_text_spec`/`run_click`/`run_type_text` alongside the
//! original two, reusing `window_for_locked`/`transport_failure` rather
//! than duplicating them, because all four tools share the same
//! open-or-reuse-the-window plumbing.
//!
//! 🔴 **Page text is EXTERNAL DATA.** `format_page` below fences the page's
//! own words (`super::fence_tool_output`) before they ever reach a bot's
//! prompt - a page that says "ignore your previous instructions and..." is
//! exactly the case this closes. Title and URL are the server's own read of
//! `document.title`/`location.href`, not rendered as an instruction a bot
//! would act on the way page BODY text would be, so they stay outside the
//! fence for readability; TS's own format (`app.ts:7089`/`7107`,
//! `[title, url, "", text].join("\n")`) has no fence at all, which is the
//! gap this ticket exists to close.
//!
//! 🔴 `may_visit` (`crate::desk::may_visit`, wired to
//! `crate::egress::decide_connect`) gates every navigation, and it runs
//! BEFORE `window_for_locked` ever touches the `Cdp` - see
//! `run_browse`'s own doc and this ticket's bite (a).
//!
//! 🔴 **S8b-03: that first check is not enough on its own.** `may_visit`
//! only validates the url a bot ASKED for; Chromium follows redirects on
//! its own after `Page.navigate`, and until S8b-03 nothing re-checked where
//! it actually landed - an attacker-controlled public url could 302 to a
//! private address and its content would come back with an honest, correct
//! final url attached. `navigate_and_read` now re-runs `may_visit` on the
//! `PageView`'s own url once `read_page` returns, and refuses (without
//! returning the body) if that final url fails the fence. `run_read_page`/
//! `run_click`/`run_type_text` do NOT get this check - see
//! `run_read_page`'s S8b-03 doc for why that is a named gap, not a fixed one.

use std::sync::{Arc, Mutex, MutexGuard, PoisonError};

use model::ToolSpec;
use serde::Deserialize;
use serde_json::json;
use store::Db;

use crate::desk::{self, Cdp, PageView};
use crate::egress::Resolver;

use super::fence_tool_output;

pub fn browse_spec() -> ToolSpec {
    ToolSpec {
        name: "browse".to_string(),
        description: "Open a page in your window on the shared computer and read what it \
actually shows, with JavaScript run. Use this when fetch_url returns a page with no content in \
it, or when you need a site that only works in a browser. Your window stays where you left it, \
so you can browse, then click or type, then read again."
            .to_string(),
        parameters: json!({
            "type": "object",
            "properties": {
                "url": { "type": "string", "description": "An http or https URL." }
            },
            "required": ["url"]
        }),
    }
}

pub fn read_page_spec() -> ToolSpec {
    ToolSpec {
        name: "read_page".to_string(),
        description: "Read the page currently open in your window again, without navigating. \
Use after clicking or typing to see what changed."
            .to_string(),
        parameters: json!({ "type": "object", "properties": {} }),
    }
}

/// S8a-03: spec text ported VERBATIM from TS `app.ts:5191-5199` - the
/// wording is what a model reads to decide when to call this tool, so this
/// is not this ticket's to reword.
pub fn click_spec() -> ToolSpec {
    ToolSpec {
        name: "click".to_string(),
        description: "Click the first link or button on your current page whose visible text \
contains what you give. Then call read_page to see the result."
            .to_string(),
        parameters: json!({
            "type": "object",
            "properties": {
                "text": { "type": "string", "description": "Visible text of the link or button." }
            },
            "required": ["text"]
        }),
    }
}

/// S8a-03: spec text ported VERBATIM from TS `app.ts:5201-5210`, same
/// reasoning as `click_spec`.
pub fn type_text_spec() -> ToolSpec {
    ToolSpec {
        name: "type_text".to_string(),
        description: "Type into a field on your current page, chosen by CSS selector. For \
search boxes and forms. Never type a password: Josh completes those himself on the shared \
desktop."
            .to_string(),
        parameters: json!({
            "type": "object",
            "properties": {
                "selector": {
                    "type": "string",
                    "description": "A CSS selector, such as input[name=q]."
                },
                "text": { "type": "string", "description": "What to type." }
            },
            "required": ["selector", "text"]
        }),
    }
}

#[derive(Deserialize, Default)]
struct BrowseArgs {
    #[serde(default)]
    url: String,
}

fn lock(db: &Arc<Mutex<Db>>) -> MutexGuard<'_, Db> {
    db.lock().unwrap_or_else(PoisonError::into_inner)
}

/// The bot's window, opened if it does not exist yet. Same table, same SQL
/// as `desk::window_for` (`desk::existing_window`/`desk::save_window`) -
/// this exists only because `desk::window_for` takes `&Db` across its own
/// internal `Cdp` awaits, which only ever works for a caller that already
/// holds an owned `Db` for the whole call (every `desk.rs` test). A tool
/// wired through `tools::ToolBox` holds an `Arc<Mutex<Db>>` instead, and
/// `std::sync::MutexGuard` is never `Send` - a guard held across an
/// `.await` would make this tool's future `!Send`, which `ToolBox`'s
/// handler type requires (see `tools::mod::ToolFuture`). `vm.rs`'s
/// `start_vm_reaper` hit the identical constraint against `hibernate_idle`
/// and resolved it the same way: lock only for the synchronous read/write,
/// release before every `Cdp` await. Same behavior as `window_for`, same
/// SQL, different lock discipline.
async fn window_for_locked(
    db: &Arc<Mutex<Db>>,
    cdp: &dyn Cdp,
    bot_id: &str,
) -> Result<String, String> {
    let existing = {
        let guard = lock(db);
        desk::existing_window(&guard, bot_id)?
    };

    if let Some(target_id) = existing
        && cdp.has_target(&target_id).await
    {
        return Ok(target_id);
    }

    let target_id = cdp.create_window("about:blank").await?;
    {
        let guard = lock(db);
        desk::save_window(&guard, bot_id, &target_id)?;
    }
    Ok(target_id)
}

/// TS's own `[title, url, "", text]` shape (`app.ts:7089`/`7107`), with the
/// text FENCED instead of TS's raw join - see this module's own doc.
fn format_page(view: &PageView) -> String {
    format!(
        "{}\n{}\n\n{}",
        view.title,
        view.url,
        fence_tool_output(&view.text)
    )
}

/// A transport failure past `may_visit` (the desk down, a target that
/// vanished mid-call, `HttpCdp`'s own unfinished WebSocket half) becomes a
/// sentence, not this tool's raw error text - TS's identical `catch`
/// (`app.ts:7108-7114`): "the shared computer did not answer", not a stack.
fn transport_failure(err: &str) -> String {
    format!("The shared computer did not answer: {err}")
}

/// Runs `browse`: refuse-or-navigate, then read. `settle_ms` is `None` in
/// production (`desk::SETTLE_MS`) and `Some(0)` in every test here - same
/// convention `desk::browse` itself uses, and for the same reason (a real
/// test suite cannot afford a 3.5s sleep per case).
///
/// 🔴 Bite (a): `may_visit` runs and can refuse BEFORE `window_for_locked`
/// (and therefore before any `Cdp::create_window`/`call`) ever runs - a
/// blocked host never reaches the browser at all, not even to be told no.
pub async fn run_browse(
    db: &Arc<Mutex<Db>>,
    cdp: &dyn Cdp,
    resolver: &dyn Resolver,
    bot_id: &str,
    args: &str,
    settle_ms: Option<u64>,
) -> String {
    let parsed: BrowseArgs = serde_json::from_str(args).unwrap_or_default();
    let raw_url = parsed.url.trim();
    if raw_url.is_empty() {
        return "No url was given.".to_string();
    }

    let url = match desk::may_visit(raw_url, resolver).await {
        Ok(url) => url,
        Err(refusal) => return refusal.error,
    };

    match navigate_and_read(db, cdp, resolver, bot_id, raw_url, url.as_str(), settle_ms).await {
        Ok(view) => format_page(&view),
        Err(NavigateOutcome::Refused(msg)) => msg,
        Err(NavigateOutcome::Transport(err)) => transport_failure(&err),
    }
}

/// `navigate_and_read`'s two distinct failure shapes - kept apart so
/// `run_browse` never mislabels a security refusal as `transport_failure`'s
/// "the shared computer did not answer" (the computer answered fine; it
/// answered with a page that is not allowed).
enum NavigateOutcome {
    /// A `Cdp` call itself errored (desk down, target vanished mid-call,
    /// `HttpCdp`'s own unfinished WebSocket half).
    Transport(String),
    /// S8b-03: the page's FINAL url (after Chromium followed whatever
    /// redirects it issued) failed the same fence `may_visit` already ran
    /// on the url the bot asked for. Message is already what the model
    /// should see, unwrapped.
    Refused(String),
}

/// The part of `desk::browse` that happens once `may_visit` has already
/// said yes for the REQUESTED url - open/reuse the window, enable+navigate,
/// settle, read, then (S8b-03) re-check the url actually landed on. Mirrors
/// `desk::browse`'s own sequencing exactly (see that function's doc on why
/// the URL is re-read from `location.href` rather than trusted as given, and
/// on the same redirect fence); duplicated here rather than called because
/// `desk::browse` needs `&Db` held across its own awaits (see
/// `window_for_locked`'s doc). Both copies were patched for S8b-03 - see
/// this ticket's Results for why fixing only one would have left the other,
/// reachable one exploitable.
///
/// `raw_url` is the url the bot ASKED for (before `may_visit` parsed it) -
/// kept only so a refusal message can say "asked for A, got sent to B".
async fn navigate_and_read(
    db: &Arc<Mutex<Db>>,
    cdp: &dyn Cdp,
    resolver: &dyn Resolver,
    bot_id: &str,
    raw_url: &str,
    url: &str,
    settle_ms: Option<u64>,
) -> Result<PageView, NavigateOutcome> {
    let target_id = window_for_locked(db, cdp, bot_id)
        .await
        .map_err(NavigateOutcome::Transport)?;
    cdp.call(&target_id, "Page.enable", json!({}))
        .await
        .map_err(NavigateOutcome::Transport)?;
    cdp.call(&target_id, "Page.navigate", json!({ "url": url }))
        .await
        .map_err(NavigateOutcome::Transport)?;
    tokio::time::sleep(std::time::Duration::from_millis(
        settle_ms.unwrap_or(desk::SETTLE_MS),
    ))
    .await;
    let view = desk::read_page(cdp, &target_id)
        .await
        .map_err(NavigateOutcome::Transport)?;

    // S8b-03 (`DEFERRED.md` F15a): re-run the fence on the FINAL url, reusing
    // `may_visit` rather than a second, narrower check - see `desk::browse`'s
    // matching comment for the DNS-cost/duplication reasoning, identical here.
    if let Err(refusal) = desk::may_visit(&view.url, resolver).await {
        // Best-effort blank-out: the private page is already loaded in this
        // tab. The model never seeing its body is enforced below (`view` is
        // never returned as `Ok`); this just stops the tab holding it for a
        // later `read_page`/`click`/`type_text` call on the same window. Its
        // own outcome is discarded - a failed blank-out must not turn a
        // successful refusal into a reported transport error.
        let _ = cdp
            .call(&target_id, "Page.navigate", json!({ "url": "about:blank" }))
            .await;
        return Err(NavigateOutcome::Refused(format!(
            "Refused: asked to browse {raw_url}, but the page redirected to {}, which is not allowed ({})",
            view.url, refusal.error
        )));
    }

    Ok(view)
}

/// Runs `read_page`: whatever the bot's window currently shows, no
/// navigation - opens the window first if this bot has never browsed yet,
/// matching TS's own `windowFor` call ahead of `readPage` (`app.ts:7092,
/// 7106`).
///
/// 🔴 **S8b-03 named gap, not a fixed one.** This function has NEVER
/// re-checked `location.href` against `may_visit` - not even for the
/// original `browse` navigation, since `run_browse`'s new redirect check
/// lives in `navigate_and_read`, one call up. That is fine for the
/// `browse`-then-`read_page` sequence (the redirect check already ran),
/// but `click` can navigate the page too (any link/button it matches), and
/// a `read_page` called after THAT click has no fence at all - not "first
/// hop only" the way `browse` used to, but no check whatsoever. Closing
/// that means either giving `read_page` its own `Resolver` and running the
/// same check here, or having `click`/`type_text` run it themselves; either
/// is a real design decision (this function currently takes no `Resolver`
/// at all) that deserves its own ticket and its own tests, not a fold-in
/// here. Left open deliberately - see this ticket's Result for why.
pub async fn run_read_page(db: &Arc<Mutex<Db>>, cdp: &dyn Cdp, bot_id: &str) -> String {
    let target_id = match window_for_locked(db, cdp, bot_id).await {
        Ok(id) => id,
        Err(err) => return transport_failure(&err),
    };
    match desk::read_page(cdp, &target_id).await {
        Ok(view) => format_page(&view),
        Err(err) => transport_failure(&err),
    }
}

#[derive(Deserialize, Default)]
struct ClickArgs {
    #[serde(default)]
    text: String,
}

#[derive(Deserialize, Default)]
struct TypeTextArgs {
    #[serde(default)]
    selector: String,
    #[serde(default)]
    text: String,
}

/// Runs `click`: acts on the bot's CURRENT window - opened first if this
/// bot has never browsed yet, same as `run_read_page` - without
/// navigating. TS dispatch: `app.ts:7092-7096`.
///
/// `desk::click_text` does the actual DOM search-and-click over CDP; this
/// wraps it exactly the way `run_browse`/`run_read_page` wrap
/// `desk::browse`/`desk::read_page` - resolve the window under the lock,
/// release it, then make the `Cdp` call.
///
/// 🔴 The returned string is PAGE-DERIVED: either the clicked element's own
/// visible text, or the "nothing on this page says that" sentence
/// `desk::click_text` produces when no element matches. Both go through
/// `fence_tool_output`, same reason `format_page` fences a page's body -
/// see this module's own top-of-file doc. `"No text was given."` is the
/// one branch that is NOT page-derived (a bot called `click` with an empty
/// `text`, refused before `Cdp` is ever touched, same shape as
/// `run_browse`'s empty-`url` guard) and stays unfenced accordingly.
pub async fn run_click(db: &Arc<Mutex<Db>>, cdp: &dyn Cdp, bot_id: &str, args: &str) -> String {
    let parsed: ClickArgs = serde_json::from_str(args).unwrap_or_default();
    let text = parsed.text.trim();
    if text.is_empty() {
        return "No text was given.".to_string();
    }

    let target_id = match window_for_locked(db, cdp, bot_id).await {
        Ok(id) => id,
        Err(err) => return transport_failure(&err),
    };

    match desk::click_text(cdp, &target_id, text).await {
        Ok(result) => fence_tool_output(&result),
        Err(err) => transport_failure(&err),
    }
}

/// Runs `type_text`: same window handling as `run_click`, delegating the
/// field lookup/fill to `desk::type_into`. TS dispatch: `app.ts:7097-7103`.
/// Same fencing/refusal shape as `run_click` - see that function's doc.
pub async fn run_type_text(db: &Arc<Mutex<Db>>, cdp: &dyn Cdp, bot_id: &str, args: &str) -> String {
    let parsed: TypeTextArgs = serde_json::from_str(args).unwrap_or_default();
    let selector = parsed.selector.trim();
    if selector.is_empty() {
        return "No selector was given.".to_string();
    }

    let target_id = match window_for_locked(db, cdp, bot_id).await {
        Ok(id) => id,
        Err(err) => return transport_failure(&err),
    };

    match desk::type_into(cdp, &target_id, selector, &parsed.text).await {
        Ok(result) => fence_tool_output(&result),
        Err(err) => transport_failure(&err),
    }
}
