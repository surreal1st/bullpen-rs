//! S13a-01: the one seam between `api.rs`/`events.rs` and the platform.
//!
//! Before this ticket, `api.rs` called `gloo_net::http::{Request, Response}`
//! directly - fine for the web build, fatal for a native one (no `fetch`,
//! no `web_sys`). Every route function in `api.rs` still calls
//! `Request::get(url).send().await` exactly as it did before - only the
//! import line changed, from `gloo_net::http::{Request, Response}` to
//! `crate::transport::{Request, Response}` - because `Request`/`Response`
//! here keep the same builder shape and dispatch to whichever
//! [`Transport`] impl this target has:
//!
//! - `web.rs` (`#[cfg(target_arch = "wasm32")]`): today's `gloo-net`/
//!   `web-sys` fetch code, moved out of `api.rs` verbatim.
//! - `native.rs` (`#[cfg(not(target_arch = "wasm32"))]`): the same shape
//!   over `reqwest`, rustls only - see that module's doc on why never
//!   `native-tls`.
//!
//! The three genuinely browser-only calls that used to sit in `api.rs`
//! itself moved here too: `login`'s
//! `.credentials(web_sys::RequestCredentials::Include)` is now
//! `Request::with_credentials()` (a no-op on native - see its doc);
//! `send_message`'s `ReadableStreamDefaultReader` loop is now
//! `Response::into_body_stream()` plus [`BodyStream::next_chunk`], read the
//! same way on both platforms; and `fetch_models`/`fetch_bot_memory_query`'s
//! `js_sys::encode_uri_component` - a JS-extern binding that, like
//! `gloo-net`, only resolves inside an actual wasm+JS runtime, so it was as
//! much a native-breaker as the other two even though the ticket's own
//! grep for `gloo-net`/`web-sys` missed it - is now [`encode_uri_component`]
//! below, a plain Rust port of the same MDN-documented escaping.

use serde::Serialize;
use serde::de::DeserializeOwned;

#[cfg(not(target_arch = "wasm32"))]
mod native;
#[cfg(target_arch = "wasm32")]
mod web;

#[cfg(not(target_arch = "wasm32"))]
use native::NativeTransport as Backend;
#[cfg(target_arch = "wasm32")]
use web::WebTransport as Backend;

#[cfg(not(target_arch = "wasm32"))]
use native::NativeBody as PlatformBody;
#[cfg(target_arch = "wasm32")]
use web::WebBody as PlatformBody;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Method {
    Get,
    Post,
    Put,
    Patch,
    Delete,
}

/// One HTTP round trip as the platform sees it: a method, a path
/// (relative on web - resolved by the browser's own `fetch` against its
/// origin; resolved against `BULLPEN_URL` on native, see
/// `native.rs::resolve_url`), an optional pre-serialized JSON body, and
/// whether the session cookie must ride along explicitly (`login`'s
/// `with_credentials()` below).
pub struct RequestSpec {
    pub method: Method,
    pub url: String,
    pub body: Option<Vec<u8>>,
    pub with_credentials: bool,
}

/// A finished response: status, the one header `api.rs::send_message`
/// actually reads, and the body as a stream so a caller can either buffer
/// it whole (`Response::json`) or read it frame-by-frame (the SSE loop).
pub struct TransportResponse {
    pub status: u16,
    pub content_type: Option<String>,
    pub body: PlatformBody,
}

/// The seam itself, implemented once per platform for that platform's own
/// zero-sized marker type (`web::WebTransport`, `native::NativeTransport`).
/// `mod.rs` picks the implementation at compile time via
/// `#[cfg(target_arch = "wasm32")]`, so nothing outside `transport/` names
/// `gloo-net`, `web-sys` or `reqwest`.
pub trait Transport {
    async fn request(spec: RequestSpec) -> Result<TransportResponse, String>;
}

/// One chunk of a streamed body, or `None` at end of stream - the portable
/// half of the old wasm-only `ReadableStreamDefaultReader` loop that used
/// to live in `api.rs::send_message` before this ticket.
pub struct BodyStream(PlatformBody);

impl BodyStream {
    pub async fn next_chunk(&mut self) -> Result<Option<Vec<u8>>, String> {
        self.0.next_chunk().await
    }

    async fn collect(mut self) -> Result<Vec<u8>, String> {
        let mut buf = Vec::new();
        while let Some(chunk) = self.next_chunk().await? {
            buf.extend(chunk);
        }
        Ok(buf)
    }
}

/* --------------------------------------------------- the ergonomic wrapper */

/// Same call shape every `api.rs` route function already used against
/// `gloo_net::http::Request`, namely
/// `Request::get(url).json(&body)?.send().await?`, now backed by whichever
/// [`Transport`] this target has. This is the only reason S13a-01 could
/// touch two imports and a handful of lines instead of rewriting fifty call
/// sites.
pub struct Request {
    spec: RequestSpec,
}

impl Request {
    pub fn get(url: &str) -> Self {
        Self::new(Method::Get, url)
    }

    pub fn post(url: &str) -> Self {
        Self::new(Method::Post, url)
    }

    pub fn put(url: &str) -> Self {
        Self::new(Method::Put, url)
    }

    pub fn patch(url: &str) -> Self {
        Self::new(Method::Patch, url)
    }

    pub fn delete(url: &str) -> Self {
        Self::new(Method::Delete, url)
    }

    fn new(method: Method, url: &str) -> Self {
        Self {
            spec: RequestSpec {
                method,
                url: url.to_string(),
                body: None,
                with_credentials: false,
            },
        }
    }

    pub fn json<T: Serialize>(mut self, body: &T) -> Result<Self, String> {
        self.spec.body = Some(serde_json::to_vec(body).map_err(|e| e.to_string())?);
        Ok(self)
    }

    /// Ported from `api.rs::login`'s
    /// `.credentials(web_sys::RequestCredentials::Include)`. Native has no
    /// per-request browser cookie jar to opt into - `native.rs`'s shared
    /// client already carries the session cookie on every request via its
    /// own cookie store, so this is a no-op there. Kept as an explicit call
    /// (rather than silently dropped) so a reader of `login` still sees
    /// that sending credentials is deliberate, not an oversight.
    pub fn with_credentials(mut self) -> Self {
        self.spec.with_credentials = true;
        self
    }

    pub async fn send(self) -> Result<Response, String> {
        let resp = Backend::request(self.spec).await?;
        Ok(Response {
            status: resp.status,
            content_type: resp.content_type,
            body: BodyStream(resp.body),
        })
    }
}

pub struct Response {
    status: u16,
    content_type: Option<String>,
    body: BodyStream,
}

impl Response {
    pub fn ok(&self) -> bool {
        (200..300).contains(&self.status)
    }

    pub fn status(&self) -> u16 {
        self.status
    }

    /// `api.rs::send_message`'s JSON-vs-stream check - the one response
    /// header this client reads (`resp.headers().get("content-type")`,
    /// before this ticket).
    pub fn content_type(&self) -> Option<&str> {
        self.content_type.as_deref()
    }

    pub async fn json<T: DeserializeOwned>(self) -> Result<T, String> {
        let bytes = self.body.collect().await?;
        serde_json::from_slice(&bytes).map_err(|e| e.to_string())
    }

    /// `api.rs::send_message`'s SSE loop reads the body one chunk at a
    /// time instead of buffering it whole - see [`BodyStream::next_chunk`].
    pub fn into_body_stream(self) -> BodyStream {
        self.body
    }
}

/// Percent-encodes a query value the way `encodeURIComponent` does -
/// ported off `js_sys::encode_uri_component` (`api.rs::fetch_models`,
/// `fetch_bot_memory_query`), which is a `#[wasm_bindgen] extern "C"`
/// binding into the browser's own global function and, like `gloo-net`,
/// only resolves inside an actual wasm+JS runtime - it was never callable
/// natively even though nothing in the ticket's own `gloo-net`/`web-sys`
/// grep named it. Unreserved set per MDN's `encodeURIComponent` doc:
/// `A-Z a-z 0-9 - _ . ! ~ * ' ( )`.
pub fn encode_uri_component(input: &str) -> String {
    let mut out = String::with_capacity(input.len());
    for byte in input.bytes() {
        match byte {
            b'A'..=b'Z'
            | b'a'..=b'z'
            | b'0'..=b'9'
            | b'-'
            | b'_'
            | b'.'
            | b'!'
            | b'~'
            | b'*'
            | b'\''
            | b'('
            | b')' => out.push(byte as char),
            _ => out.push_str(&format!("%{byte:02X}")),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn encode_uri_component_matches_js_semantics() {
        assert_eq!(encode_uri_component("abc123-_.!~*'()"), "abc123-_.!~*'()");
        assert_eq!(encode_uri_component("a b"), "a%20b");
        assert_eq!(encode_uri_component("caf\u{e9}"), "caf%C3%A9");
        assert_eq!(encode_uri_component("a&b=c"), "a%26b%3Dc");
    }
}
