//! S13b-01's two bites for [`super::attempt_silent_sign_in`] - see that
//! function's doc, and `desktop.rs::attempt_silent_sign_in_before_launch`
//! for how this plugs into the real desktop startup path. A new file
//! (rather than folding into `native.rs`'s own `mod tests`) per this
//! ticket's "Owns: ... and a new test file".
//!
//! Nested into `native.rs` via `#[path = "silent_sign_in_tests.rs"] mod
//! silent_sign_in_tests;`, so this is still a child of `native`'s own
//! module scope - `use super::*` below reaches every private item the
//! same way `native.rs`'s own inline `mod tests` already does
//! (`password_key_path`, `attempt_silent_sign_in`, `SilentSignInOutcome`,
//! `resolve_url_with_base` are all crate-private, never part of the public
//! `transport` API surface).
//!
//! Neither test ever reads or writes Josh's real
//! `%USERPROFILE%\.bullpen\password.key` - each writes its own throwaway
//! key file under the OS temp directory and points `attempt_silent_sign_in`
//! at that file's path directly.

use super::*;
use std::io::{Read, Write};
use std::net::TcpListener;
use std::path::PathBuf;
use std::time::Duration;

/// A throwaway directory under the OS temp root, unique per call so
/// parallel tests never collide. No `tempfile` dependency for two tests;
/// left on disk afterward (a few bytes in the OS temp directory, which the
/// OS itself reclaims) rather than adding delete logic that a test file has
/// no real need for.
fn unique_temp_dir(label: &str) -> PathBuf {
    let unique = format!(
        "bullpen-rs-{label}-{pid}-{nanos}",
        pid = std::process::id(),
        nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("the clock is after 1970")
            .as_nanos()
    );
    let dir = std::env::temp_dir().join(unique);
    std::fs::create_dir_all(&dir).expect("create a throwaway test dir under the OS temp root");
    dir
}

fn write_password_file(dir: &std::path::Path, password: &str) -> PathBuf {
    let path = dir.join("password.key");
    std::fs::write(&path, password).expect("write a throwaway test key file");
    path
}

/// A one-shot HTTP/1.1 server answering `POST /api/auth/login` with 401 and
/// the server's own real error body (`routes/auth.rs`'s literal
/// `{"error": "That password is not right."}`), so this test exercises the
/// exact shape a real wrong password produces.
fn spawn_wrong_password_server() -> String {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind a local test port");
    let addr = listener.local_addr().expect("a bound listener has an addr");
    std::thread::spawn(move || {
        if let Ok((mut stream, _)) = listener.accept() {
            let mut buf = [0u8; 4096];
            // Drains the request - the wrong password sent as this test's
            // input lives only in this buffer, on loopback, and is never
            // inspected or logged.
            let _ = stream.read(&mut buf);
            let body = br#"{"error":"That password is not right."}"#;
            let response = format!(
                "HTTP/1.1 401 Unauthorized\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                body.len()
            );
            let _ = stream.write_all(response.as_bytes());
            let _ = stream.write_all(body);
        }
    });
    format!("http://{addr}")
}

/// Accepts a connection and then never answers it - the "what if the guard
/// that skips the network call is gone" canary for bite (b): contacting
/// this at all would hang forever, which is exactly what the guard being
/// present must prevent.
fn spawn_black_hole_server() -> String {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind a local test port");
    let addr = listener.local_addr().expect("a bound listener has an addr");
    std::thread::spawn(move || {
        if let Ok((_stream, _)) = listener.accept() {
            // Hold the connection open, answering nothing, for as long as
            // the test process lives - the test's own `tokio::time::timeout`
            // is what ends the wait, never this thread.
            std::thread::sleep(Duration::from_secs(600));
        }
    });
    format!("http://{addr}")
}

/// **Two worlds.** Guard present: a wrong password reaches the fake server,
/// which answers 401 with `{"error":"That password is not right."}`, and
/// `attempt_silent_sign_in` returns that exact string as `Err`. Guard
/// removed: a status-check bug that treats any completed HTTP response as
/// success (the same class of mistake S13a-01's own bite (a) guarded
/// against one layer down, in `request_with_base`) would instead return
/// `Ok(SignedIn)` here - silently treating a rejected password as a working
/// session, which is precisely "showing an empty roster" instead of the
/// real problem. The observable that differs: the `Result` variant, and on
/// `Err`, its exact text.
///
/// **Mutation run (captured for `## Results`, then reverted by re-editing -
/// never `git checkout`):** changed `attempt_silent_sign_in`'s
/// `if status.is_success()` check to `if true`, so the 401 above was
/// treated as a signed-in session and the code tried to decode its body as
/// the success shape (`{"token": ...}`) instead of the error shape
/// (`{"error": ...}`) - it does not even have a `token` field, so this
/// produced a body-decode `Err`, not the real rejection message, going red
/// exactly the same either way: the caller never sees
/// "That password is not right.".
#[tokio::test]
async fn wrong_password_surfaces_the_servers_own_rejection() {
    let dir = unique_temp_dir("wrong-password");
    let key_path = write_password_file(&dir, "not-the-real-password");
    let base = spawn_wrong_password_server();

    let result = attempt_silent_sign_in(&base, &key_path).await;

    assert_eq!(result, Err("That password is not right.".to_string()));
}

/// **Two worlds.** Guard present: `attempt_silent_sign_in` checks
/// `path.exists()` before doing anything else, so with no key file it
/// returns `Ok(NoKeyFile)` immediately and never opens a connection - safe
/// even if the server is completely unreachable, which is what lets
/// `desktop.rs` always proceed to open the window (the sign-in gate then
/// renders normally). Guard removed: drop that early check, as a future
/// edit might "simplify" it away, and the function falls through to
/// reading the missing file and - because nothing then stops it - on to an
/// actual network call. Pointed at a server that accepts a connection and
/// never answers, that call hangs forever; wrapping the whole attempt in a
/// short `tokio::time::timeout` turns "the desktop app would hang on
/// launch with no key file" into a fast, provable red instead of an
/// actually-frozen test run.
///
/// **Mutation run (captured for `## Results`, then reverted by re-editing -
/// never `git checkout`):** removed `attempt_silent_sign_in`'s two early
/// returns (`path.exists()` and `password.is_empty()`) and replaced the
/// file read with `.unwrap_or_default()`, so a missing key file fell
/// through to an unconditional network call with an empty password.
#[tokio::test]
async fn missing_key_file_returns_immediately_without_touching_the_network() {
    let dir = unique_temp_dir("missing-key");
    let missing_key_path = dir.join("password.key");
    assert!(!missing_key_path.exists());
    let base = spawn_black_hole_server();

    let result = tokio::time::timeout(
        Duration::from_millis(500),
        attempt_silent_sign_in(&base, &missing_key_path),
    )
    .await
    .expect("must return immediately - no key file means no network call, never a hang");

    assert_eq!(result, Ok(SilentSignInOutcome::NoKeyFile));
}
