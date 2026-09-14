//! Session credentials: reading one off a request, building one onto a
//! response, and the `/api/*` gate that requires one. Mirrors the TS
//! `presentedToken`/`SESSION_COOKIE`/`isOpenPath`/`sessionCookie` in
//! `projects/bullpen-night/src/server/auth.ts`, and the gate itself
//! (`app.ts:1037-1058`).

use axum::extract::{Request, State};
use axum::http::HeaderMap;
use axum::http::header::{AUTHORIZATION, COOKIE};
use axum::http::{StatusCode, header::SET_COOKIE};
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};
use axum::{Json, http::HeaderValue};
use serde_json::json;
use std::sync::{Mutex, PoisonError};
use std::time::{Duration, Instant};

use crate::{AppError, AppState};

pub const SESSION_COOKIE: &str = "bullpen_session";

/// How long a session cookie lasts. Mirrors the TS `SESSION_DAYS`.
const SESSION_DAYS: i64 = 60;

/// The session token presented by a request, from either transport: a
/// `Bearer` authorization header (iOS app, admin scripts) or the
/// `bullpen_session` cookie (browser, Electron shell). Returns "" when
/// there is none.
///
/// Deliberately no query-parameter fallback: a token in a URL lands in
/// every proxy log between here and the client.
pub fn presented_token(headers: &HeaderMap) -> String {
    if let Some(auth) = headers.get(AUTHORIZATION).and_then(|v| v.to_str().ok())
        && let Some(token) = bearer_token(auth)
    {
        return token;
    }
    if let Some(cookie) = headers.get(COOKIE).and_then(|v| v.to_str().ok())
        && let Some(token) = cookie_token(cookie)
    {
        return token;
    }
    String::new()
}

/// Matches the TS `/^Bearer\s+(\S+)$/i`: the whole header must be the word
/// "Bearer", whitespace, then a single whitespace-free token with nothing
/// trailing it.
fn bearer_token(auth: &str) -> Option<String> {
    let mut parts = auth.splitn(2, char::is_whitespace);
    let scheme = parts.next()?;
    let rest = parts.next()?.trim_start();
    if !scheme.eq_ignore_ascii_case("bearer")
        || rest.is_empty()
        || rest.contains(char::is_whitespace)
    {
        return None;
    }
    Some(rest.to_string())
}

fn cookie_token(cookie: &str) -> Option<String> {
    for part in cookie.split(';') {
        let part = part.trim();
        if let Some((name, value)) = part.split_once('=')
            && name == SESSION_COOKIE
        {
            return Some(value.to_string());
        }
    }
    None
}

/// The `Set-Cookie` value for a new session. Mirrors the TS `sessionCookie`.
pub fn session_cookie(token: &str, secure: bool) -> String {
    let mut value = format!(
        "{SESSION_COOKIE}={token}; Path=/; HttpOnly; SameSite=Lax; Max-Age={}",
        SESSION_DAYS * 24 * 60 * 60
    );
    // Omitted only for plain-http local development; over the tunnel it is
    // always on, and a cookie without it can be stripped to http and read.
    if secure {
        value.push_str("; Secure");
    }
    value
}

/// The `Set-Cookie` value that clears a session. Mirrors the TS `clearedCookie`.
pub fn cleared_cookie() -> String {
    format!("{SESSION_COOKIE}=; Path=/; HttpOnly; SameSite=Lax; Max-Age=0")
}

/// Whether the connection this request arrived on should be treated as
/// https, for the `Secure` cookie flag. `BULLPEN_HOST` defaults to
/// `127.0.0.1` (B7) with plain http; the public path is a Cloudflare Tunnel
/// in front of it, which sets `x-forwarded-proto`. Unlike the TS version
/// (`new URL(c.req.url).protocol === "https:"`), axum's `Request::uri()` on
/// an ordinary origin-form HTTP/1.1 request carries no scheme to read, so
/// this checks only the forwarded header - the one path that matters here.
fn is_secure(headers: &HeaderMap) -> bool {
    headers
        .get("x-forwarded-proto")
        .and_then(|v| v.to_str().ok())
        .is_some_and(|v| v.eq_ignore_ascii_case("https"))
}

/// Paths reachable without a session. Exact match only, except the
/// `invites/` prefix - a prefix test elsewhere would also open
/// `/api/health/../bots` and anything else a creative URL can spell.
/// Mirrors the TS `OPEN_PATHS`/`OPEN_PREFIXES` (`auth.ts:216-255`), with two
/// differences: `version` is inert build metadata that was open before this
/// ticket and stays that way, and `auth/check` is listed open here because
/// its own handler (`routes/mod.rs::auth_check`) already re-checks
/// `session_valid` and answers 401 itself - gating it here too would only
/// ever repeat the same answer in two places. `slack/events`, `teams/events`,
/// `stripe/webhook` and the `hooks/` prefix are the TS list's webhook routes;
/// none of those features exist in bullpen-rs yet, so they are left out
/// rather than naming paths nothing serves.
const OPEN_PATHS: &[&str] = &[
    "/api/health",
    "/api/version",
    "/api/auth/status",
    "/api/auth/login",
    "/api/auth/logout",
    "/api/auth/check",
];

const OPEN_PREFIXES: &[&str] = &["/api/invites/"];

pub fn is_open_path(path: &str) -> bool {
    OPEN_PATHS.contains(&path) || OPEN_PREFIXES.iter().any(|prefix| path.starts_with(prefix))
}

/// B7: the `/api/*` session gate that was never ported - every route
/// (`/api/bots/:id/messages` included) answered unauthenticated while `main`
/// bound `0.0.0.0`. Wired in `lib.rs::build_app` as a layer over
/// `routes::router()` only, so static files and the SPA fallback (never
/// wrapped in this layer) stay open the way the ticket asks, with no path
/// check needed here for that half. Mirrors the TS gate at `app.ts:1039-1057`,
/// including its order: "is this open" before "is a password even set"
/// before "is the presented token valid".
pub async fn require_session(State(state): State<AppState>, req: Request, next: Next) -> Response {
    let path = req.uri().path().to_string();
    if is_open_path(&path) {
        return next.run(req).await;
    }

    let configured = {
        let db = state.db();
        match store::is_configured(&db) {
            Ok(c) => c,
            Err(err) => return AppError::from(err).into_response(),
        }
    };
    // A server with no password refuses everything rather than allowing it -
    // a fresh deploy is exactly when it is most exposed.
    if !configured {
        return (
            StatusCode::SERVICE_UNAVAILABLE,
            Json(json!({"error": "This Bullpen has no password set yet.", "setup": true})),
        )
            .into_response();
    }

    let token = presented_token(req.headers());
    let signed_in = {
        let db = state.db();
        match store::session_valid(&db, &token) {
            Ok(v) => v,
            Err(err) => return AppError::from(err).into_response(),
        }
    };
    if !signed_in {
        return (
            StatusCode::UNAUTHORIZED,
            Json(json!({"error": "Sign in to Bullpen."})),
        )
            .into_response();
    }

    next.run(req).await
}

/// Sets the response's `Set-Cookie` header from the request that produced
/// it, so `routes/auth.rs`'s login/logout handlers don't each have to know
/// `HeaderValue`'s fallible parse.
pub fn set_session_cookie(response: &mut Response, headers: &HeaderMap, token: &str) {
    let value = session_cookie(token, is_secure(headers));
    if let Ok(header) = HeaderValue::from_str(&value) {
        response.headers_mut().insert(SET_COOKIE, header);
    }
}

pub fn clear_session_cookie(response: &mut Response) {
    if let Ok(header) = HeaderValue::from_str(&cleared_cookie()) {
        response.headers_mut().insert(SET_COOKIE, header);
    }
}

/// Login attempts, throttled per server process - one shared secret and an
/// unlimited guess rate is a password that falls overnight. Mirrors the TS
/// `loginThrottled`/`recordLoginFailure`/`clearLoginFailures`
/// (`auth.ts:257-282`), deliberately crude in the same way: a counter and a
/// window, no storage. Held on `AppState` (one per running server) rather
/// than the TS module-level `let attempts` (one per Node process) so that
/// `cargo test`'s many parallel `AppState`s - each its own would-be
/// "process" - never share a counter and flake each other's throttle tests;
/// same reasoning F25 already accepted for the per-instance change bus.
pub struct LoginThrottle(Mutex<Attempts>);

struct Attempts {
    count: u32,
    since: Instant,
}

const ATTEMPT_LIMIT: u32 = 10;
const ATTEMPT_WINDOW: Duration = Duration::from_secs(5 * 60);

impl Default for LoginThrottle {
    fn default() -> Self {
        Self::new()
    }
}

impl LoginThrottle {
    pub fn new() -> Self {
        Self(Mutex::new(Attempts {
            count: 0,
            since: Instant::now(),
        }))
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Attempts> {
        self.0.lock().unwrap_or_else(PoisonError::into_inner)
    }

    pub fn throttled(&self) -> bool {
        let attempts = self.lock();
        attempts.since.elapsed() <= ATTEMPT_WINDOW && attempts.count >= ATTEMPT_LIMIT
    }

    pub fn record_failure(&self) {
        let mut attempts = self.lock();
        if attempts.since.elapsed() > ATTEMPT_WINDOW {
            attempts.count = 0;
            attempts.since = Instant::now();
        }
        attempts.count += 1;
    }

    pub fn clear(&self) {
        let mut attempts = self.lock();
        attempts.count = 0;
        attempts.since = Instant::now();
    }
}
