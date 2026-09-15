//! `browse`/`read_page`: the two desk tools a bot's model may call
//! directly. Port of the relevant branch of TS `app.ts:7082-7116` (the
//! `browse`/`read_page`/`click`/`type_text` dispatch), narrowed to the two
//! tools S6-W-03 owns - `click`/`type_text` are a later ticket's scope,
//! same reasoning `desk.rs`'s own "scope cuts" section gives for
//! `deskShell`/`deskAction`.
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

    match navigate_and_read(db, cdp, bot_id, url.as_str(), settle_ms).await {
        Ok(view) => format_page(&view),
        Err(err) => transport_failure(&err),
    }
}

/// The part of `desk::browse` that happens once `may_visit` has already
/// said yes - open/reuse the window, enable+navigate, settle, read. Mirrors
/// `desk::browse`'s own sequencing exactly (see that function's doc on why
/// the URL is re-read from `location.href` rather than trusted as given);
/// duplicated here rather than called because `desk::browse` needs `&Db`
/// held across its own awaits (see `window_for_locked`'s doc).
async fn navigate_and_read(
    db: &Arc<Mutex<Db>>,
    cdp: &dyn Cdp,
    bot_id: &str,
    url: &str,
    settle_ms: Option<u64>,
) -> Result<PageView, String> {
    let target_id = window_for_locked(db, cdp, bot_id).await?;
    cdp.call(&target_id, "Page.enable", json!({})).await?;
    cdp.call(&target_id, "Page.navigate", json!({ "url": url }))
        .await?;
    tokio::time::sleep(std::time::Duration::from_millis(
        settle_ms.unwrap_or(desk::SETTLE_MS),
    ))
    .await;
    desk::read_page(cdp, &target_id).await
}

/// Runs `read_page`: whatever the bot's window currently shows, no
/// navigation - opens the window first if this bot has never browsed yet,
/// matching TS's own `windowFor` call ahead of `readPage` (`app.ts:7092,
/// 7106`).
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
