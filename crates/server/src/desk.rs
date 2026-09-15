//! The desk: one Linux computer every bot shares - a persistent browser
//! (Chrome via DevTools) with a real route to the internet, separate from
//! the network-isolated sandbox (`crate::sandbox`) that `shell`/`read_file`
//! use.
//!
//! Ported from TypeScript `projects/bullpen-night/src/server/desk.ts` (660
//! lines). `Cdp` (`desk.ts:123-205`) is the injected DevTools boundary:
//! every fake built against it for this module's tests RECORDS the calls it
//! received, and the tests assert the call SEQUENCE, not just a return
//! value - S6-lite's own review (`DEFERRED.md` F18) found a fake that could
//! not tell "called once" from "called twice" apart, which is exactly the
//! trap a fake that only checks return values falls into here too.
//!
//! 🔴 **There is no Docker and no browser on this workstation.** Nothing in
//! this module may claim a container or a browser actually worked - see
//! "Scope cuts" at the bottom of this file for what TS `desk.ts` exports
//! this port deliberately does not carry yet, and why. The only real proof
//! that a browser exists on the other end of `Cdp` is a smoke test on
//! meridian, run by the orchestrator after the slice ships (S6-SMOKE).
//!
//! 🔴 **Page text is EXTERNAL DATA.** Whatever `read_page`/`browse` hands
//! back is exactly what a real web page said, and it reaches a bot's prompt
//! FENCED as data, never as instructions - a page that says "ignore your
//! previous instructions and..." is a finding to report, not a command this
//! file (or its caller) executes. This module's own contract stops at
//! handing back a plain `String`; nothing here parses, evaluates, or acts
//! on what a page's text says.

use crate::egress::{EgressPolicy, Resolver, decide_connect};
use async_trait::async_trait;
use reqwest::Url;
use rusqlite::OptionalExtension;
use std::collections::HashMap;
use std::time::Duration;
use store::Db;

/* ------------------------------------------------------------- config */

/// Where the desk lives: the CDP endpoint, the noVNC view, the container
/// name, and the docker socket. Read once at startup by whatever wires a
/// real `Cdp` (a scope cut here - see the bottom of this file) and the
/// terminal/status tools (also a scope cut).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DeskConfig {
    /// Chromium's DevTools endpoint, reached through socat inside the container.
    pub cdp: String,
    /// The noVNC desktop, proxied to Josh behind Bullpen's own auth.
    pub view: String,
    /// The container name, for the terminal.
    pub container: String,
    /// Passed to docker so it talks to the bullpen user's rootless daemon.
    pub docker_host: String,
}

/// Reads `DeskConfig` out of an env map, same shape `store::vms::vm_config`
/// already uses for `VmConfig` (S6-02) rather than TS's `NodeJS.ProcessEnv`
/// default-parameter idiom, which Rust has no equivalent for.
pub fn desk_config(env: &HashMap<String, String>) -> DeskConfig {
    DeskConfig {
        cdp: env
            .get("BULLPEN_DESK_CDP")
            .cloned()
            .unwrap_or_else(|| "http://127.0.0.1:9223".to_string()),
        view: env
            .get("BULLPEN_DESK_VIEW")
            .cloned()
            .unwrap_or_else(|| "http://127.0.0.1:6101".to_string()),
        container: env
            .get("BULLPEN_DESK_CONTAINER")
            .cloned()
            .unwrap_or_else(|| "bullpen-desk".to_string()),
        docker_host: env
            .get("DOCKER_HOST")
            .cloned()
            .unwrap_or_else(|| "unix:///run/user/1004/docker.sock".to_string()),
    }
}

/* ------------------------------------------------------------- the fence */

/// Why a bot may not point the browser at a URL. Matches TS's `Refusal`
/// shape (`{ ok: false, error }`) minus the `ok` tag - Rust's `Result`
/// already carries that.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Refusal {
    pub error: String,
}

/// The real DNS resolver `may_visit`/`browse` use outside a test: a thin
/// `crate::egress::Resolver` wrapper over tokio's own lookup. Port of TS's
/// `realResolve` (`desk.ts:115-118`), which is a plain function in the TS
/// file rather than a class - this is the one piece of `crate::egress`'s
/// own missing "a real resolver" that `may_visit`/`browse` need to be
/// production-usable, and TS's copy of it lives in `desk.ts`, not
/// `egress.ts`, which is why it is ported here rather than left for S6-05.
pub struct RealResolver;

#[async_trait]
impl Resolver for RealResolver {
    async fn resolve(&self, host: &str) -> Result<Vec<String>, String> {
        tokio::net::lookup_host((host, 0))
            .await
            .map(|addrs| addrs.map(|a| a.ip().to_string()).collect())
            .map_err(|e| e.to_string())
    }
}

/// Whether a bot may point the browser at `raw_url`.
///
/// 🔴 This is the SAME SSRF boundary S6-01 already built and tested
/// (`crate::egress::decide_connect`) - reused here, not re-implemented. A
/// browse target has no CONNECT port of its own the way `decide_connect`'s
/// original caller (the egress proxy) does, so this builds a one-host allow
/// list (`raw_url`'s own hostname, and only that host) and asks
/// `decide_connect` to judge it at the port the URL actually names (its
/// explicit port, or the scheme's default 80/443).
///
/// **Judgment call, not silently absorbed** (see this ticket's Results): TS
/// `mayVisit` never checked a port at all - `http://internal-tool:8080/`
/// reached Chromium's own `--host-rules` fence in TS, where this refuses it
/// outright, because `decide_connect`'s `ALLOWED_PORTS` is 80/443 only. The
/// alternative was a second, hand-rolled copy of `is_private_address` and
/// the DNS-rebinding check duplicating S6-01's already-tested one, which is
/// exactly the duplication the S6 header forbids ("a fake that cannot
/// distinguish... will certify a bug here too" - the same applies to a
/// second copy of a security check).
///
/// **Also scoped out**: TS additionally blocks by NAME
/// (`web.ts`'s `hostBlocked`/`getBlocklist`, backed by a `web.blocklist`
/// setting, plus a hardcoded `ALWAYS_BLOCKED` list: "meridian",
/// "meridian.local", "localhost", the cloud metadata hostnames). `web.ts`
/// has no Rust port yet and is not one of this ticket's owned files, so the
/// named blocklist is a scope cut. `web.ts`'s own doc says the list "was
/// never the thing protecting meridian" - the structural guard (resolve the
/// name, then refuse every private/loopback/link-local address) is what
/// `decide_connect` still gives for free, including for "meridian" and
/// "localhost" themselves, as long as they resolve to what they actually
/// answer to on the real network. A curated deny-list layered on top is a
/// real, separate hardening (Chromium's `--host-rules` gets the same
/// two-layer treatment in TS, deliberately, per `desk.ts:59-70`'s own doc)
/// and belongs in a ticket that also owns `web.rs`.
pub async fn may_visit(raw_url: &str, resolver: &dyn Resolver) -> Result<Url, Refusal> {
    let url = Url::parse(raw_url).map_err(|_| Refusal {
        error: format!("Not a URL: {raw_url}"),
    })?;

    if url.scheme() != "http" && url.scheme() != "https" {
        return Err(Refusal {
            error: format!("Only http and https are allowed, not {}:", url.scheme()),
        });
    }

    let host = url.host_str().ok_or_else(|| Refusal {
        error: format!("Not a URL: {raw_url}"),
    })?;
    let port = url
        .port_or_known_default()
        .unwrap_or(if url.scheme() == "https" { 443 } else { 80 });

    // The one-host allow list that turns decide_connect's allow-list model
    // into "any public host is fine, but the private-address and
    // DNS-rebinding checks still run" for a browse target.
    let policy = EgressPolicy {
        allow: vec![host.to_lowercase()],
    };

    let verdict = decide_connect(host, port, &policy, resolver).await;
    if verdict.ok {
        Ok(url)
    } else {
        Err(Refusal {
            error: verdict.reason,
        })
    }
}

/* ---------------------------------------------------------- talking to it */

/// One DevTools request/response. Injectable so the logic above (and
/// `window_for`/`read_page`/`browse`/`click_text`/`type_into`/`screenshot`
/// below) is testable without a real browser. Every fake built against this
/// trait for this module's tests RECORDS every call it receives, per the S6
/// header's own rule.
#[async_trait]
pub trait Cdp: Send + Sync {
    /// Opens a window and returns its target id.
    async fn create_window(&self, url: &str) -> Result<String, String>;
    /// Whether a target still exists.
    async fn has_target(&self, target_id: &str) -> bool;
    /// Runs a command against one page target.
    async fn call(
        &self,
        target_id: &str,
        method: &str,
        params: serde_json::Value,
    ) -> Result<serde_json::Value, String>;
    /// Closes a target. TS's own real implementation swallows every error
    /// here (`.catch(() => undefined)`, `desk.ts:200-202`), so the trait
    /// carries no failure at all.
    async fn close_target(&self, target_id: &str);
}

/* ------------------------------------------------------ one window per bot */

/// Self-creating, same discipline as `store::vms::ensure_vm_tables`
/// (S6-02) - never a numbered migration (S6 header). Not wired into
/// `store::Db::open`: this ticket owns only `crates/server`, and TS itself
/// calls `ensureDeskTables(db)` once at server startup (`app.ts:981`)
/// rather than from inside `openDb`, so production wiring (and every test
/// in this module) calls this directly, matching that.
pub fn ensure_desk_tables(db: &Db) -> rusqlite::Result<()> {
    db.ensure(
        "CREATE TABLE IF NOT EXISTS desk_windows (
            bot_id    TEXT PRIMARY KEY,
            target_id TEXT NOT NULL,
            opened_at TEXT NOT NULL
        )",
    )
}

/// The window this bot works in, opened if it does not exist.
///
/// Remembered in the DATABASE, not in memory - windows outlive the server
/// (restarting bullpen-rs does not close Chromium), so a process-local map
/// would lose track of them and open a new window on every restart until
/// the desktop was buried in them.
pub async fn window_for(db: &Db, cdp: &dyn Cdp, bot_id: &str) -> Result<String, String> {
    let existing: Option<String> = db
        .conn()
        .query_row(
            "SELECT target_id FROM desk_windows WHERE bot_id = ?1",
            rusqlite::params![bot_id],
            |row| row.get(0),
        )
        .optional()
        .map_err(|e| e.to_string())?;

    if let Some(target_id) = existing
        && cdp.has_target(&target_id).await
    {
        return Ok(target_id);
    }

    let target_id = cdp.create_window("about:blank").await?;
    db.conn()
        .execute(
            "INSERT INTO desk_windows (bot_id, target_id, opened_at) VALUES (?1, ?2, ?3)
             ON CONFLICT(bot_id) DO UPDATE SET target_id = excluded.target_id,
                                               opened_at = excluded.opened_at",
            rusqlite::params![
                bot_id,
                target_id,
                chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Millis, true)
            ],
        )
        .map_err(|e| e.to_string())?;

    Ok(target_id)
}

/* --------------------------------------------------------------- the tools */

/// How much page text a bot gets. Beyond this it is a document, not an
/// answer.
///
/// 🔴 Counted in CHARACTERS (Unicode scalar values via `str::chars`), never
/// bytes. A page scraped off a real site is routinely full of multi-byte
/// text - CJK, accented Latin, emoji, curly quotes - whose byte count
/// diverges hard from its character count. A byte-based cap either slices
/// mid-character (a `str` panic in Rust, since `&raw[..n]` requires `n` to
/// land on a char boundary) or truncates far short of what `MAX_PAGE_CHARS`
/// actually promises. See this ticket's Results for the literal red output
/// that proves the char/byte distinction, not just "truncation happens".
pub const MAX_PAGE_CHARS: usize = 12_000;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PageView {
    pub url: String,
    pub title: String,
    pub text: String,
    pub truncated: bool,
}

async fn evaluate(cdp: &dyn Cdp, target_id: &str, expression: &str) -> Result<String, String> {
    let reply = cdp
        .call(
            target_id,
            "Runtime.evaluate",
            serde_json::json!({
                "expression": expression,
                "returnByValue": true,
                "awaitPromise": true,
            }),
        )
        .await?;

    // A CDP evaluate can hand back any JSON value when the expression did
    // not return a primitive. A bare string passes through untouched;
    // anything else (including an explicit `null`) is rendered as its own
    // JSON text - matching TS's `JSON.stringify(value)` fallback - so a bot
    // sees the real shape of what came back instead of a value that quietly
    // reads like a real answer. A missing `result.value` (CDP's `undefined`)
    // becomes "", matching TS's own `value === undefined ? "" : ...`.
    match reply.get("result").and_then(|r| r.get("value")) {
        None => Ok(String::new()),
        Some(serde_json::Value::String(s)) => Ok(s.clone()),
        Some(other) => Ok(other.to_string()),
    }
}

/// What a bot reads off the page.
///
/// `innerText`, not the HTML: a bot asked to find something needs the words
/// a person would see, and a page's full markup is both unreadable and
/// expensive. Menus and scripts are dropped by `innerText` for free.
const READ_PAGE: &str = r#"(() => {
  const t = document.body ? document.body.innerText : "";
  return t.replace(/\n{3,}/g, "\n\n").trim();
})()"#;

pub async fn read_page(cdp: &dyn Cdp, target_id: &str) -> Result<PageView, String> {
    let url = evaluate(cdp, target_id, "location.href").await?;
    let title = evaluate(cdp, target_id, "document.title").await?;
    let raw = evaluate(cdp, target_id, READ_PAGE).await?;

    let (text, truncated) = truncate_page_text(&raw);

    Ok(PageView {
        url,
        title,
        text,
        truncated,
    })
}

/// Cuts `raw` to at most `MAX_PAGE_CHARS` CHARACTERS - see that constant's
/// own doc for why this must count `char`s, not bytes.
fn truncate_page_text(raw: &str) -> (String, bool) {
    let char_count = raw.chars().count();
    if char_count <= MAX_PAGE_CHARS {
        return (raw.to_string(), false);
    }
    let kept: String = raw.chars().take(MAX_PAGE_CHARS).collect();
    (format!("{kept}\n\n[\u{2026}page continues]"), true)
}

/// Milliseconds to let a page settle before reading it.
pub const SETTLE_MS: u64 = 3_500;

/// Navigates the bot's window to `raw_url` and reads the page back.
///
/// `settle_ms` is `None` in production (defaults to `SETTLE_MS`) and
/// `Some(0)` in every test here - the equivalent of TS's
/// `options.settleMs ?? SETTLE_MS` default parameter, which Rust has no
/// direct syntax for.
pub async fn browse(
    db: &Db,
    cdp: &dyn Cdp,
    bot_id: &str,
    raw_url: &str,
    resolver: &dyn Resolver,
    settle_ms: Option<u64>,
) -> Result<PageView, String> {
    let url = may_visit(raw_url, resolver).await.map_err(|r| r.error)?;

    let target_id = window_for(db, cdp, bot_id).await?;
    cdp.call(&target_id, "Page.enable", serde_json::json!({}))
        .await?;
    cdp.call(
        &target_id,
        "Page.navigate",
        serde_json::json!({ "url": url.to_string() }),
    )
    .await?;
    tokio::time::sleep(Duration::from_millis(settle_ms.unwrap_or(SETTLE_MS))).await;

    // 🔴 Reported from location.href AFTER the fact, inside read_page. A
    // page that redirected somewhere else is a different page from the one
    // that was asked for, and a bot that does not notice will describe the
    // wrong thing with confidence.
    read_page(cdp, &target_id).await
}

/// Clicks the first link or button whose visible text contains `text`.
pub async fn click_text(cdp: &dyn Cdp, target_id: &str, text: &str) -> Result<String, String> {
    let wanted = serde_json::to_string(&text.to_lowercase()).unwrap_or_else(|_| "\"\"".to_string());
    let script = format!(
        r#"(() => {{
    const wanted = {wanted};
    const nodes = [...document.querySelectorAll('a,button,[role="button"],input[type="submit"]')];
    const hit = nodes.find((n) => (n.innerText || n.value || "").toLowerCase().includes(wanted));
    if (!hit) return "nothing on this page says that";
    hit.click();
    return "clicked: " + (hit.innerText || hit.value || "").trim().slice(0, 80);
}})()"#
    );
    evaluate(cdp, target_id, &script).await
}

/// Types into the first field matching a CSS selector.
///
/// Both `selector` and `value` are JSON-encoded (`serde_json::to_string`)
/// before being spliced into the script, the same protection TS's
/// `JSON.stringify(...)` calls give - a bot's own text is untrusted input
/// to this generated script, exactly the concern `deskShellStdin`'s doc
/// raises about `bash -lc`, just one layer up (JS source instead of a
/// shell command).
pub async fn type_into(
    cdp: &dyn Cdp,
    target_id: &str,
    selector: &str,
    value: &str,
) -> Result<String, String> {
    let selector_json = serde_json::to_string(selector).unwrap_or_else(|_| "\"\"".to_string());
    let value_json = serde_json::to_string(value).unwrap_or_else(|_| "\"\"".to_string());
    let script = format!(
        r#"(() => {{
    const el = document.querySelector({selector_json});
    if (!el) return "no field matches that selector";
    el.focus();
    el.value = {value_json};
    el.dispatchEvent(new Event("input", {{ bubbles: true }}));
    el.dispatchEvent(new Event("change", {{ bubbles: true }}));
    return "typed into " + (el.name || el.id || el.tagName.toLowerCase());
}})()"#
    );
    evaluate(cdp, target_id, &script).await
}

pub async fn screenshot(cdp: &dyn Cdp, target_id: &str) -> Result<String, String> {
    let reply = cdp
        .call(
            target_id,
            "Page.captureScreenshot",
            serde_json::json!({ "format": "png" }),
        )
        .await?;
    Ok(reply
        .get("data")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string())
}

/* ------------------------------------------------------------- scope cuts */
//
// What TS `desk.ts` exports that this file does NOT port, and why - per the
// S6 header, no ticket in this slice may claim a container/browser/socket
// actually worked, and this ticket owns no `Cargo.toml`:
//
// - `httpCdp` (`desk.ts:149-204`), the real DevTools client. It drives raw
//   WebSockets (`new WebSocket(...)`), and no WebSocket client is a
//   dependency of this crate yet - adding one is a `Cargo.toml` change,
//   which is not a file this ticket owns ("any other file: STOP and tell
//   the orchestrator"). It also could not be exercised here regardless (no
//   browser on this workstation). `Cdp` (the trait above) is the seam a
//   future ticket implements this behind.
// - `deskShell`/`deskShellStdin`/`deskStatus` ("the terminal"/"status"
//   sections, `desk.ts:382-461`) and `deskAction`/`DeskAction`
//   ("coordinate-level input", `desk.ts:463-660`, the `xdotool` computer-use
//   surface). Each is a separable subsystem TS itself marks off with its
//   own `/* ---- */` banner, none of them named in this ticket's title
//   ("desk: mayVisit, windowFor, readPage, browse"), and together they are
//   a second ticket's worth of validation-heavy surface. `deskShell` in
//   particular duplicates `crate::sandbox`'s `docker exec` shape closely
//   enough that it deserves the same `CommandRunner`-style injection
//   treatment sandbox.rs already has, which is a design decision for
//   whichever ticket picks it up, not a five-minute addition to this one.
