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
use futures::{SinkExt, StreamExt};
use reqwest::Url;
use rusqlite::OptionalExtension;
use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;
use store::Db;
use tokio_tungstenite::tungstenite::Message;

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

    /// S8b-01: whether the endpoint is answering AT ALL right now - a
    /// cheap, side-effect-free probe (no window, no target, no
    /// navigation). Deliberately NOT `has_target`: that method's own
    /// contract already swallows a transport failure into `false` to mean
    /// "no such target" (see its own doc) - indistinguishable from "nobody
    /// is home yet", which is exactly the distinction `wait_for_ready`
    /// needs. Defaults to `true` so every fake `Cdp` already written for
    /// this crate's tests (all of them modelling an ALREADY-RUNNING desk)
    /// keeps compiling and behaving unchanged; only `HttpCdp` (a real
    /// transport, checked for real) and `UnavailableCdp` (never ready -
    /// there is nothing behind it) override this.
    async fn ready(&self) -> bool {
        true
    }
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
    let existing = existing_window(db, bot_id)?;

    if let Some(target_id) = existing
        && cdp.has_target(&target_id).await
    {
        return Ok(target_id);
    }

    let target_id = cdp.create_window("about:blank").await?;
    save_window(db, bot_id, &target_id)?;

    Ok(target_id)
}

/// The sync half of `window_for`'s DB read - split out so a caller stuck
/// with an `Arc<Mutex<Db>>` (the `browse`/`read_page` TOOLS, `tools::
/// browse`) can lock, read, and drop the guard before ever awaiting a
/// `Cdp` call, instead of holding it across one the way `window_for`
/// itself does.
///
/// 🔴 `std::sync::MutexGuard` is never `Send`, so a guard held across an
/// `.await` makes the enclosing future `!Send` - exactly the constraint
/// `vm.rs`'s `start_vm_reaper` hit and documented for `hibernate_idle`.
/// `window_for` itself is fine taking `&Db` across its own awaits because
/// every caller of `window_for` directly (every test in this module, an
/// owned `Db`) already holds it for the whole call; `tools::browse`'s
/// callers hold an `Arc<Mutex<Db>>` instead, so it reuses this and
/// `save_window` around its own awaits rather than `window_for` itself.
pub(crate) fn existing_window(db: &Db, bot_id: &str) -> Result<Option<String>, String> {
    db.conn()
        .query_row(
            "SELECT target_id FROM desk_windows WHERE bot_id = ?1",
            rusqlite::params![bot_id],
            |row| row.get(0),
        )
        .optional()
        .map_err(|e| e.to_string())
}

/// The sync half of `window_for`'s DB write - see `existing_window`'s doc.
pub(crate) fn save_window(db: &Db, bot_id: &str, target_id: &str) -> Result<(), String> {
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
    Ok(())
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
    let view = read_page(cdp, &target_id).await?;

    // S8b-03 (`DEFERRED.md` F15a): `may_visit` above only validated the url
    // the bot ASKED for. Chromium follows redirects on its own between
    // `Page.navigate` and here, and nothing re-checked where it actually
    // landed - an attacker-controlled public url that 302s to
    // `169.254.169.254`/`127.0.0.1`/a LAN address would otherwise hand that
    // private page's content back with an honest, correct final url
    // attached. Re-run the SAME fence on the FINAL url, reusing `may_visit`
    // rather than a second, narrower check: `may_visit` already re-parses
    // and re-resolves (one extra DNS round trip), which is negligible next
    // to the page load/settle this function already just paid for, and a
    // hand-rolled duplicate of `decide_connect`'s private-address/
    // DNS-rebinding logic is exactly the trap this module's own header
    // already warns about for `may_visit` itself (see that function's doc,
    // "Judgment call, not silently absorbed").
    if let Err(refusal) = may_visit(&view.url, resolver).await {
        // The VM's Chromium already fetched the private page into this tab
        // before this check ever ran - the model never seeing its BODY is
        // the actual security boundary, and that is enforced by returning
        // `Err` below instead of `Ok(view)` (the caller never gets `view`,
        // let alone `view.text`). But leaving the tab parked on a private
        // page is a smaller, real exposure of its own: a later `read_page`
        // (or `click`/`type_text`, which reuse this same window) would see
        // it with no redirect involved at all. Best-effort only - a second
        // navigation failing here must not turn a successful REFUSAL into a
        // reported error, so its own result is discarded.
        let _ = cdp
            .call(
                &target_id,
                "Page.navigate",
                serde_json::json!({ "url": "about:blank" }),
            )
            .await;
        return Err(format!(
            "Refused: asked to browse {raw_url}, but the page redirected to {}, which is not allowed ({})",
            view.url, refusal.error
        ));
    }

    Ok(view)
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

/* --------------------------------------------------------- the production Cdp */

/// A `Cdp` that refuses everything with one fixed reason - used when
/// `BULLPEN_DESK` is off, mirroring `sandbox::UnavailableSandbox` exactly
/// (same shape, same reasoning: a caller never has to special-case "no
/// browser configured here" against "a browser call actually failed").
pub struct UnavailableCdp {
    reason: String,
}

impl UnavailableCdp {
    pub fn new(reason: impl Into<String>) -> Self {
        Self {
            reason: reason.into(),
        }
    }
}

#[async_trait]
impl Cdp for UnavailableCdp {
    async fn create_window(&self, _url: &str) -> Result<String, String> {
        Err(self.reason.clone())
    }
    async fn has_target(&self, _target_id: &str) -> bool {
        false
    }
    async fn call(
        &self,
        _target_id: &str,
        _method: &str,
        _params: serde_json::Value,
    ) -> Result<serde_json::Value, String> {
        Err(self.reason.clone())
    }
    async fn close_target(&self, _target_id: &str) {}
    /// Never ready - there is no browser behind this at all, only a fixed
    /// refusal reason. Never `true`, or `wait_for_ready` would report a
    /// desk that will never answer as having woken up.
    async fn ready(&self) -> bool {
        false
    }
}

/// S6-W-03 ported the HTTP half of TS `httpCdp` (`desk.ts:149-204`) for
/// real (`has_target`/`close_target`) and refused the WebSocket half
/// loudly because no WebSocket client was a dependency of this crate yet.
/// S6-W-05 lands that half: `tokio-tungstenite` (approved, see this
/// ticket's Results for the version pin and how it was confirmed against
/// `cargo tree`) drives `create_window`'s `Target.createTarget` call and
/// every `call()` - the per-target JSON-RPC round trip
/// `evaluate`/`read_page`/`browse`/`click_text`/`type_into`/`screenshot`
/// all depend on.
///
/// 🔴 Still unproven against a real browser - see this file's own header
/// and this ticket's Results. A local WebSocket listener that speaks the
/// DevTools wire format is not Chromium; the tests below prove this
/// CLIENT's behaviour (timeout, error surfacing, id matching), not that a
/// real DevTools endpoint answers correctly. The meridian smoke test
/// (S6-SMOKE) is the only thing that can prove that.
pub struct HttpCdp {
    config: DeskConfig,
    http: reqwest::Client,
    connect_timeout: Duration,
    call_timeout: Duration,
}

impl HttpCdp {
    pub fn new(config: DeskConfig) -> Self {
        Self::with_timeouts(config, SOCKET_CONNECT_TIMEOUT, CALL_TIMEOUT)
    }

    /// Same as `new`, with the connect/call timeouts overridable. TS has no
    /// equivalent - `CALL_TIMEOUT_MS`/the 10s connect timeout are both
    /// module-level constants there - but proving this ticket's bite (b)
    /// (a wedged socket cannot hang a run forever) against the REAL 30s
    /// production timeout would mean waiting out 30 real seconds on every
    /// run of the suite. `build_cdp` (the only production call site) always
    /// calls `new`, never this - production behaviour is untouched.
    pub fn with_timeouts(
        config: DeskConfig,
        connect_timeout: Duration,
        call_timeout: Duration,
    ) -> Self {
        Self {
            config,
            http: reqwest::Client::new(),
            connect_timeout,
            call_timeout,
        }
    }
}

/// A DevTools WebSocket, already connected - what `create_window`'s
/// `Target.createTarget` call and `call()`'s per-target round trip both
/// send `once()` requests over. `tokio_tungstenite::connect_async` always
/// returns this concrete stream type (TLS-wrapping enum, "plain" variant
/// used here since every CDP endpoint this file talks to is `ws://`, never
/// `wss://`) regardless of which URL was connected to, so `once` below can
/// take it directly rather than being generic over the stream type.
type WsStream =
    tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>>;

/// TS `socket()`'s own connect timeout (`desk.ts:143`: "DevTools did not
/// accept a connection" after 10s) - guards `call()`'s per-target socket
/// open. `create_window` connects straight to the `webSocketDebuggerUrl`
/// `GET /json/version` hands back with no timeout of its own, faithfully
/// matching TS `createWindow` (`desk.ts:163-171`), which also has none;
/// that connect is bounded only by the CALL_TIMEOUT_MS wrapped around the
/// `Target.createTarget` round trip that follows it.
const SOCKET_CONNECT_TIMEOUT: Duration = Duration::from_secs(10);

/// TS `CALL_TIMEOUT_MS` (`desk.ts:139`) - what stops a wedged DevTools
/// socket hanging a bot's run forever. Wraps the message-wait half of
/// `once()`, not the connect (that is `SOCKET_CONNECT_TIMEOUT`, a separate
/// timer in TS too).
const CALL_TIMEOUT: Duration = Duration::from_secs(30);

/// TS's `${config.cdp.replace(/^http/, "ws")}` (`desk.ts:141`): "https://"
/// becomes "wss://", "http://" becomes "ws://". Only used by `call()`'s
/// `socket()` helper - `create_window` never rewrites a scheme, it connects
/// to whatever `webSocketDebuggerUrl` Chromium already handed back as a
/// `ws://` URL.
fn cdp_as_ws_scheme(cdp: &str) -> String {
    match cdp.strip_prefix("http") {
        Some(rest) => format!("ws{rest}"),
        None => cdp.to_string(),
    }
}

/// One DevTools JSON-RPC reply. TS `CdpMessage` (`desk.ts:135-138`
/// interface) - `id`/`result`/`error.message` all optional the same way.
#[derive(serde::Deserialize)]
struct CdpMessage {
    id: Option<u64>,
    result: Option<serde_json::Value>,
    error: Option<CdpErrorField>,
}

#[derive(serde::Deserialize)]
struct CdpErrorField {
    message: Option<String>,
}

/// Port of TS's free function `once` (`desk.ts:217-231`): sends one
/// `{id, method, params}` request over an already-open socket and waits for
/// the reply carrying that same `id`, ignoring every other message the
/// socket delivers in between (another in-flight call's reply, a
/// keep-alive, anything) - matching TS's own `if (msg.id !== id) return;`
/// inside its `message` listener. Guarded end-to-end by `CALL_TIMEOUT`,
/// the literal thing this ticket's bite (b) proves cannot be bypassed by a
/// socket that accepts and then goes silent.
async fn once(
    ws: &mut WsStream,
    id: u64,
    method: &str,
    params: serde_json::Value,
    call_timeout: Duration,
) -> Result<serde_json::Value, String> {
    let request = serde_json::json!({ "id": id, "method": method, "params": params }).to_string();

    let round_trip = async {
        ws.send(Message::text(request))
            .await
            .map_err(|e| e.to_string())?;

        loop {
            match ws.next().await {
                Some(Ok(Message::Text(text))) => {
                    let Ok(msg) = serde_json::from_str::<CdpMessage>(&text) else {
                        // Not JSON, or not shaped like a CdpMessage - not a
                        // reply to anything this call sent. Keep waiting,
                        // same as TS's own JSON.parse inside the listener
                        // (a parse failure there would throw synchronously
                        // out of the listener and never reach the `id`
                        // check; skipping it here is the closer-to-intent
                        // behaviour and keeps this loop from dying on a
                        // stray non-JSON frame).
                        continue;
                    };
                    if msg.id != Some(id) {
                        continue;
                    }
                    return match msg.error {
                        Some(e) => Err(e.message.unwrap_or_else(|| method.to_string())),
                        None => Ok(msg.result.unwrap_or(serde_json::Value::Null)),
                    };
                }
                // Non-text frames (ping/pong/binary/close) carry no reply -
                // keep waiting for the one that does.
                Some(Ok(_)) => continue,
                Some(Err(e)) => return Err(e.to_string()),
                None => return Err("DevTools closed the connection".to_string()),
            }
        }
    };

    match tokio::time::timeout(call_timeout, round_trip).await {
        Ok(result) => result,
        Err(_) => Err(format!("{method} timed out")),
    }
}

#[async_trait]
impl Cdp for HttpCdp {
    /// TS `createWindow` (`desk.ts:161-180`): `PUT /json/new` makes a tab,
    /// but a WINDOW needs `Target.createTarget`, which is browser-scoped,
    /// so this fetches the browser's own WebSocket endpoint from `GET
    /// /json/version` first.
    async fn create_window(&self, url: &str) -> Result<String, String> {
        #[derive(serde::Deserialize)]
        struct VersionInfo {
            #[serde(rename = "webSocketDebuggerUrl")]
            web_socket_debugger_url: String,
        }

        let version: VersionInfo = self
            .http
            .get(format!("{}/json/version", self.config.cdp))
            .send()
            .await
            .map_err(|e| e.to_string())?
            .json()
            .await
            .map_err(|e| e.to_string())?;

        let (mut ws, _response) =
            tokio_tungstenite::connect_async(&version.web_socket_debugger_url)
                .await
                .map_err(|_| "DevTools refused".to_string())?;

        let reply = once(
            &mut ws,
            1,
            "Target.createTarget",
            serde_json::json!({ "url": url, "newWindow": true }),
            self.call_timeout,
        )
        .await;
        let _ = ws.close(None).await;

        let reply = reply?;
        match reply.get("targetId").and_then(|v| v.as_str()) {
            Some(id) => Ok(id.to_string()),
            None => Err("Chromium did not return a window".to_string()),
        }
    }

    /// The one real surface `httpCdp` needed no socket for: `GET
    /// /json/list`, matching TS `hasTarget` (`desk.ts:182-189`) exactly,
    /// including swallowing a transport or parse error into `false` rather
    /// than propagating it - TS's own `catch { return false }`.
    async fn has_target(&self, target_id: &str) -> bool {
        #[derive(serde::Deserialize)]
        struct ListedTarget {
            id: String,
        }
        let Ok(resp) = self
            .http
            .get(format!("{}/json/list", self.config.cdp))
            .send()
            .await
        else {
            return false;
        };
        let Ok(list) = resp.json::<Vec<ListedTarget>>().await else {
            return false;
        };
        list.iter().any(|t| t.id == target_id)
    }

    /// TS `call` (`desk.ts:191-198`): a fresh WebSocket per call to the
    /// PER-TARGET endpoint (`/devtools/page/{targetId}`), closed straight
    /// after - see TS's own doc (`desk.ts:145-148`) for why a long-lived
    /// connection was not worth the reconnect/bookkeeping cost.
    async fn call(
        &self,
        target_id: &str,
        method: &str,
        params: serde_json::Value,
    ) -> Result<serde_json::Value, String> {
        let ws_url = format!(
            "{}/devtools/page/{}",
            cdp_as_ws_scheme(&self.config.cdp),
            target_id
        );

        let mut ws = match tokio::time::timeout(
            self.connect_timeout,
            tokio_tungstenite::connect_async(&ws_url),
        )
        .await
        {
            Ok(Ok((ws, _response))) => ws,
            Ok(Err(_)) => return Err("DevTools refused".to_string()),
            Err(_) => return Err("DevTools did not accept a connection".to_string()),
        };

        let result = once(&mut ws, 1, method, params, self.call_timeout).await;
        let _ = ws.close(None).await;
        result
    }

    /// `GET /json/close/{id}`, errors swallowed - TS's own `.catch(() =>
    /// undefined)` (`desk.ts:200-202`).
    async fn close_target(&self, target_id: &str) {
        let _ = self
            .http
            .get(format!("{}/json/close/{}", self.config.cdp, target_id))
            .send()
            .await;
    }

    /// S8b-01: the SAME `GET /json/version` `create_window` itself makes
    /// (`desk.ts` has no equivalent - see this file's S8b-01 doc below),
    /// used here only as a liveness probe: "did anything answer", not "was
    /// the body a valid `VersionInfo`". A container whose Chromium has not
    /// finished booting refuses the TCP connection outright (`send()`
    /// returns `Err`); once the port is accepting, this returns `true`
    /// even before the JSON is well-formed, matching what `wait_for_ready`
    /// actually needs to know.
    async fn ready(&self) -> bool {
        self.http
            .get(format!("{}/json/version", self.config.cdp))
            .send()
            .await
            .is_ok()
    }
}

/* ---------------------------------------------------- S8b-01: wake polling */
//
// The defect (reproduced on meridian 2026-09-17, not reasoned about): the
// reaper (`start_vm_reaper`, `b010c30`) hibernates an idle bot's VM with
// `docker stop`. That bot's NEXT `browse` correctly wakes it
// (`ensure_vm_in`'s `!state.running` branch issues `docker start`), but the
// container coming up is not the same thing as Chromium inside it
// finishing its own boot - `docker start` returning does not mean
// `/json/version` will answer yet. `cdp_for_bot` used to hand back a
// freshly built `HttpCdp` the instant `docker start` succeeded, so the
// bot's very next tool call (`browse`/`read_page`/`click`/`type_text`) hit
// a bare connection error a model cannot act on
// ("error sending request for url (http://127.0.0.1:9300/json/version)").
// A retry minutes later worked - not because minutes were needed, but
// because nobody asked again sooner.
//
// **Decision 1 - where the wait lives: here (`cdp_for_bot`), not
// `vm::ensure_vm_in`.** `ensure_vm_in` is shared by `routes/vms.rs`'s
// route handlers (VM status/refresh), which have no reason to block a
// request on Chromium's own readiness - they only care about the DOCKER
// CONTAINER's state, which `docker start`/`docker inspect` already answer
// correctly and fast. `cdp_for_bot` is the one function only the four
// desk TOOLS call (`tools/mod.rs`'s `browse`/`read_page`/`click`/
// `type_text` dispatch arms) - the path that actually needs a real,
// answering CDP endpoint before doing anything with it. Blocking
// `ensure_vm_in` would also block every OTHER caller for a browser that
// caller never asked about.
//
// **Decision 2 - the budget.** `runs.rs`'s `MAX_STEPS = 24` and a run's own
// wall clock are real: a tool call cannot block for minutes without
// burning a large share of one bot turn on a single wait. `WAKE_POLL_BUDGET`
// (6s, polled every `WAKE_POLL_INTERVAL` = 500ms) is UNMEASURED against a
// real meridian wake - this workstation has no Docker and no route to
// meridian's `bullpen-desk`/VM containers (this file's own header), so the
// actual "docker start done" to "Chromium answering /json/version" gap on
// meridian could not be timed for this ticket. 6 seconds is chosen because
// the container itself is already warm (no image pull, this is a
// `docker start` of an existing container, not `docker run` of a fresh
// one) - a LinuxServer webtop's Xorg + Chromium coming up from an already
// materialised filesystem is a low-single-digit-seconds operation on
// ordinary hardware, and 6s is comfortably inside that while still being a
// small fraction of one of 24 steps. If meridian's real wake turns out to
// need longer, this constant is the one place to raise it - the poll
// itself is unaffected by any particular value.
//
// **Decision 3 - what the model sees on timeout.** Never the raw transport
// error (useless: "did not answer" reads like a broken machine, not "wait
// a moment") and never anything that could read as the browse having
// succeeded. `cdp_for_bot` on a poll timeout returns an `UnavailableCdp`
// carrying an actionable sentence in the same voice as this file's other
// `UnavailableCdp` reasons ("Per-bot machines are off here. There is
// nothing to browse with.") - short, states the real cause, tells the
// model what to do next, and cannot be mistaken for a page having loaded.

/// S8b-01: how long `cdp_for_bot` waits for a just-woken machine's
/// Chromium to start answering before giving up - see this section's
/// header doc (Decision 2) for the reasoning and the explicit "unmeasured"
/// admission.
pub const WAKE_POLL_BUDGET: Duration = Duration::from_secs(6);

/// S8b-01: how often `wait_for_ready` re-checks within `WAKE_POLL_BUDGET`.
pub const WAKE_POLL_INTERVAL: Duration = Duration::from_millis(500);

/// S8b-01: polls `cdp.ready()` until it answers `true` or `budget` elapses.
/// Generic over `&dyn Cdp` (not `HttpCdp` directly) so a scripted fake
/// exercises the IDENTICAL loop `cdp_for_bot`'s real caller runs - the only
/// production caller is `cdp_for_bot`, right after resolving this bot's own
/// machine (see this section's header doc, Decision 1). Returns `false`,
/// never panics or hangs past `budget`, when nothing ever answers -
/// covered by this ticket's timeout bite.
pub async fn wait_for_ready(cdp: &dyn Cdp, budget: Duration, interval: Duration) -> bool {
    let deadline = std::time::Instant::now() + budget;
    loop {
        if cdp.ready().await {
            return true;
        }
        if std::time::Instant::now() >= deadline {
            return false;
        }
        tokio::time::sleep(interval).await;
    }
}

/// S8b-01: the sentence `cdp_for_bot` hands back when `wait_for_ready`
/// times out - see this section's header doc, Decision 3, for why this
/// exact wording and not the raw transport error.
const WAKING_UP_REASON: &str = "This bot's shared computer just woke up and is still starting its browser. \
Try again in a few seconds.";

/// The `Cdp` a bot's `browse`/`read_page` call should use - S8a-02.
///
/// Replaces `build_cdp()` (S6-W-03..S8a-01, deleted by this ticket): that
/// function read `BULLPEN_DESK` and handed EVERY bot the same `HttpCdp`
/// pointed at ONE shared desk, regardless of whether per-bot machines
/// (`vm::desk_for_in`/`vm::vm_desk`, ported in S6-06b) were ever wired up to
/// a caller - they had zero production callers until this ticket. This is
/// that caller: resolved per bot, at call time, exactly like `build_cdp`'s
/// own env-gated pattern (`sandbox::default_sandbox`'s `BULLPEN_SANDBOX`
/// gate, cited in `build_cdp`'s old doc) - never threaded in ahead of time,
/// because a bot's own VM can change state (started, hibernated by the
/// reaper, `b010c30`) between one tool call and the next.
///
/// **Decision 1 (vm_enabled = false):** refuses with `UnavailableCdp`
/// BEFORE ever touching `vm::desk_for_in` - never a shared-desk
/// `DeskConfig`. `desk_for`'s own doc (still true for ITS callers,
/// `tests/vm.rs`) frames "VMs off -> shared desk" as intentional, and
/// `desk_for`'s existing test (`desk_for_returns_fallback_when_vms_disabled`)
/// locks that reading in for `desk_for` itself - but `desk_for` takes its
/// `fallback` as a value the CALLER supplies, and this is a fresh caller,
/// free to supply "there is nothing to browse with" instead of a working
/// alternate desk. `UnavailableCdp`'s reason string is exactly what a model
/// can already act on (say so, try something else) - the same shape
/// `sandbox::UnavailableSandbox` already gives `shell`/`sandbox_read`.
///
/// **Decision 2 (no VM row / `stopped`):** handled entirely inside
/// `vm::ensure_vm_in` (via `desk_for_in`), unchanged by this function -
/// `stopped` is the reaper-hibernated common case (`b010c30`), and
/// `ensure_vm_in`'s `!state.running` branch issues `docker start` and hands
/// back the SAME `cdp_port` the row always had, so a woken bot's very next
/// `browse` reaches its own machine again, not a new one. The one dead end
/// (`DeskResolution::Unavailable`) is every VM slot taken - genuinely no
/// machine of this bot's own exists yet - and this refuses with that
/// reason rather than inventing a stand-in desk.
///
/// 🔴 **S8b-01 note** (a LATER ticket than the three decisions above, do
/// not confuse the numbering): `docker start` succeeding is not the same
/// as Chromium inside the container answering yet - see this file's
/// "wake polling" section for the defect this closes and its own three
/// decisions.
///
/// **Decision 3 (`build_cdp`/`BULLPEN_DESK`):** deleted. Keeping it as a
/// fallback would mean a misconfiguration (this function reached with a
/// bad `vm_docker`/`vm_config`, or a future caller that forgets to check
/// `vm_enabled`) could silently route a bot back onto ONE shared desk -
/// exactly the bug S8a-02 exists to remove. `desk_config`/`HttpCdp` are
/// NOT deleted (`tests/desk.rs` still exercises `desk_config` directly, and
/// `HttpCdp` is what THIS function builds from a resolved `DeskConfig`);
/// only the env-gated "pick a shared desk" resolution is gone.
pub async fn cdp_for_bot(
    db: &Arc<Mutex<Db>>,
    vm_docker: Arc<dyn crate::vm::DockerRun>,
    vm_config: &store::vms::VmConfig,
    vm_enabled: bool,
    bot_id: &str,
) -> Arc<dyn Cdp> {
    if !vm_enabled {
        return Arc::new(UnavailableCdp::new(
            "Per-bot machines are off here. There is nothing to browse with.",
        ));
    }

    // Cosmetic only (container TITLE env var, wake/boot log detail) - never
    // read for routing, so a bot missing from `bots` (should not happen in
    // production; cheap to fall back to `bot_id` rather than fail the call
    // over a display string) still gets its own machine.
    let bot_name = {
        let guard = db.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        store::get_bot(&guard, bot_id)
            .ok()
            .flatten()
            .map(|b| b.name)
    }
    .unwrap_or_else(|| bot_id.to_string());

    match crate::vm::desk_for_in(db, vm_docker, bot_id, &bot_name, vm_config).await {
        Ok(crate::vm::DeskResolution::Own(config)) => {
            let cdp = HttpCdp::new(config);
            // S8b-01: see this file's "wake polling" section above for the
            // full defect and all three decisions. `ensure_vm_in` (inside
            // `desk_for_in`, just above) only proves the CONTAINER is
            // running - this is what proves Chromium INSIDE it is actually
            // answering before a tool is handed a `Cdp` it will try to use
            // immediately.
            if wait_for_ready(&cdp, WAKE_POLL_BUDGET, WAKE_POLL_INTERVAL).await {
                Arc::new(cdp)
            } else {
                Arc::new(UnavailableCdp::new(WAKING_UP_REASON))
            }
        }
        Ok(crate::vm::DeskResolution::Unavailable(reason)) => Arc::new(UnavailableCdp::new(reason)),
        Err(err) => Arc::new(UnavailableCdp::new(format!(
            "Could not read this bot's machine: {err}"
        ))),
    }
}

/// S8b-02: the `DeskConfig` a bot's `desk_shell` (and the four tools that
/// route through it - `snap_desk`, `record_desk`, `desk_shell_stdin`,
/// `desk_act`, none landed yet) should act on: the calling bot's OWN
/// machine. Same routing as `cdp_for_bot` above (Decisions 1-2 in that
/// function's own doc: refuse BEFORE `vm::desk_for_in` when VMs are off,
/// let `desk_for_in`/`ensure_vm_in` handle "no row yet"/"stopped"/"every
/// slot taken" on their own) - deliberately NOT reusing `cdp_for_bot`
/// itself, because `desk_shell` execs into the container directly and never
/// touches Chromium, so there is no `Cdp` to build and no
/// `wait_for_ready`/`WAKE_POLL_BUDGET` poll to run; a command on a
/// container whose `docker start` just returned needs no DevTools endpoint
/// to be answering first.
///
/// Returns `Err(reason)` rather than an `UnavailableCdp`-shaped type -
/// `desk_shell` has no `Cdp` trait object to wrap a refusal in, so its
/// caller (`tools::desk_shell::run_desk_shell`'s call site in
/// `tools/mod.rs`) folds this `Err` into the SAME "The shared computer did
/// not answer: {reason}" sentence `browse`/`read_page`/`click`/`type_text`
/// already produce for the identical class of refusal (`vm_enabled=false`,
/// every slot taken) - one voice for "there is nothing to act on" across
/// every desk tool, not a second wording invented for this one.
pub async fn desk_config_for_bot(
    db: &Arc<Mutex<Db>>,
    vm_docker: Arc<dyn crate::vm::DockerRun>,
    vm_config: &store::vms::VmConfig,
    vm_enabled: bool,
    bot_id: &str,
) -> Result<DeskConfig, String> {
    if !vm_enabled {
        return Err(
            "Per-bot machines are off here. There is nothing to run a command on.".to_string(),
        );
    }

    // Cosmetic only - see `cdp_for_bot`'s identical comment on this same
    // lookup for why a missing bot row falls back to `bot_id` rather than
    // failing the call.
    let bot_name = {
        let guard = db.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        store::get_bot(&guard, bot_id)
            .ok()
            .flatten()
            .map(|b| b.name)
    }
    .unwrap_or_else(|| bot_id.to_string());

    match crate::vm::desk_for_in(db, vm_docker, bot_id, &bot_name, vm_config).await {
        Ok(crate::vm::DeskResolution::Own(config)) => Ok(config),
        Ok(crate::vm::DeskResolution::Unavailable(reason)) => Err(reason),
        Err(err) => Err(format!("Could not read this bot's machine: {err}")),
    }
}

/* ------------------------------------------------------------- scope cuts */
//
// What TS `desk.ts` exports that this file does NOT port, and why - per the
// S6 header, no ticket in this slice may claim a container/browser/socket
// actually worked:
//
// - (S6-W-03/S6-W-05, CLOSED) `httpCdp` (`desk.ts:149-204`) is now fully
//   ported: S6-W-03 shipped the HTTP half for real (`has_target`,
//   `close_target`); S6-W-05 added `tokio-tungstenite` (approved, see that
//   ticket's Results) and shipped the WebSocket half (`create_window`,
//   `call`). Still unproven against a real browser - a local test listener
//   is not Chromium - see this file's header and S6-W-05's Results; the
//   meridian smoke test (S6-SMOKE) is the only thing that can prove that.
// - (S8b-02, CLOSED) `deskShell` ("the terminal" section, `desk.ts:382-440`)
//   is now ported - `desk_config_for_bot` above resolves the routing,
//   `tools::desk_shell::run_desk_shell` runs the command. Still scoped out:
//   `deskShellStdin`/`deskStatus` (the rest of "the terminal"/"status",
//   `desk.ts:390-461`) and `deskAction`/`DeskAction` ("coordinate-level
//   input", `desk.ts:463-660`, the `xdotool` computer-use surface) - each a
//   separable subsystem TS itself marks off with its own `/* ---- */`
//   banner, none of them this ticket's title, and together a second
//   ticket's worth of validation-heavy surface.
