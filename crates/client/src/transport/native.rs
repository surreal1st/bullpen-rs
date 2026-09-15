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
//!
//! **S13b-01: silent sign-in.** The above covers a session surviving
//! *within* one process; it does nothing for the session surviving *across
//! launches*, so the desktop build showed the sign-in gate every single
//! time - strictly worse than the Electron shell it replaces, which signs
//! in from `%USERPROFILE%\.bullpen\password.key`
//! (`projects/bullpen/desktop/main.cjs::signIn`) before its window ever
//! loads a page. [`silent_sign_in`] ports that: read the same file, POST it
//! to `/api/auth/login`, and if it is accepted, remember the returned
//! session token for every later request this process makes
//! ([`SESSION_TOKEN`], attached in [`request_with_base`] below).
//!
//! **Re-authenticate every launch, not persist-to-disk.** The Electron app
//! itself does not persist a session either - `main.cjs::signIn` runs fresh
//! on every start, reading the password file each time rather than caching
//! a token between runs. Copying that (rather than writing a new session
//! file to disk) means no new secret-bearing artifact to protect beyond
//! `password.key` itself, and no stale-session edge case: a signed-in
//! session outlives nothing past process exit, so there is nothing to
//! expire, revoke, or clean up later.
//!
//! **Why a bearer token, not the shared cookie jar, carries the result.**
//! `desktop.rs::attempt_silent_sign_in_before_launch` calls this before
//! `dioxus-desktop`'s own long-lived Tokio runtime exists, from a
//! short-lived throwaway runtime built just for that one call (there is no
//! async context to `.await` in yet at that point in `main()`). A
//! `reqwest::Client`'s pooled HTTP/1.1 connections are driven by a
//! background task tied to whichever Tokio runtime was current when the
//! connection was opened; handing the *shared* `client()` singleton's
//! cookie-bearing connections across to a runtime that then gets dropped
//! would leave the app's very first *real* request (`app.rs`'s own
//! `auth_status()` call, moments later, inside the real runtime) trying to
//! drive a connection nothing is polling anymore. `silent_sign_in` sidesteps
//! this entirely: it builds and uses its own one-off `reqwest::Client` for
//! the login POST alone, and only a plain `String` token - which has no
//! runtime affinity at all - crosses out of that throwaway runtime. `login`
//! (`api.rs`, the interactive password-box path) is unaffected: it already
//! only ever runs inside the one real runtime, so its cookie-store approach
//! stays exactly as it was. The server accepts either (`auth::presented_token`
//! checks `Authorization: Bearer` before falling back to the cookie), so
//! this is additive, not a second competing mechanism.

use super::{Method, RequestSpec, Transport, TransportResponse};
#[cfg(feature = "desktop")]
use serde::Deserialize;
#[cfg(feature = "desktop")]
use std::path::{Path, PathBuf};
#[cfg(feature = "desktop")]
use std::sync::Mutex;
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

/// The token a successful [`silent_sign_in`] obtained, if any - see this
/// module's top doc ("why a bearer token, not the shared cookie jar") for
/// why this, rather than `client()`'s own cookie jar, is what carries a
/// silent sign-in's result. `None` until a silent sign-in succeeds; never
/// written anywhere else, and never logged or displayed - only ever read
/// back into an `Authorization` header in [`request_with_base`].
///
/// `feature = "desktop"`-gated, like the rest of this section: silent
/// sign-in only ever has one caller (`desktop.rs`), itself gated the same
/// way in `main.rs` - without this, `cargo check -p client` on the default
/// `web` feature (still native-target, since only `target_arch` gates this
/// whole file - see this module's top doc) would compile this with no
/// caller anywhere in the crate and warn every one of these items dead,
/// which `cargo clippy -D warnings` then turns into a build failure.
#[cfg(feature = "desktop")]
static SESSION_TOKEN: Mutex<Option<String>> = Mutex::new(None);

#[cfg(feature = "desktop")]
fn stored_session_token() -> Option<String> {
    SESSION_TOKEN
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .clone()
}

#[cfg(feature = "desktop")]
fn store_session_token(token: String) {
    *SESSION_TOKEN
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner()) = Some(token);
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
    // S13b-01: a silent sign-in's result rides in here, not in `client()`'s
    // cookie jar - see this module's top doc. Attached unconditionally,
    // same as the cookie store already is, regardless of `with_credentials`.
    #[cfg(feature = "desktop")]
    if let Some(token) = stored_session_token() {
        builder = builder.bearer_auth(token);
    }
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

/* -------------------------------------------------------------- S13b-01 */
// Every item below is `feature = "desktop"`-gated - see `SESSION_TOKEN`'s
// doc above for why.

/// What a silent sign-in attempt found. `NoKeyFile` covers both "the file
/// does not exist" and "it exists but is empty/whitespace" - both mean
/// "there is nothing to sign in with", the same outcome either way: fall
/// back to the sign-in gate that already works, quietly.
#[cfg(feature = "desktop")]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SilentSignInOutcome {
    SignedIn,
    NoKeyFile,
}

#[cfg(feature = "desktop")]
#[derive(Deserialize)]
struct SignInOk {
    token: String,
}

#[cfg(feature = "desktop")]
#[derive(Deserialize)]
struct SignInError {
    error: String,
}

/// Where the Electron app's own silent sign-in reads its credential from
/// (`projects/bullpen/desktop/main.cjs::signIn`, `deploy/Bullpen-rs.cmd`'s
/// own comment): `%USERPROFILE%\.bullpen\password.key`. `BULLPEN_PASSWORD_FILE`
/// overrides it - mirrors `main.cjs`'s own env override - purely so a test
/// can point this at a disposable file instead of Josh's real one; nothing
/// in this codebase sets it otherwise.
#[cfg(feature = "desktop")]
fn password_key_path() -> Option<PathBuf> {
    if let Ok(overridden) = std::env::var("BULLPEN_PASSWORD_FILE") {
        return Some(PathBuf::from(overridden));
    }
    std::env::var("USERPROFILE")
        .ok()
        .map(|home| Path::new(&home).join(".bullpen").join("password.key"))
}

/// The real entry point: `desktop.rs::attempt_silent_sign_in_before_launch`
/// calls this once at startup, before the window opens. Never reads or logs
/// the key's contents outside [`attempt_silent_sign_in`] itself, and never
/// puts them in the `Result`'s `Err` - see this module's top doc and the
/// crate's own secret-handling rule.
#[cfg(feature = "desktop")]
pub async fn silent_sign_in() -> Result<SilentSignInOutcome, String> {
    let Some(path) = password_key_path() else {
        return Ok(SilentSignInOutcome::NoKeyFile);
    };
    attempt_silent_sign_in(&base_url(), &path).await
}

/// The testable half of [`silent_sign_in`], taking `base` and `path`
/// explicitly - same reasoning as [`request_with_base`]/
/// [`resolve_url_with_base`] above: a test can point this at a local fake
/// server and a throwaway key file without touching `BULLPEN_URL` or
/// Josh's real `password.key`, and without racing another test over either.
///
/// On any failure below - a read error, an empty file, a rejected password,
/// a network error - the `Err`/`NoKeyFile` returned names only the file's
/// PATH and the failure KIND (an `io::ErrorKind`, or the server's own error
/// message, which the server itself never echoes the password into - see
/// `routes/auth.rs::login`). The password itself lives only in the local
/// `password` binding below, used once to build the request body, never
/// formatted into any `Err`, log line, URL, or header.
#[cfg(feature = "desktop")]
async fn attempt_silent_sign_in(base: &str, path: &Path) -> Result<SilentSignInOutcome, String> {
    if !path.exists() {
        return Ok(SilentSignInOutcome::NoKeyFile);
    }
    let contents = std::fs::read_to_string(path).map_err(|e| {
        format!(
            "could not read {path}: {kind:?}",
            path = path.display(),
            kind = e.kind()
        )
    })?;
    let password = contents.trim_end_matches(['\r', '\n']);
    if password.is_empty() {
        return Ok(SilentSignInOutcome::NoKeyFile);
    }

    let url = resolve_url_with_base(base, "/api/auth/login")?;
    let body = serde_json::to_vec(&serde_json::json!({ "password": password }))
        .map_err(|e| e.to_string())?;

    // A one-off client, deliberately not the shared `client()` above - see
    // this module's top doc ("why a bearer token, not the shared cookie
    // jar carries the result").
    let login_client = reqwest::Client::builder()
        .use_rustls_tls()
        .build()
        .map_err(|e| e.to_string())?;
    let resp = login_client
        .post(&url)
        .header("content-type", "application/json")
        .body(body)
        .send()
        .await
        .map_err(|e| e.to_string())?;

    let status = resp.status();
    if status.is_success() {
        let ok: SignInOk = resp.json().await.map_err(|e| e.to_string())?;
        store_session_token(ok.token);
        return Ok(SilentSignInOutcome::SignedIn);
    }
    let message = match resp.json::<SignInError>().await {
        Ok(err) => err.error,
        Err(_) => format!("sign-in failed with status {status}"),
    };
    Err(message)
}

#[cfg(all(test, feature = "desktop"))]
#[path = "silent_sign_in_tests.rs"]
mod silent_sign_in_tests;

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
