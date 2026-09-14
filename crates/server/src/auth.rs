//! Header parsing for the session credential. Mirrors the TS
//! `presentedToken`/`SESSION_COOKIE` in `projects/bullpen-night/src/server/auth.ts`.

use axum::http::HeaderMap;
use axum::http::header::{AUTHORIZATION, COOKIE};

pub const SESSION_COOKIE: &str = "bullpen_session";

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
