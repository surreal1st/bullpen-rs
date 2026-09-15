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

use axum::http::HeaderMap;
use store::Db;
use tokio::io::AsyncWriteExt;

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
const HOP_BY_HOP: &[&str] = &[
    "connection",
    "keep-alive",
    "proxy-authenticate",
    "proxy-authorization",
    "te",
    "trailer",
    "transfer-encoding",
    "upgrade",
    "content-encoding",
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
    S: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin + Send + 'static,
{
    let (path_only, query) = match raw_target.split_once('?') {
        Some((path, q)) => (path, format!("?{q}")),
        None => (raw_target, String::new()),
    };

    let Some(target) = viewer_target(path_only) else {
        return ProxyOutcome::NotAViewerPath;
    };

    // A client that has already gone away must not panic this handler -
    // every write below is best-effort.
    async fn refuse<S: tokio::io::AsyncWrite + Unpin>(client: &mut S, status: &str) {
        let line = format!("HTTP/1.1 {status}\r\nConnection: close\r\n\r\n");
        let _ = client.write_all(line.as_bytes()).await;
    }

    if require_auth {
        let token = crate::auth::presented_token(request_headers);
        let valid = store::auth::session_valid(db, &token).unwrap_or(false);
        if !valid {
            refuse(&mut client, "401 Unauthorized").await;
            return ProxyOutcome::Unauthorized;
        }
    }

    let vm = match store::vms::get_vm(db, &target.bot_id) {
        Ok(Some(vm)) => vm,
        _ => {
            refuse(&mut client, "404 Not Found").await;
            return ProxyOutcome::UnknownBot;
        }
    };

    let upstream_path = format!("/{}{}", target.rest, query);
    let handshake = build_upgrade_request(&upstream_path, request_headers, vm.web_port);

    let mut upstream = match tokio::net::TcpStream::connect(("127.0.0.1", vm.web_port as u16)).await
    {
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
