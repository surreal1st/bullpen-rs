//! The CONNECT proxy a sandboxed bot's traffic is pointed at, so every
//! outbound connection is checked against that bot's policy before a real
//! socket to the target ever opens.
//!
//! Ported from TypeScript `src/server/egress.ts:163-239` (`ProxyOptions`,
//! `createEgressProxy`) only - not `createBotProxyManager`/
//! `ensureBotEgressColumn`/`getBotEgress`/`setBotEgress` (`:240-436`, per-bot
//! wiring, out of this ticket's scope) and not the decision itself
//! (`decide_connect`, `parse_connect`, `EgressPolicy`, `Resolver`), which
//! lives in `crate::egress` (S6-01) and is called here, never reimplemented.
//!
//! HTTPS only, by design: a plain-HTTP proxy would have to read and rewrite
//! requests, and a bot's traffic is not something this should be in the
//! middle of. CONNECT tunnels bytes after the decision is made.
//!
//! 🔴 **THE ORDER IS THE WHOLE POINT.** `handle_client` below must decide
//! before it dials. A proxy that dials, then decides, then closes the
//! connection on a refusal looks IDENTICAL from the client's side to one
//! that refused correctly - same 403, same rough timing, nothing observable
//! differs from outside the proxy. So the test that guards this
//! (`tests/egress_proxy.rs`) does not watch the client's view at all: it
//! points `dial` at a real local TCP listener that records every accept,
//! and asserts that listener saw zero accepts for a refused CONNECT. A test
//! that only checked the client's response would pass against the ordering
//! bug just as happily as against the correct code - see S6's own header on
//! the S6-lite F18 trap and `[[feedback_prove_the_test_bites]]` shape (a).

use crate::egress::{self, EgressPolicy, Resolver, Verdict};
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};

/// Where `EgressProxy::serve` reads the policy to enforce from: fixed for
/// the lifetime of the proxy, or re-read fresh on every CONNECT.
///
/// 🔴 The dynamic form is what a per-bot proxy manager (out of this
/// ticket's scope, TS's `createBotProxyManager`) would use: a bot's allow
/// list can change while its proxy is already running - a background job
/// started five minutes ago, or simply Josh editing the list - and
/// re-reading on every connection means that edit takes effect on the NEXT
/// request through the same port, rather than needing the proxy torn down
/// and rebuilt.
pub enum PolicySource {
    Fixed(EgressPolicy),
    Dynamic(Box<dyn Fn() -> EgressPolicy + Send + Sync>),
}

impl PolicySource {
    fn current(&self) -> EgressPolicy {
        match self {
            PolicySource::Fixed(policy) => policy.clone(),
            PolicySource::Dynamic(f) => f(),
        }
    }
}

impl From<EgressPolicy> for PolicySource {
    fn from(policy: EgressPolicy) -> Self {
        PolicySource::Fixed(policy)
    }
}

/// Opens the upstream connection for an ALLOWED CONNECT. Overridable so a
/// test can assert what would be dialled without dialling the real target -
/// or, for the "never opened a socket" bite test, point it at a local
/// recording listener instead of a real host, so a refused CONNECT that
/// wrongly reaches this trait object is caught even though nothing in the
/// test touches the network the bot would actually be trying to reach.
#[async_trait::async_trait]
pub trait Dial: Send + Sync {
    async fn dial(&self, host: &str, port: u16) -> std::io::Result<TcpStream>;
}

/// The real dialer: a plain TCP connect. TLS (when the tunnelled traffic is
/// HTTPS, the expected case) is negotiated end-to-end between the client and
/// the upstream through the tunnel; this proxy only ever sees ciphertext.
pub struct RealDial;

#[async_trait::async_trait]
impl Dial for RealDial {
    async fn dial(&self, host: &str, port: u16) -> std::io::Result<TcpStream> {
        TcpStream::connect((host, port)).await
    }
}

type DecisionHook = dyn Fn(&str, u16, &Verdict) + Send + Sync;

/// What `create_egress_proxy` needs. Mirrors TS's `ProxyOptions`.
pub struct ProxyOptions {
    pub policy: PolicySource,
    pub resolve: Arc<dyn Resolver>,
    pub dial: Arc<dyn Dial>,
    pub on_decision: Option<Arc<DecisionHook>>,
}

impl ProxyOptions {
    /// The common case: a fixed policy, the real resolver, the real dialer,
    /// no decision hook.
    pub fn new(policy: EgressPolicy, resolve: Arc<dyn Resolver>) -> Self {
        ProxyOptions {
            policy: PolicySource::Fixed(policy),
            resolve,
            dial: Arc::new(RealDial),
            on_decision: None,
        }
    }
}

/// A CONNECT proxy that refuses everything not on its policy's list.
///
/// Mirrors TS's `createEgressProxy` returning a `Server` that is not yet
/// listening: `create_egress_proxy` builds the proxy; the caller binds a
/// real `TcpListener` (port 0 for an ephemeral port, same as TS's
/// `server.listen(0, bindHost, ...)`) and hands it to `serve` separately.
pub struct EgressProxy {
    policy: PolicySource,
    resolve: Arc<dyn Resolver>,
    dial: Arc<dyn Dial>,
    on_decision: Option<Arc<DecisionHook>>,
}

/// Builds a CONNECT proxy from `options`. Does not bind or listen - see
/// `EgressProxy::serve`.
pub fn create_egress_proxy(options: ProxyOptions) -> EgressProxy {
    EgressProxy {
        policy: options.policy,
        resolve: options.resolve,
        dial: options.dial,
        on_decision: options.on_decision,
    }
}

impl EgressProxy {
    /// Accepts connections from `listener` until it errors, handling each on
    /// its own task. Never returns on a clean run - the caller drives this
    /// inside its own `tokio::spawn`/lifetime management (out of this
    /// ticket's scope: the per-bot manager that would own that).
    pub async fn serve(self: Arc<Self>, listener: TcpListener) {
        loop {
            let (client, _addr) = match listener.accept().await {
                Ok(pair) => pair,
                Err(_) => continue,
            };
            let this = Arc::clone(&self);
            tokio::spawn(async move {
                this.handle_client(client).await;
            });
        }
    }

    /// Handles exactly one client connection: read its first chunk, decide,
    /// then (and only then) dial.
    ///
    /// 🔴 This function's statement order IS the security boundary. Do not
    /// reorder the `decide_connect` call to after `self.dial.dial(...)` -
    /// see the module doc.
    async fn handle_client(&self, mut client: TcpStream) {
        let mut buf = vec![0u8; 8192];
        let n = match client.read(&mut buf).await {
            Ok(0) | Err(_) => return,
            Ok(n) => n,
        };

        // Anything that is not a CONNECT is refused without being read
        // further. This proxy has exactly one verb.
        let head = String::from_utf8_lossy(&buf[..n]).into_owned();
        let Some((host, port)) = egress::parse_connect(&head) else {
            let _ = client
                .write_all(b"HTTP/1.1 405 Method Not Allowed\r\n\r\n")
                .await;
            return;
        };

        let policy = self.policy.current();
        let verdict = egress::decide_connect(&host, port, &policy, self.resolve.as_ref()).await;

        if let Some(hook) = &self.on_decision {
            hook(&host, port, &verdict);
        }

        if !verdict.ok {
            let body = format!(
                "HTTP/1.1 403 Forbidden\r\n\r\nRefused: {}\n",
                verdict.reason
            );
            let _ = client.write_all(body.as_bytes()).await;
            return;
        }

        let upstream = match self.dial.dial(&host, port).await {
            Ok(stream) => stream,
            Err(_) => {
                let _ = client.shutdown().await;
                return;
            }
        };

        if client
            .write_all(b"HTTP/1.1 200 Connection Established\r\n\r\n")
            .await
            .is_err()
        {
            return;
        }

        let mut client = client;
        let mut upstream = upstream;
        let _ = tokio::io::copy_bidirectional(&mut client, &mut upstream).await;
    }
}

/// A fn-backed `Dial`, for callers/tests that want a closure instead of a
/// named type.
pub struct FnDial<F>(pub F)
where
    F: Fn(String, u16) -> Pin<Box<dyn Future<Output = std::io::Result<TcpStream>> + Send>>
        + Send
        + Sync;

#[async_trait::async_trait]
impl<F> Dial for FnDial<F>
where
    F: Fn(String, u16) -> Pin<Box<dyn Future<Output = std::io::Result<TcpStream>> + Send>>
        + Send
        + Sync,
{
    async fn dial(&self, host: &str, port: u16) -> std::io::Result<TcpStream> {
        (self.0)(host.to_string(), port).await
    }
}
