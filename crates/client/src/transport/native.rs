//! The non-wasm half of the transport seam ([`super::Transport`]) -
//! `reqwest` over rustls, **never `native-tls`**: the workspace's own
//! `reqwest` entry already pins `default-features = false` with
//! `rustls-tls`, and `client()` below calls `.use_rustls_tls()` explicitly
//! so a future feature-flag mistake that drags `default-tls` back in fails
//! loudly (a link error) instead of silently starting to ship OpenSSL/
//! schannel - see S13a-01's ticket, which is explicit that a native TLS
//! stack is exactly how a port like this one starts dragging in system
//! dependencies.
//!
//! One shared `reqwest::Client` for the whole process, with its cookie
//! store turned on, so `login`'s session survives across every later
//! request - a native build has no browser cookie jar to lean on the way
//! the web build does (`Request::with_credentials`'s doc).

use super::{Method, RequestSpec, Transport, TransportResponse};
use std::sync::OnceLock;

/// `deploy/Bullpen-rs.cmd` already sets `BULLPEN_URL` for the desktop
/// build; this reads the same variable, falling back to the server's own
/// tailnet address (S13a ticket's stated default) when it is unset - e.g.
/// running the binary by hand outside the launcher script.
const DEFAULT_BASE_URL: &str = "http://100.119.100.103:4380";

fn base_url() -> String {
    std::env::var("BULLPEN_URL").unwrap_or_else(|_| DEFAULT_BASE_URL.to_string())
}

fn client() -> &'static reqwest::Client {
    static CLIENT: OnceLock<reqwest::Client> = OnceLock::new();
    CLIENT.get_or_init(|| {
        reqwest::Client::builder()
            .cookie_store(true)
            .use_rustls_tls()
            .build()
            .expect("a rustls-only reqwest client always builds")
    })
}

/// Every relative path in `api.rs` (`/api/rooms`, `/api/bots/:id/...`) is
/// resolved against `base` the same way a browser resolves an
/// absolute-path `fetch` against its own origin: `Url::join` on a path
/// starting with `/` replaces the base's entire path, so a base with or
/// without a trailing slash or its own sub-path behaves the same either
/// way.
///
/// **Bite (b)** (S13a-01's `## Results`): replace this function's body
/// with `Ok(path.to_string())` - dropping `base` entirely - and
/// `resolve_url_joins_the_base_and_the_path` goes red, because the
/// resolved URL is no longer absolute.
fn resolve_url_with_base(base: &str, path: &str) -> Result<String, String> {
    let base = reqwest::Url::parse(base).map_err(|e| format!("bad BULLPEN_URL: {e}"))?;
    base.join(path).map(|url| url.to_string()).map_err(|e| {
        format!(
            "could not resolve {path:?} against {base}: {e}",
            base = base.as_str()
        )
    })
}

pub struct NativeTransport;

impl Transport for NativeTransport {
    async fn request(spec: RequestSpec) -> Result<TransportResponse, String> {
        request_with_base(&base_url(), spec).await
    }
}

/// The actual round trip, taking `base` explicitly so **bite (a)**
/// (S13a-01's `## Results`) can point it at a local server that always
/// answers 500 without touching the real `BULLPEN_URL` or racing other
/// tests over a shared env var.
///
/// Mirrors `gloo-net`'s own contract (`web.rs`): this returns `Ok` for any
/// completed HTTP response, including a 4xx/5xx - only a genuine transport
/// failure (DNS, connection refused, TLS) is `Err`. Every `api.rs` route
/// function already handles a non-2xx status itself
/// (`if !resp.ok() { return Err(...) }`), so the status must reach that
/// code rather than being turned into a transport-level error here.
async fn request_with_base(base: &str, spec: RequestSpec) -> Result<TransportResponse, String> {
    let url = resolve_url_with_base(base, &spec.url)?;
    let mut builder = match spec.method {
        Method::Get => client().get(&url),
        Method::Post => client().post(&url),
        Method::Put => client().put(&url),
        Method::Patch => client().patch(&url),
        Method::Delete => client().delete(&url),
    };
    if let Some(bytes) = spec.body {
        builder = builder
            .header("content-type", "application/json")
            .body(bytes);
    }
    // `spec.with_credentials` is a web-only opt-in - see
    // `Request::with_credentials`'s doc. `client()`'s cookie store above
    // already carries the session cookie on every request regardless, so
    // there is nothing more to set here.
    let resp = builder.send().await.map_err(|e| e.to_string())?;
    let status = resp.status().as_u16();
    let content_type = resp
        .headers()
        .get(reqwest::header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .map(str::to_string);
    Ok(TransportResponse {
        status,
        content_type,
        body: NativeBody::new(resp),
    })
}

/// A `reqwest::Response`'s body, read one chunk at a time - native's twin
/// of `web.rs::WebBody`'s `ReadableStreamDefaultReader` loop, driving the
/// same `api.rs::feed`/`send_message` SSE parser and `events.rs`'s native
/// `run()`.
pub struct NativeBody {
    inner: Option<reqwest::Response>,
}

impl NativeBody {
    fn new(resp: reqwest::Response) -> Self {
        Self { inner: Some(resp) }
    }

    pub async fn next_chunk(&mut self) -> Result<Option<Vec<u8>>, String> {
        let Some(resp) = self.inner.as_mut() else {
            return Ok(None);
        };
        match resp.chunk().await.map_err(|e| e.to_string())? {
            Some(bytes) => Ok(Some(bytes.to_vec())),
            None => {
                self.inner = None;
                Ok(None)
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{Read, Write};
    use std::net::TcpListener;

    /// A one-shot HTTP/1.1 server that always answers 500, so a test can
    /// point the native transport at a real failing endpoint instead of
    /// mocking `reqwest` itself. Returns the base URL to hit.
    fn spawn_500_server() -> String {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind a local test port");
        let addr = listener.local_addr().expect("a bound listener has an addr");
        std::thread::spawn(move || {
            if let Ok((mut stream, _)) = listener.accept() {
                let mut buf = [0u8; 1024];
                let _ = stream.read(&mut buf); // drain the request, ignore its content
                let body = b"synthetic failure for S13a-01's bite (a)";
                let response = format!(
                    "HTTP/1.1 500 Internal Server Error\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                    body.len()
                );
                let _ = stream.write_all(response.as_bytes());
                let _ = stream.write_all(body);
            }
        });
        format!("http://{addr}")
    }

    /// **Bite (a).** A 500 must reach the caller as `Err`, carrying the
    /// status - the same shape every `api.rs` route function already
    /// builds (`format!("{url} -> {}", resp.status())`) - not vanish into
    /// an `Ok`-with-empty-body or a swallowed default the way
    /// `api.rs::fetch_bot_tools` deliberately swallows a 404 into
    /// `Vec::new()` today. This asserts the *shape a caller sees*, which is
    /// what would go wrong if a native reqwest client were built with
    /// `.error_for_status()` (turns every non-2xx into an opaque transport
    /// error that has already lost the status by the time `api.rs` sees
    /// it) or with the status check inverted.
    #[tokio::test]
    async fn a_500_response_reaches_the_caller_as_an_error_with_its_status() {
        let base = spawn_500_server();
        let spec = RequestSpec {
            method: Method::Get,
            url: "/api/rooms".to_string(),
            body: None,
            with_credentials: false,
        };
        let resp = request_with_base(&base, spec)
            .await
            .expect("a completed HTTP response is Ok even when the status is 500");
        assert!(!resp.ok_for_test());
        let caller_error = format!("/api/rooms -> {}", resp.status);
        assert_eq!(caller_error, "/api/rooms -> 500");
    }

    /// **Bite (b).** Swap `resolve_url_with_base`'s body for
    /// `Ok(path.to_string())` (dropping `base`) to see this go red - the
    /// resolved URL is then a bare path (`/api/rooms`), not an absolute
    /// URL `reqwest` can even send.
    #[test]
    fn resolve_url_joins_the_base_and_the_path() {
        assert_eq!(
            resolve_url_with_base("http://100.119.100.103:4380", "/api/rooms").unwrap(),
            "http://100.119.100.103:4380/api/rooms"
        );
        assert_eq!(
            resolve_url_with_base("http://100.119.100.103:4380/", "/api/bots/b1/seen").unwrap(),
            "http://100.119.100.103:4380/api/bots/b1/seen"
        );
    }

    impl TransportResponse {
        /// Test-only mirror of `transport::Response::ok` - `TransportResponse`
        /// itself is the pre-wrap shape `NativeTransport::request` returns,
        /// with no `ok()` of its own since only `mod.rs`'s `Response` is
        /// public API.
        fn ok_for_test(&self) -> bool {
            (200..300).contains(&self.status)
        }
    }
}
