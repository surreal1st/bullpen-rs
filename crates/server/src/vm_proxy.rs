//! The viewer: which bot a `/api/bots/<id>/vm/view/...` request reaches,
//! what headers a proxied response may carry, and the raw socket relay
//! that opens the desktop's own WebSocket through Bullpen's auth.
//!
//! Ported from TypeScript `src/server/vm.ts:592-741` (`viewerTarget`,
//! `proxyResponseHeaders`, `buildUpgradeRequest`, `attachVmProxy`), by
//! export name, per S6-06b (`.scratch/bullpen-rs/tickets/S6-tickets.md`).
//!
//! 🔴 `viewer_target` decides WHICH bot's screen a request reaches. Treated
//! here as an authorization boundary, not a string parse: TS's
//! `decodeURIComponent(botId)` runs AFTER the regex match with no
//! revalidation, so a percent-encoded `..%2Fother-bot` decodes to a bot id
//! that contains a path separator - harmless against `get_vm`'s exact
//! primary-key lookup today, but the value stops meaning "one bot" the
//! moment anything downstream ever does a prefix/startsWith check instead
//! of an exact one. This port adds that revalidation (see `viewer_target`'s
//! own doc for the guard and S6-06b's bite (a)).
//!
//! **There is no Docker and no browser on this workstation.** Nothing here
//! may claim a container, a screen capture or a socket actually worked.
//! `attach_vm_proxy` takes a raw `AsyncRead + AsyncWrite` socket rather
//! than an axum `Request` on purpose - see its own doc for why, and for
//! what that buys in testability without a real browser.
//!
//! 🔴 S6-F-02 (`.scratch/bullpen-rs/tickets/S6-F-tickets.md`, from S6-R F7):
//! the bot-id half of a viewer target is guarded (see `viewer_target`'s own
//! doc) but the `rest`/`query` half was not - TS normalises the whole
//! target through `new URL(req.url, "http://127.0.0.1").pathname`
//! (`vm.ts:697`) before either half is used; this port never did, so a raw
//! `\r`/`\n` in the query rode straight into `build_upgrade_request`'s
//! concatenated request line and injected a header into the container's
//! handshake. `target_has_raw_control_break` closes that at both places a
//! target can enter this module: `parse_request_head` (the real server's
//! only ingestion point, via `read_request_head`) and `attach_vm_proxy`
//! itself (for any caller that hands it a `raw_target` directly). Unreached
//! in production today - no VM route is mounted (S6-R's header) - but it
//! had to be fixed before one is, not after.

use axum::http::HeaderMap;
use std::pin::Pin;
use std::sync::{Arc, Mutex, PoisonError};
use std::task::{Context, Poll};
use store::Db;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt, ReadBuf};

/* -------------------------------------------------------------- the viewer */

/// The bot a viewer request is for, and the path inside its desktop.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ViewerTarget {
    pub bot_id: String,
    pub rest: String,
}

/// Where a viewer request is really going.
///
/// Port of TS `viewerTarget` (`vm.ts:608`). `/api/bots/<id>/vm/view/<rest>`
/// proxies to the container's web desktop at `/<rest>`. Returns `None` for
/// anything that is not a viewer path.
///
/// The trailing slash on the mount point is load-bearing and not cosmetic:
/// selkies builds its own WebSocket URL as
/// `location.pathname.substring(0, lastIndexOf("/") + 1) + "websockets"`,
/// so the page must be served from a path ENDING in "/" or the socket
/// opens one level up, against a path that proxies nothing, and the
/// desktop shows a black screen that never connects.
///
/// 🔴 Guard (S6-06b bite (a), added beyond the literal TS body): the
/// decoded bot id is rejected if it is empty, contains `/`, or is exactly
/// `..`. Every caller of this function (`get_vm`, `attach_vm_proxy`) uses
/// the returned `bot_id` for an EXACT lookup - a value shaped like a path
/// segment sequence is not "one bot id" any more the instant it can
/// re-introduce a separator, and treating it as one anyway is how a
/// traversal or a neighbour's id sneaks past whatever calls this.
pub fn viewer_target(pathname: &str) -> Option<ViewerTarget> {
    let re = regex::Regex::new(r"^(?:/[^/]+)??/api/bots/([^/]+)/vm/view(?:/(.*))?$")
        .expect("viewer_target regex is a fixed, valid pattern");
    let caps = re.captures(pathname)?;
    let raw_bot_id = caps.get(1)?.as_str();
    let bot_id = percent_decode(raw_bot_id);

    if bot_id.is_empty() || bot_id.contains('/') || bot_id == ".." {
        return None;
    }

    let rest = caps
        .get(2)
        .map(|m| m.as_str().to_string())
        .unwrap_or_default();
    Some(ViewerTarget { bot_id, rest })
}

/// Minimal `%XX` decoder. `decodeURIComponent`'s Rust equivalent for the
/// one piece this file needs (a path segment); anything not a valid `%XX`
/// escape is passed through literally, same as `decodeURIComponent` would
/// for a percent that is not followed by two hex digits (it throws in JS,
/// which `viewerTarget`'s caller never catches - so a malformed escape
/// falling back to a literal `%` here is at least as safe: it fails the
/// slash/`..`/empty guard above and is rejected instead of crashing).
fn percent_decode(input: &str) -> String {
    let bytes = input.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' && i + 3 <= bytes.len() {
            let hex = std::str::from_utf8(&bytes[i + 1..i + 3]).ok();
            if let Some(byte) = hex.and_then(|h| u8::from_str_radix(h, 16).ok()) {
                out.push(byte);
                i += 3;
                continue;
            }
        }
        out.push(bytes[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// Headers a proxy must not pass on, per RFC 9110 - they describe ONE hop.
///
/// 🔴 `content-encoding` is deliberately NOT on this list, unlike TS
/// `vm.ts:617-628`'s equivalent. `content-encoding` is an END-TO-END header
/// in RFC 9110, not a hop-by-hop one - TS could get away with stripping it
/// only because TS fetches upstream with `fetch()`/undici, which transparently
/// decompresses the body before TS ever sees it, so by the time TS strips the
/// header it no longer described the (already-plain) bytes. This crate's
/// `reqwest` client is built `default-features = false` with no `gzip`
/// feature (`Cargo.toml`) - it never decompresses, so the body a browser
/// receives here is still gzip-compressed. Stripping `content-encoding` too
/// left the compressed bytes with no header saying so, so a browser (every
/// browser sends `accept-encoding: gzip`) rendered them as plain text
/// (S6-W-06). Do NOT add reqwest's `gzip` feature to "fix" this instead -
/// this proxy relays bytes for a desktop stream; decompressing only to
/// re-send costs CPU on every frame for nothing. `content-length` stays
/// stripped: axum recomputes it from the body it actually sends.
const HOP_BY_HOP: &[&str] = &[
    "connection",
    "keep-alive",
    "proxy-authenticate",
    "proxy-authorization",
    "te",
    "trailer",
    "transfer-encoding",
    "upgrade",
    "content-length",
];

/// Port of TS `proxyResponseHeaders` (`vm.ts:630`). Strips the hop-by-hop
/// set above and forces `cache-control: no-store` - the desktop is Josh's
/// browser sessions on a screen; nothing embeds it but Bullpen itself, and
/// nothing caches it anywhere.
///
/// 🔴 S6-06b bite (b) lives here: a header on `HOP_BY_HOP` that this does
/// not strip is a proxy passing one hop's framing (`transfer-encoding`,
/// `connection`, ...) straight to a client that has no business seeing it.
pub fn proxy_response_headers(source: &HeaderMap) -> HeaderMap {
    let mut headers = HeaderMap::new();
    for (name, value) in source.iter() {
        if !HOP_BY_HOP.contains(&name.as_str()) {
            headers.insert(name.clone(), value.clone());
        }
    }
    headers.insert(
        axum::http::header::CACHE_CONTROL,
        axum::http::HeaderValue::from_static("no-store"),
    );
    headers
}

/// The raw HTTP request that opens a WebSocket on the container.
///
/// Port of TS `buildUpgradeRequest` (`vm.ts:655`). Built as a string
/// rather than through a request-builder because an upgrade is relayed
/// byte for byte in both directions after the handshake; once the socket
/// is a duplex pipe there is nothing for an HTTP client to do here.
///
/// `Host` is rewritten to the container's own port: selkies sits behind
/// nginx inside the container, and nginx picks a server block by `Host`;
/// handing it Bullpen's public hostname is how the upgrade comes back as a
/// 404 page instead of a 101. `Cookie`/`Authorization` are dropped: the
/// cookie is Bullpen's session and has no meaning inside the container -
/// sending it on would put Josh's session token in the container's logs.
pub fn build_upgrade_request(path: &str, headers: &HeaderMap, web_port: i32) -> String {
    let mut lines = vec![
        format!("GET {path} HTTP/1.1"),
        format!("Host: 127.0.0.1:{web_port}"),
    ];
    for (name, value) in headers.iter() {
        let lower = name.as_str();
        if lower == "host" || lower == "cookie" || lower == "authorization" {
            continue;
        }
        if let Ok(text) = value.to_str() {
            lines.push(format!("{}: {}", name.as_str(), text));
        }
    }
    format!("{}\r\n\r\n", lines.join("\r\n"))
}

/// What `attach_vm_proxy` did with one upgrade attempt - the observable a
/// test asserts, since nothing here can prove a real socket conversation
/// happened.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProxyOutcome {
    /// Not a `/api/bots/<id>/vm/view` path - left untouched for whatever
    /// else handles upgrades.
    NotAViewerPath,
    /// No valid session; refused before any upstream connection was made.
    Unauthorized,
    /// The path names a bot with no VM row.
    UnknownBot,
    /// A VM row exists, but its web desktop would not accept a connection.
    UpstreamUnreachable,
    /// The raw target carried a `\r` or `\n` that TS's `new URL(...).pathname`
    /// (`vm.ts:697`) would have percent-encoded away before this ever saw it
    /// (S6-R F7). Refused before any upstream connection was made - a bare
    /// line break here would land inside `build_upgrade_request`'s
    /// concatenated request line and inject a header into the container's
    /// handshake.
    MalformedTarget,
    /// The handshake was sent upstream and both directions are now piped.
    Proxying,
}

/// WebSocket upgrades for the in-app viewer.
///
/// Port of TS `attachVmProxy` (`vm.ts:686`). The session is checked HERE,
/// in full, with the same `session_valid` the `/api/*` gate uses: an
/// upgrade bypasses ordinary route middleware entirely, so an
/// unauthenticated socket would be a live remote desktop on the open
/// internet carrying Josh's signed-in browser.
///
/// 🔴 Takes a raw `client: S` (`AsyncRead + AsyncWrite`) rather than an
/// axum `Request`, on purpose. TS attaches to the raw Node `http.Server`'s
/// `"upgrade"` event specifically because `@hono/node-server` turns
/// requests into typed `Request` objects and an upgrade has no body to
/// become one - Node hands you the duplex BEFORE any response is written.
/// axum/hyper's typed `Response` model has no equivalent hook: returning a
/// 101 `Response` from a route handler makes hyper write ITS OWN response
/// bytes before handing back the raw duplex, which would require this
/// crate to independently compute a correct `Sec-WebSocket-Accept`
/// (RFC 6455) rather than simply relaying the upstream's own handshake
/// bytes the way TS does - a real feature, not a port, and out of this
/// ticket's scope (see S6-06b's `## Results`). Accepting the raw socket
/// directly sidesteps that: whoever accepts the TCP connection and detects
/// an `Upgrade: websocket` request line (the wiring - main.rs/routes, not
/// owned by this ticket) hands the still-unwritten-to duplex in here, and
/// this pipes it to the upstream exactly like the TS original does. It is
/// also what makes the auth/target-resolution paths testable with
/// `tokio::io::duplex` instead of a real socket.
pub async fn attach_vm_proxy<S>(
    mut client: S,
    raw_target: &str,
    request_headers: &HeaderMap,
    db: &Db,
    require_auth: bool,
) -> ProxyOutcome
where
    S: AsyncRead + AsyncWrite + Unpin + Send + 'static,
{
    // S6-R F7: a raw `\r`/`\n` anywhere in the target - not just after the
    // `?`, see `target_has_raw_control_break`'s own doc - must never reach
    // `build_upgrade_request`'s concatenated request line. Checked on the
    // whole, unsplit target so a break hiding in what would become `rest`
    // is caught the same way as one in the query.
    if target_has_raw_control_break(raw_target) {
        refuse(&mut client, "400 Bad Request").await;
        return ProxyOutcome::MalformedTarget;
    }

    let (path_only, query) = split_raw_target(raw_target);

    match resolve_viewer_target(path_only, request_headers, db, require_auth) {
        Resolution::NotAViewerPath => ProxyOutcome::NotAViewerPath,
        Resolution::Unauthorized => {
            refuse(&mut client, "401 Unauthorized").await;
            ProxyOutcome::Unauthorized
        }
        Resolution::UnknownBot => {
            refuse(&mut client, "404 Not Found").await;
            ProxyOutcome::UnknownBot
        }
        Resolution::Ready { target, web_port } => {
            proxy_to_vm(client, &target, &query, request_headers, web_port).await
        }
    }
}

/// A client that has already gone away must not panic a caller - every
/// write here is best-effort. Module-level (not nested in
/// `attach_vm_proxy` any more) so `accept_and_route`'s pass-through
/// detection can answer a non-viewer refusal the same way.
async fn refuse<S: AsyncWrite + Unpin>(client: &mut S, status: &str) {
    let line = format!("HTTP/1.1 {status}\r\nConnection: close\r\n\r\n");
    let _ = client.write_all(line.as_bytes()).await;
}

/// A request TARGET carrying a raw `\r` or `\n` must never reach
/// `build_upgrade_request`'s concatenated request line (S6-R F7). TS's
/// `attachVmProxy` runs `new URL(req.url, "http://127.0.0.1").pathname`
/// before `viewerTarget` ever sees the target (`vm.ts:697`), which
/// percent-encodes control characters and resolves `.`/`..` segments; this
/// port took the raw request target as-is and dropped that normalisation.
/// Rather than resurrect a URL parser, this checks the one property that
/// actually matters here: a `%0a`/`%0d` (an ASCII `%`, `0`, `a`/`d` - three
/// ordinary printable bytes) is left exactly as it arrived, because nothing
/// downstream of this ever decodes the query or `rest` half of a viewer
/// path, so it can never become a real line break; a literal `\r` or `\n`
/// byte is refused outright, because it already is one.
fn target_has_raw_control_break(target: &str) -> bool {
    target.contains(['\r', '\n'])
}

/// `raw_target`'s path and its `?query` (re-prefixed with `?`, or empty) -
/// the split `attach_vm_proxy` always did inline; pulled out so
/// `resolve_viewer_target` and `proxy_to_vm` share exactly one copy of it.
fn split_raw_target(raw_target: &str) -> (&str, String) {
    match raw_target.split_once('?') {
        Some((path, q)) => (path, format!("?{q}")),
        None => (raw_target, String::new()),
    }
}

/// What `resolve_viewer_target` decided about one request - everything
/// needed to either refuse or dial upstream, with no `db` reference left
/// in it, so a caller can drop a database lock (or a `MutexGuard`, which is
/// never `Send`) before going anywhere near an `.await`.
enum Resolution {
    NotAViewerPath,
    Unauthorized,
    UnknownBot,
    Ready { target: ViewerTarget, web_port: i32 },
}

/// The synchronous half of `attach_vm_proxy`: parse the path, check the
/// session, look up the bot's VM row. No `.await` anywhere in this
/// function - every call in it (`session_valid`, `get_vm`) is a plain
/// `rusqlite` query - so it is safe to call while holding a
/// `std::sync::MutexGuard<Db>` (`accept_and_route`'s real caller wraps
/// `AppState`'s db in exactly that), as long as the guard is dropped
/// before whatever comes next `.await`s (S6-06b's `start_vm_reaper` hit
/// this same guard-across-await wall for the same reason - see its doc).
fn resolve_viewer_target(
    path_only: &str,
    request_headers: &HeaderMap,
    db: &Db,
    require_auth: bool,
) -> Resolution {
    let Some(target) = viewer_target(path_only) else {
        return Resolution::NotAViewerPath;
    };

    if require_auth {
        let token = crate::auth::presented_token(request_headers);
        let valid = store::auth::session_valid(db, &token).unwrap_or(false);
        if !valid {
            return Resolution::Unauthorized;
        }
    }

    match store::vms::get_vm(db, &target.bot_id) {
        Ok(Some(vm)) => Resolution::Ready {
            target,
            web_port: vm.web_port,
        },
        _ => Resolution::UnknownBot,
    }
}

/// The relay itself, once a bot's VM row is already resolved: connect to
/// its web desktop, send the handshake, pipe both directions. Split out of
/// `attach_vm_proxy` (which still does exactly this after its own
/// `db`-touching checks) so `accept_and_route` can reach it too, having
/// already resolved the target with the database lock released.
async fn proxy_to_vm<S>(
    mut client: S,
    target: &ViewerTarget,
    query: &str,
    request_headers: &HeaderMap,
    web_port: i32,
) -> ProxyOutcome
where
    S: AsyncRead + AsyncWrite + Unpin + Send + 'static,
{
    let upstream_path = format!("/{}{}", target.rest, query);
    let handshake = build_upgrade_request(&upstream_path, request_headers, web_port);

    let mut upstream = match tokio::net::TcpStream::connect(("127.0.0.1", web_port as u16)).await {
        Ok(stream) => stream,
        Err(_) => {
            refuse(&mut client, "502 Bad Gateway").await;
            return ProxyOutcome::UpstreamUnreachable;
        }
    };

    if upstream.write_all(handshake.as_bytes()).await.is_err() {
        refuse(&mut client, "502 Bad Gateway").await;
        return ProxyOutcome::UpstreamUnreachable;
    }

    // Both halves die together, or a closed tab leaves a socket holding a
    // desktop open on meridian for as long as the process lives. Whatever
    // the upstream writes back (its own 101 line included) is relayed
    // byte for byte, same as TS's `upstream.pipe(socket); socket.pipe
    // (upstream)`.
    tokio::spawn(async move {
        let _ = tokio::io::copy_bidirectional(&mut client, &mut upstream).await;
    });

    ProxyOutcome::Proxying
}

/* -------------------------------------------------------------- accept and route */
//
// Everything above this point (`attach_vm_proxy`, S6-06b) already does the
// right thing GIVEN a raw duplex that is definitely a viewer upgrade. What
// was still missing is Node's other half: `server.on("upgrade", ...)` in TS
// fires at the raw `http.Server` for EVERY accepted connection, before
// anything has been parsed as a normal request - that's the only reason
// `attachVmProxy` gets the socket before any bytes are written back to the
// client. axum's `Router` has no equivalent hook: `axum::serve` hands each
// connection straight to hyper's own HTTP/1 machinery, which - for an
// ordinary route - always writes hyper's own response before anything else
// runs. `viewer_target` matching a path is not enough on its own to claim
// that connection either, since a plain GET to a viewer path (loading the
// desktop's own HTML shell, not the WebSocket) is exactly the kind of
// ordinary request that must still reach the normal server.
//
// `read_request_head`/`accept_and_route` below are the accept/route half of
// `attachVmProxy` (`vm.ts:686-741`): peek the request line and headers off
// a freshly accepted connection, by hand, BEFORE handing it to hyper at
// all - hyper's own upgrade support (`hyper::upgrade::on`) still requires
// writing a real HTTP response first, which is exactly the RFC 6455
// `Sec-WebSocket-Accept` feature S6-06b's own doc flagged as out of scope
// (see `attach_vm_proxy`'s doc); peeking first sidesteps needing it at all,
// the same way Node's raw socket handoff does. Only a request carrying an
// `Upgrade` header is even a candidate; everything else - viewer path or
// not - is left completely alone and handed to `serve`'s normal hyper
// connection with every peeked byte restored to the front, via
// `PrefixedStream`.

const MAX_REQUEST_HEAD_BYTES: usize = 16 * 1024;

/// The request line's target and headers of one HTTP/1.x request, read by
/// hand off a raw connection - `raw` is the EXACT bytes consumed doing it,
/// so a non-upgrade connection can be handed onward with nothing lost.
struct RequestHead {
    target: String,
    headers: HeaderMap,
    is_upgrade: bool,
    raw: Vec<u8>,
}

/// Reads one HTTP/1.x request head (request line + headers, up to and
/// including the terminating blank line) off `stream`, one byte at a time.
/// Slow next to a real parser, but the head is a few hundred bytes and this
/// is the only way to stop EXACTLY at the boundary without ever reading
/// into whatever comes after (a body, or the next pipelined request) - a
/// chunked read risks swallowing bytes that a real HTTP server (`serve`'s
/// hyper fallback) would need to see. `None` on a malformed head, a closed
/// connection before one full head arrives, or a head over
/// `MAX_REQUEST_HEAD_BYTES` (a client that never sends `\r\n\r\n` must not
/// be read from forever).
async fn read_request_head<S: AsyncRead + Unpin>(stream: &mut S) -> Option<RequestHead> {
    let mut raw = Vec::new();
    let mut byte = [0u8; 1];
    loop {
        if raw.len() >= MAX_REQUEST_HEAD_BYTES {
            return None;
        }
        stream.read_exact(&mut byte).await.ok()?;
        raw.push(byte[0]);
        if raw.ends_with(b"\r\n\r\n") {
            break;
        }
    }
    parse_request_head(&raw).map(|(target, headers, is_upgrade)| RequestHead {
        target,
        headers,
        is_upgrade,
        raw,
    })
}

/// The request line and headers, hand-parsed - not a general HTTP parser
/// (no continuation lines, no folding), just enough for a well-formed
/// request from a real browser or `serve`'s own test doubles. A line this
/// crate cannot make sense of drops that ONE header rather than failing
/// the whole request, same as a tolerant proxy would.
fn parse_request_head(raw: &[u8]) -> Option<(String, HeaderMap, bool)> {
    let text = std::str::from_utf8(raw).ok()?;
    let mut lines = text.split("\r\n");

    let request_line = lines.next()?;
    let mut parts = request_line.split(' ');
    let _method = parts.next()?;
    let target = parts.next()?.to_string();
    let _version = parts.next()?;

    // S6-R F7: `lines` was split on the two-byte sequence "\r\n", so a bare
    // `\r` or `\n` on its own survives inside `request_line` and therefore
    // inside `target` - e.g. a query of `?a=1\nX-Injected:y` never meets a
    // "\r\n" boundary and rides straight through to here. Refuse the whole
    // head rather than accept a target that could later inject a line into
    // `build_upgrade_request`'s handshake.
    if target_has_raw_control_break(&target) {
        return None;
    }

    let mut headers = HeaderMap::new();
    let mut is_upgrade = false;
    for line in lines {
        if line.is_empty() {
            continue;
        }
        let Some((name, value)) = line.split_once(':') else {
            continue;
        };
        let name = name.trim();
        let value = value.trim();
        if name.eq_ignore_ascii_case("upgrade") {
            is_upgrade = true;
        }
        if let (Ok(header_name), Ok(header_value)) = (
            axum::http::HeaderName::from_bytes(name.as_bytes()),
            axum::http::HeaderValue::from_str(value),
        ) {
            headers.append(header_name, header_value);
        }
    }

    Some((target, headers, is_upgrade))
}

/// A stream with some already-read bytes glued back onto the front of its
/// read side - reconstructs "as if nothing had been peeked" for whatever
/// reads from it next. Writes pass straight through untouched.
pub struct PrefixedStream<S> {
    prefix: std::io::Cursor<Vec<u8>>,
    inner: S,
}

impl<S> PrefixedStream<S> {
    fn new(prefix: Vec<u8>, inner: S) -> Self {
        Self {
            prefix: std::io::Cursor::new(prefix),
            inner,
        }
    }
}

impl<S: AsyncRead + Unpin> AsyncRead for PrefixedStream<S> {
    fn poll_read(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<std::io::Result<()>> {
        let this = self.get_mut();
        let pos = this.prefix.position() as usize;
        let remaining = &this.prefix.get_ref()[pos..];
        if !remaining.is_empty() {
            let n = remaining.len().min(buf.remaining());
            buf.put_slice(&remaining[..n]);
            this.prefix.set_position((pos + n) as u64);
            return Poll::Ready(Ok(()));
        }
        Pin::new(&mut this.inner).poll_read(cx, buf)
    }
}

impl<S: AsyncWrite + Unpin> AsyncWrite for PrefixedStream<S> {
    fn poll_write(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<std::io::Result<usize>> {
        Pin::new(&mut self.get_mut().inner).poll_write(cx, buf)
    }
    fn poll_flush(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
        Pin::new(&mut self.get_mut().inner).poll_flush(cx)
    }
    fn poll_shutdown(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
        Pin::new(&mut self.get_mut().inner).poll_shutdown(cx)
    }
}

/// What `accept_and_route` did with one freshly accepted connection - the
/// observable both bite tests assert on. `PassThrough` carries the stream
/// back (bytes already peeked restored to the front) rather than a bool,
/// so nothing about a non-viewer connection is lost.
pub enum RouteOutcome<S> {
    /// Was a viewer upgrade; handled (proxied, or refused) already.
    Viewer(ProxyOutcome),
    /// Not a viewer upgrade - the caller serves it normally.
    PassThrough(PrefixedStream<S>),
    /// No full request head ever arrived.
    Unreadable,
}

/// Manual, not derived: `PrefixedStream<S>` has no reason to require
/// `S: Debug`, and a test's `{outcome:?}` only ever needs to say WHICH
/// variant this was, not print a stream's bytes.
impl<S> std::fmt::Debug for RouteOutcome<S> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            RouteOutcome::Viewer(outcome) => f.debug_tuple("Viewer").field(outcome).finish(),
            RouteOutcome::PassThrough(_) => f.write_str("PassThrough(..)"),
            RouteOutcome::Unreadable => f.write_str("Unreadable"),
        }
    }
}

/// The accept/route half of `attachVmProxy` (`vm.ts:686-741`): given one
/// freshly accepted connection, decide whether it is a bot's screen
/// (relayed via `attach_vm_proxy`'s own logic, `proxy_to_vm`/`refuse`) or
/// ordinary traffic (`PassThrough`, untouched). This is the piece S6-06b's
/// relay had no caller for - see this module's own header doc for why a
/// route match alone is not enough (`viewer_target` matching plus an
/// `Upgrade` header, together, is).
///
/// 🔴 Authorization boundary, same as `viewer_target`/`attach_vm_proxy`
/// (S6-06b bite (a); this ticket's own bite (a) is the end-to-end version
/// of that same guarantee): a request whose path names bot A must reach
/// bot A's `VmRow.web_port` and nothing else - `resolve_viewer_target`'s
/// `get_vm` lookup is the ONLY thing that decides that, by exact bot id,
/// same as before.
///
/// Takes `db: &Db` directly - correct and sufficient for every test below,
/// since a test simply `.await`s this in its own async fn without ever
/// spawning it. `serve`'s own per-connection task, which DOES need to be
/// `Send` for `tokio::spawn`, cannot call this function with a live
/// `std::sync::MutexGuard<Db>` (or even a bare `&Db`, which is `!Send`
/// since `Db: !Sync`) sitting across this function's `read_request_head`
/// await - so `serve` does not call `accept_and_route` at all; it inlines
/// the same three steps (read head, resolve with the lock held only for
/// that one synchronous call, dispatch) so the lock is provably dropped
/// before either `.await`. See `serve`'s own doc.
pub async fn accept_and_route<S>(mut stream: S, db: &Db, require_auth: bool) -> RouteOutcome<S>
where
    S: AsyncRead + AsyncWrite + Unpin + Send + 'static,
{
    let Some(head) = read_request_head(&mut stream).await else {
        return RouteOutcome::Unreadable;
    };

    if !head.is_upgrade {
        return RouteOutcome::PassThrough(PrefixedStream::new(head.raw, stream));
    }

    let (path_only, query) = split_raw_target(&head.target);
    match resolve_viewer_target(path_only, &head.headers, db, require_auth) {
        Resolution::NotAViewerPath => {
            RouteOutcome::PassThrough(PrefixedStream::new(head.raw, stream))
        }
        Resolution::Unauthorized => {
            refuse(&mut stream, "401 Unauthorized").await;
            RouteOutcome::Viewer(ProxyOutcome::Unauthorized)
        }
        Resolution::UnknownBot => {
            refuse(&mut stream, "404 Not Found").await;
            RouteOutcome::Viewer(ProxyOutcome::UnknownBot)
        }
        Resolution::Ready { target, web_port } => RouteOutcome::Viewer(
            proxy_to_vm(stream, &target, &query, &head.headers, web_port).await,
        ),
    }
}

/// Runs the whole server: `accept_and_route`'s logic per connection
/// (inlined, not called - see `accept_and_route`'s own doc for why),
/// dispatching a bot-screen upgrade straight to `proxy_to_vm`/`refuse` and
/// everything else to `app` (the ordinary axum `Router`) through a manual
/// HTTP/1 connection. This is the actual replacement for
/// `axum::serve(listener, app).await` - main.rs's one-line swap, landed by
/// the orchestrator per this ticket's file-ownership rule.
///
/// **Nothing in this crate has run this against a real browser or a real
/// container** (no Docker, no browser on this workstation - S6's own
/// constraint). What IS proven here: `cargo check` accepts this against
/// the real `axum::Router`/`store::Db` types, and `accept_and_route`
/// (identical routing/refusal logic, minus the lock-scoping this function
/// exists to get right) is green against both bites. The meridian smoke
/// test is still the only proof a real socket makes it through.
/// 🔴 An accept error NEVER ends this loop (S6-R F1). The first version
/// wrote `listener.accept().await?`, and `main.rs` wraps this in
/// `.expect("serve")` - so one `ECONNABORTED` from a client that vanished
/// between the SYN and the accept took the whole server down. `axum::serve`
/// does not do that (`axum/src/serve/listener.rs`, `handle_accept_error`):
/// a per-connection error is retried immediately, and anything else is
/// logged and slept on so a persistent failure (out of file descriptors)
/// cannot spin the CPU. This mirrors that, and `egress_proxy.rs`'s own
/// accept loop already did the same thing.
///
/// It matters more here than it looks: the systemd unit is
/// `Restart=on-failure` with `RestartSec=2` and no `StartLimit` override,
/// so five panics inside ten seconds leave the service `failed` until
/// somebody runs `systemctl reset-failed` by hand.
pub async fn serve(
    listener: tokio::net::TcpListener,
    app: axum::Router,
    db: Arc<Mutex<Db>>,
    require_auth: bool,
) -> std::io::Result<()> {
    serve_with_accepter(listener, app, db, require_auth).await
}

/// What `serve` accepts from. Exists so a test can hand the loop a stream
/// of accept ERRORS before a real connection: forcing a genuine
/// `ECONNABORTED` out of the OS is not portable, and F1 was a bug about
/// what the loop does with one.
pub trait Accepter: Send + 'static {
    fn accept(
        &mut self,
    ) -> impl std::future::Future<
        Output = std::io::Result<(tokio::net::TcpStream, std::net::SocketAddr)>,
    > + Send;
}

impl Accepter for tokio::net::TcpListener {
    async fn accept(&mut self) -> std::io::Result<(tokio::net::TcpStream, std::net::SocketAddr)> {
        tokio::net::TcpListener::accept(self).await
    }
}

pub async fn serve_with_accepter<A: Accepter>(
    mut listener: A,
    app: axum::Router,
    db: Arc<Mutex<Db>>,
    require_auth: bool,
) -> std::io::Result<()> {
    loop {
        let (stream, _addr) = match listener.accept().await {
            Ok(accepted) => accepted,
            Err(err) if is_connection_error(&err) => continue,
            Err(err) => {
                tracing::warn!(%err, "accept failed; retrying");
                tokio::time::sleep(std::time::Duration::from_secs(1)).await;
                continue;
            }
        };
        let app = app.clone();
        let db = Arc::clone(&db);
        tokio::spawn(async move {
            route_one_connection(stream, app, db, require_auth).await;
        });
    }
}

/// The errors that belong to the connection that just died, not to the
/// listener - retry these immediately rather than sleeping. Same set
/// `axum::serve` treats this way.
fn is_connection_error(err: &std::io::Error) -> bool {
    matches!(
        err.kind(),
        std::io::ErrorKind::ConnectionRefused
            | std::io::ErrorKind::ConnectionAborted
            | std::io::ErrorKind::ConnectionReset
    )
}

async fn route_one_connection(
    mut stream: tokio::net::TcpStream,
    app: axum::Router,
    db: Arc<Mutex<Db>>,
    require_auth: bool,
) {
    let Some(head) = read_request_head(&mut stream).await else {
        return;
    };

    if !head.is_upgrade {
        serve_via_hyper(PrefixedStream::new(head.raw, stream), app).await;
        return;
    }

    let (path_only, query) = split_raw_target(&head.target);

    // The lock lives only inside this block, around one synchronous call -
    // `resolve_viewer_target` never `.await`s (see its own doc) - so the
    // `MutexGuard` (never `Send`) is dropped here, before either `.await`
    // below, and never becomes part of this `tokio::spawn`ed task's state.
    let resolution = {
        let guard = db.lock().unwrap_or_else(PoisonError::into_inner);
        resolve_viewer_target(path_only, &head.headers, &guard, require_auth)
    };

    match resolution {
        Resolution::NotAViewerPath => {
            serve_via_hyper(PrefixedStream::new(head.raw, stream), app).await;
        }
        Resolution::Unauthorized => {
            refuse(&mut stream, "401 Unauthorized").await;
        }
        Resolution::UnknownBot => {
            refuse(&mut stream, "404 Not Found").await;
        }
        Resolution::Ready { target, web_port } => {
            proxy_to_vm(stream, &target, &query, &head.headers, web_port).await;
        }
    }
}

/// Ordinary traffic's path: one manual hyper HTTP/1 connection over
/// `io`, calling straight into `app`. The documented axum pattern for
/// serving a `Router` without `axum::serve` (`hyper::server::conn::http1`
/// plus `hyper_util::rt::TokioIo`) - needed here only because this
/// listener's accept loop has to look at the raw bytes of each connection
/// BEFORE anything is parsed as a request (see this section's own header
/// doc); `axum::serve` alone still works fine for a process with no
/// viewer proxy at all.
async fn serve_via_hyper<S>(io: S, app: axum::Router)
where
    S: AsyncRead + AsyncWrite + Unpin + Send + 'static,
{
    let io = hyper_util::rt::TokioIo::new(io);
    let hyper_service =
        hyper::service::service_fn(move |req: hyper::Request<hyper::body::Incoming>| {
            let app = app.clone();
            async move {
                let req = req.map(axum::body::Body::new);
                let response = tower::ServiceExt::oneshot(app, req)
                    .await
                    .unwrap_or_else(|err: std::convert::Infallible| match err {});
                Ok::<_, std::convert::Infallible>(response)
            }
        });
    let _ = hyper::server::conn::http1::Builder::new()
        .serve_connection(io, hyper_service)
        .with_upgrades()
        .await;
}
