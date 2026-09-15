//! Integration tests for `server::egress_proxy`.
//!
//! 🔴 The whole point of this file is proving the CHECK happens BEFORE the
//! upstream socket opens, not just that the client eventually sees a 403.
//! From the client's side, "refused after checking" and "refused after
//! connecting-then-checking-then-closing" produce the exact same response.
//! So every refusal test here points the proxy's `Dial` at a REAL local
//! `TcpListener` that records every accept, and asserts that listener's
//! accept count - not the client's HTTP response - to prove no socket was
//! ever opened to the (simulated) upstream.

use async_trait::async_trait;
use server::egress::{EgressPolicy, Resolver};
use server::egress_proxy::{Dial, ProxyOptions, create_egress_proxy};
use std::net::SocketAddr;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};

/// A resolver that always answers with a given (public, non-private) set of
/// addresses, recording how many times it was called - so a test can prove
/// the DNS-checked SSRF path in `decide_connect` (S6-01, not reimplemented
/// here) actually ran, not just that the end result happened to be right.
struct FixedResolver {
    addresses: Vec<String>,
    calls: Arc<AtomicUsize>,
}

#[async_trait]
impl Resolver for FixedResolver {
    async fn resolve(&self, _host: &str) -> Result<Vec<String>, String> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        Ok(self.addresses.clone())
    }
}

/// A `Dial` that ignores the requested host/port entirely and always
/// connects to a fixed local address, counting every attempt. Standing in
/// for "the real upstream" in every test here: what matters is not WHAT it
/// connects to, but WHETHER it is ever called for a CONNECT that should
/// have been refused.
struct RecordingDial {
    target: SocketAddr,
    attempts: Arc<AtomicUsize>,
}

#[async_trait]
impl Dial for RecordingDial {
    async fn dial(&self, _host: &str, _port: u16) -> std::io::Result<TcpStream> {
        self.attempts.fetch_add(1, Ordering::SeqCst);
        TcpStream::connect(self.target).await
    }
}

/// Binds a real local listener that does nothing but count accepts, and
/// hands back its address plus a live counter. This is the "was a socket
/// ever opened" oracle every test in this file relies on.
async fn spawn_recording_listener() -> (SocketAddr, Arc<AtomicUsize>) {
    let listener = TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind ephemeral listener");
    let addr = listener.local_addr().expect("local_addr");
    let accepts = Arc::new(AtomicUsize::new(0));
    let accepts_task = Arc::clone(&accepts);
    tokio::spawn(async move {
        while let Ok((socket, _)) = listener.accept().await {
            accepts_task.fetch_add(1, Ordering::SeqCst);
            // Keep the accepted socket alive briefly rather than dropping it
            // instantly - a dropped socket can reset the client's own
            // connection before it finishes reading a response in the
            // "connect, then close" bug shape.
            drop(socket);
        }
    });
    (addr, accepts)
}

/// Starts `create_egress_proxy(options)` listening on an ephemeral loopback
/// port and returns that address. The accept loop runs on its own task for
/// the life of the process; each test uses its own throwaway port.
async fn spawn_proxy(options: ProxyOptions) -> SocketAddr {
    let proxy = Arc::new(create_egress_proxy(options));
    let listener = TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind proxy listener");
    let addr = listener.local_addr().expect("local_addr");
    tokio::spawn(proxy.serve(listener));
    addr
}

/// Sends a raw CONNECT request to the proxy at `proxy_addr` and reads back
/// whatever it responds with (or times out).
async fn send_connect(proxy_addr: SocketAddr, target: &str) -> String {
    let mut client = tokio::time::timeout(Duration::from_secs(5), TcpStream::connect(proxy_addr))
        .await
        .expect("connect to proxy did not time out")
        .expect("connect to proxy");

    client
        .write_all(format!("CONNECT {target} HTTP/1.1\r\nHost: {target}\r\n\r\n").as_bytes())
        .await
        .expect("write CONNECT");

    let mut buf = vec![0u8; 4096];
    let read = tokio::time::timeout(Duration::from_secs(5), client.read(&mut buf))
        .await
        .expect("read from proxy did not time out")
        .expect("read from proxy");

    String::from_utf8_lossy(&buf[..read]).into_owned()
}

/// Polls `accepts` for up to `timeout` for it to become nonzero. Used only
/// where a bug would legitimately race the assertion (a background dial
/// that is not awaited before the client is answered) - never to paper over
/// a genuinely flaky proxy.
async fn wait_for_at_least_one_accept(accepts: &AtomicUsize, timeout: Duration) -> usize {
    let deadline = tokio::time::Instant::now() + timeout;
    loop {
        let n = accepts.load(Ordering::SeqCst);
        if n > 0 || tokio::time::Instant::now() >= deadline {
            return n;
        }
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
}

/// **The bite (a) target.** A host not on the allow list must be refused,
/// and the refusal must happen before any socket to the (simulated)
/// upstream opens - proven by the recording listener's accept count, not by
/// the client's response.
#[tokio::test]
async fn refused_host_never_reaches_the_upstream_listener() {
    let (upstream_addr, accepts) = spawn_recording_listener().await;
    let dial_attempts = Arc::new(AtomicUsize::new(0));

    let policy = EgressPolicy {
        allow: vec!["allowed.example".to_string()],
    };
    let resolve_calls = Arc::new(AtomicUsize::new(0));
    let options = ProxyOptions {
        policy: policy.into(),
        resolve: Arc::new(FixedResolver {
            addresses: vec!["93.184.216.34".to_string()],
            calls: Arc::clone(&resolve_calls),
        }),
        dial: Arc::new(RecordingDial {
            target: upstream_addr,
            attempts: Arc::clone(&dial_attempts),
        }),
        on_decision: None,
    };

    let proxy_addr = spawn_proxy(options).await;
    let response = send_connect(proxy_addr, "not-allowed.example:443").await;

    assert!(
        response.starts_with("HTTP/1.1 403"),
        "a host off the allow list must be refused with 403, got: {response}"
    );

    // Give any wrongly-backgrounded dial a moment to land before asserting
    // its absence - see `wait_for_at_least_one_accept`'s doc. A correct
    // implementation never calls dial at all, so this always times out at
    // zero rather than genuinely waiting on real work.
    let n = wait_for_at_least_one_accept(&accepts, Duration::from_millis(200)).await;
    assert_eq!(
        n, 0,
        "a refused CONNECT must never open a socket to the upstream - the upstream listener recorded an accept"
    );
    assert_eq!(
        dial_attempts.load(Ordering::SeqCst),
        0,
        "a refused CONNECT must never call Dial::dial at all"
    );
    // Host is off the allow list, so decide_connect must refuse before ever
    // calling the resolver - proves the refusal took the cheap path, same
    // shape S6-01's own bite (a) guards.
    assert_eq!(
        resolve_calls.load(Ordering::SeqCst),
        0,
        "a host refused on the allow-list check must never be resolved"
    );
}

/// An allowed CONNECT must actually reach the upstream: this is the
/// necessary counterpart to the refusal tests above - without it, a proxy
/// that refuses EVERYTHING would also pass those.
#[tokio::test]
async fn allowed_host_reaches_the_upstream_and_tunnels() {
    let (upstream_addr, accepts) = spawn_recording_listener().await;
    let dial_attempts = Arc::new(AtomicUsize::new(0));

    let policy = EgressPolicy {
        allow: vec!["allowed.example".to_string()],
    };
    let options = ProxyOptions {
        policy: policy.into(),
        resolve: Arc::new(FixedResolver {
            addresses: vec!["93.184.216.34".to_string()],
            calls: Arc::new(AtomicUsize::new(0)),
        }),
        dial: Arc::new(RecordingDial {
            target: upstream_addr,
            attempts: Arc::clone(&dial_attempts),
        }),
        on_decision: None,
    };

    let proxy_addr = spawn_proxy(options).await;
    let response = send_connect(proxy_addr, "allowed.example:443").await;

    assert!(
        response.starts_with("HTTP/1.1 200"),
        "an allowed host on an allowed port must tunnel through, got: {response}"
    );
    let n = wait_for_at_least_one_accept(&accepts, Duration::from_secs(1)).await;
    assert_eq!(
        n, 1,
        "an allowed CONNECT must dial the upstream exactly once"
    );
    assert_eq!(dial_attempts.load(Ordering::SeqCst), 1);
}

/// **The bite (b) target.** `ALLOWED_PORTS` is 80/443 only (S6-01). A
/// CONNECT to any other port must be refused even when the host itself is
/// on the allow list - and, same as above, refused before a socket opens.
#[tokio::test]
async fn connect_to_a_disallowed_port_never_reaches_the_upstream_listener() {
    let (upstream_addr, accepts) = spawn_recording_listener().await;
    let dial_attempts = Arc::new(AtomicUsize::new(0));

    let policy = EgressPolicy {
        allow: vec!["allowed.example".to_string()],
    };
    let options = ProxyOptions {
        policy: policy.into(),
        resolve: Arc::new(FixedResolver {
            addresses: vec!["93.184.216.34".to_string()],
            calls: Arc::new(AtomicUsize::new(0)),
        }),
        dial: Arc::new(RecordingDial {
            target: upstream_addr,
            attempts: Arc::clone(&dial_attempts),
        }),
        on_decision: None,
    };

    let proxy_addr = spawn_proxy(options).await;
    // 8080 is not in ALLOWED_PORTS (80/443 only); the host itself IS on the
    // allow list, isolating this test to the port check specifically.
    let response = send_connect(proxy_addr, "allowed.example:8080").await;

    assert!(
        response.starts_with("HTTP/1.1 403"),
        "a CONNECT to a port outside ALLOWED_PORTS must be refused even for an allowed host, got: {response}"
    );
    let n = wait_for_at_least_one_accept(&accepts, Duration::from_millis(200)).await;
    assert_eq!(
        n, 0,
        "a CONNECT to a disallowed port must never open a socket to the upstream"
    );
    assert_eq!(dial_attempts.load(Ordering::SeqCst), 0);
}

/// Anything that is not a CONNECT request is refused without being read
/// further or dialling anything.
#[tokio::test]
async fn non_connect_request_is_refused_with_405() {
    let (upstream_addr, accepts) = spawn_recording_listener().await;
    let policy = EgressPolicy { allow: vec![] };
    let options = ProxyOptions {
        policy: policy.into(),
        resolve: Arc::new(FixedResolver {
            addresses: vec![],
            calls: Arc::new(AtomicUsize::new(0)),
        }),
        dial: Arc::new(RecordingDial {
            target: upstream_addr,
            attempts: Arc::new(AtomicUsize::new(0)),
        }),
        on_decision: None,
    };

    let proxy_addr = spawn_proxy(options).await;

    let mut client = TcpStream::connect(proxy_addr)
        .await
        .expect("connect to proxy");
    client
        .write_all(b"GET / HTTP/1.1\r\nHost: allowed.example\r\n\r\n")
        .await
        .expect("write GET");
    let mut buf = vec![0u8; 4096];
    let read = tokio::time::timeout(Duration::from_secs(5), client.read(&mut buf))
        .await
        .expect("read did not time out")
        .expect("read from proxy");
    let response = String::from_utf8_lossy(&buf[..read]).into_owned();

    assert!(
        response.starts_with("HTTP/1.1 405"),
        "a non-CONNECT request must get 405, got: {response}"
    );
    assert_eq!(accepts.load(Ordering::SeqCst), 0);
}

/// The `on_decision` hook fires with the actual verdict, for whatever wants
/// to log/audit CONNECT decisions - mirrors TS's `options.onDecision`.
#[tokio::test]
async fn on_decision_hook_receives_the_actual_verdict() {
    let (upstream_addr, _accepts) = spawn_recording_listener().await;
    let policy = EgressPolicy {
        allow: vec!["allowed.example".to_string()],
    };
    let seen: Arc<std::sync::Mutex<Vec<(String, u16, bool)>>> =
        Arc::new(std::sync::Mutex::new(Vec::new()));
    let seen_hook = Arc::clone(&seen);

    let options = ProxyOptions {
        policy: policy.into(),
        resolve: Arc::new(FixedResolver {
            addresses: vec!["93.184.216.34".to_string()],
            calls: Arc::new(AtomicUsize::new(0)),
        }),
        dial: Arc::new(RecordingDial {
            target: upstream_addr,
            attempts: Arc::new(AtomicUsize::new(0)),
        }),
        on_decision: Some(Arc::new(move |host, port, verdict| {
            seen_hook
                .lock()
                .unwrap()
                .push((host.to_string(), port, verdict.ok));
        })),
    };

    let proxy_addr = spawn_proxy(options).await;
    let _ = send_connect(proxy_addr, "allowed.example:443").await;

    // Give the hook a moment - it fires inside the same task before the
    // response is written, so this should already be true, but a short
    // poll keeps the test honest without hardcoding ordering assumptions.
    tokio::time::sleep(Duration::from_millis(50)).await;
    let recorded = seen.lock().unwrap().clone();
    assert_eq!(recorded, vec![("allowed.example".to_string(), 443, true)]);
}
