use std::net::SocketAddr;
use std::path::PathBuf;
// Only the `BULLPEN_FAKE_PORT` debug path below uses these, and that path is
// `#[cfg(debug_assertions)]`. Ungated, they are four unused-import warnings in
// the RELEASE build - the build that actually ships, and the one the gate
// never compiles (`cargo clippy`/`cargo test` both run debug), so nothing
// caught them until a ship log did. 2026-09-15.
#[cfg(debug_assertions)]
use std::sync::Arc;
#[cfg(debug_assertions)]
use std::time::Duration;

#[cfg(debug_assertions)]
use model::{EventStream, ModelEvent, ModelPort, ModelRequest};

/// S1-07b: `BULLPEN_FAKE_PORT=1` swaps the real OpenRouter port for one
/// that waits `DELAY` then replies - this environment has no OpenRouter
/// key, and the working bar (`crates/client/src/working_bar.rs`) needs a
/// REAL run in flight against THIS server to screenshot, since
/// `GET /api/conversations/:id/working` only exists here, not in
/// `scripts/mock-roster.mjs`. A local type rather than `model::fake`, whose
/// `FakePort` replays instantly with no delay and is `cfg(test)`-gated -
/// this touches only this one file, nothing in `crates/model`.
/// S8b-F1-01: `BULLPEN_FAKE_PORT=empty` is the same port with `reply: None`.
/// It waits, then completes with no delta and no tool call at all, which is
/// the shape a provider produced when a run settled as a silent empty
/// success. Rendering that fix means driving a REAL run against THIS server,
/// exactly as the working bar did above.
#[cfg(debug_assertions)]
struct DelayedFakePort {
    reply: Option<&'static str>,
    delay: Duration,
}

#[cfg(debug_assertions)]
impl ModelPort for DelayedFakePort {
    fn stream(&self, _request: ModelRequest) -> EventStream {
        let reply = self.reply;
        let delay = self.delay;
        Box::pin(async_stream::stream! {
            tokio::time::sleep(delay).await;
            if let Some(reply) = reply {
                yield ModelEvent::Delta { text: reply.to_string() };
            }
            yield ModelEvent::Done {
                model: "fake/delayed".to_string(),
                usage: None,
                finish_reason: Some("stop".to_string()),
            };
        })
    }
}

#[tokio::main]
async fn main() {
    tracing_subscriber::fmt::init();

    let data_dir = std::env::var("BULLPEN_DATA_DIR").unwrap_or_else(|_| "./data".to_string());
    let port: u16 = std::env::var("BULLPEN_PORT")
        .ok()
        .and_then(|p| p.parse().ok())
        .unwrap_or(4380);

    std::fs::create_dir_all(&data_dir).expect("create data dir");
    let db_path = PathBuf::from(&data_dir).join("bullpen.db");
    let db = store::Db::open(db_path.to_str().expect("data dir path is valid utf-8"))
        .expect("open bullpen.db");

    // Says whether the dir is there, never anything from inside the db.
    tracing::info!(%data_dir, "bullpen data dir");

    // SPEND-01: `BULLPEN_FAKE_CREDITS=<amount>` swaps the real OpenRouter
    // credits reader for one that answers a fixed dollar figure - same
    // reasoning as `BULLPEN_FAKE_PORT` just above: this environment has no
    // OpenRouter key, so `accountReadable` is always false and the Spend
    // panel's main view (the figure, the bar, the per-bot rows) can never
    // be screenshotted without it. Kept to this one file, `#[cfg(debug_
    // assertions)]`-gated the same way, so a release build never carries a
    // way to fake its own balance.
    #[cfg(debug_assertions)]
    let fake_credits: Option<Arc<dyn server::spend::CreditsPort>> =
        std::env::var("BULLPEN_FAKE_CREDITS")
            .ok()
            .and_then(|v| v.parse::<f64>().ok())
            .map(|amount| {
                tracing::info!(
                    amount,
                    "BULLPEN_FAKE_CREDITS: credits reads are scripted, no OpenRouter key needed"
                );
                Arc::new(server::spend::FakeCredits::usage(amount))
                    as Arc<dyn server::spend::CreditsPort>
            });

    #[cfg(debug_assertions)]
    let fake_port: Option<Arc<dyn ModelPort>> = matches!(
        std::env::var("BULLPEN_FAKE_PORT").as_deref(),
        Ok("1") | Ok("empty")
    )
    .then(|| {
        let empty = std::env::var("BULLPEN_FAKE_PORT").as_deref() == Ok("empty");
        tracing::info!(
            empty,
            "BULLPEN_FAKE_PORT: model calls are scripted, no OpenRouter key needed"
        );
        Arc::new(DelayedFakePort {
            reply: if empty {
                None
            } else {
                Some("Working on it - give me a moment.")
            },
            delay: Duration::from_secs(4),
        }) as Arc<dyn ModelPort>
    });

    #[cfg(debug_assertions)]
    let state = match (fake_port, fake_credits) {
        (Some(port), Some(credits)) => server::AppState::with_port_and_credits(db, port, credits),
        (Some(port), None) => server::AppState::with_port(db, port),
        (None, Some(credits)) => server::AppState::with_credits(db, credits),
        (None, None) => server::AppState::new(db),
    };

    #[cfg(not(debug_assertions))]
    let state = server::AppState::new(db);

    // S5-03: the routine scheduler, ticking every 30s - started here so it
    // runs against the same `AppState` (same `Arc<Mutex<Db>>`, same
    // `RunManager`) every route in `app` shares. Cloning `state` before
    // `build_app` consumes it, not after: `build_app` takes it by value.
    let _scheduler = server::routines::start_scheduler(state.clone());
    let _goal_scheduler = server::goals::start_goal_scheduler(state.clone());

    // S6-W-02b: the CONNECT proxy every sandboxed bot's traffic is forced
    // through, started only when egress is enabled. It binds the docker
    // network's GATEWAY, not loopback - a container lives in its own network
    // namespace and can never reach the host's 127.0.0.1, which is why the
    // first version of this wiring was inert. With egress off nothing starts
    // and the sandbox keeps `--network none`, byte-for-byte its old
    // behaviour. A failure here is logged, not fatal: a bot that cannot
    // reach the network is a degraded bot, not a dead server.
    if server::egress::egress_enabled(std::env::var("BULLPEN_SANDBOX_EGRESS").ok().as_deref()) {
        let network = server::sandbox::sandbox_network_name();
        match server::egress_proxy::start_egress_proxy(&network).await {
            Ok((_handle, addr)) => {
                tracing::info!(%addr, %network, "egress proxy listening")
            }
            Err(e) => tracing::error!(%e, "egress proxy failed to start; bots stay offline"),
        }
    }

    // S6-07: `vm_proxy::serve` needs its own handle on the same db every
    // route shares, taken before `build_app` consumes `state` below.
    let db_handle = state.db_handle();

    // The idle sweep. Ported in S6-06b and then never started from here, so
    // nothing has hibernated a VM since: `bullpen-vm-dora` stayed up all of
    // 2026-09-15 into 09-16 holding its RAM. Port of TS `index.ts:127-131`,
    // including the 60s default from `vm.ts:448-452`'s `everyMs = 60_000`.
    //
    // Guarded by `vm_enabled` for the same reason TS guards it with
    // `vmsEnabled()`: a server that runs no VMs should also run no timer.
    // Started BEFORE `build_app` consumes `state`, same constraint as
    // `db_handle` above.
    if state.vm_enabled {
        let _vm_reaper = server::vm::start_vm_reaper(
            state.db_handle(),
            state.vm_docker.clone(),
            state.vm_config.clone(),
            std::time::Duration::from_secs(60),
            state.desktop_states.clone(),
            state.observations.clone(),
        );
        tracing::info!(
            slots = state.vm_config.slots,
            idle_ms = state.vm_config.idle_ms,
            "vm reaper started"
        );
    }

    let app = server::build_app(state);

    // B7: was `0.0.0.0` - every route was unauthenticated (before this
    // ticket's `/api/*` gate) while bound to every interface, so anyone on
    // the LAN could POST messages and spend the OpenRouter key. Loopback by
    // default now that the gate exists too; `BULLPEN_HOST` still opts back
    // into a wider bind for the real deploy (behind the Cloudflare Tunnel).
    let host = std::env::var("BULLPEN_HOST").unwrap_or_else(|_| "127.0.0.1".to_string());
    let ip: std::net::IpAddr = host.parse().unwrap_or_else(|_| {
        tracing::warn!(%host, "invalid BULLPEN_HOST, falling back to 127.0.0.1");
        std::net::IpAddr::from([127, 0, 0, 1])
    });
    let addr = SocketAddr::new(ip, port);
    let listener = tokio::net::TcpListener::bind(addr)
        .await
        .expect("bind port");
    tracing::info!(%addr, "listening");
    // S6-07: `vm_proxy::serve` replaces `axum::serve` so a bot-screen
    // upgrade can be claimed before any HTTP response is written - see
    // `vm_proxy.rs`'s own doc for why `axum::serve` alone has no hook for
    // that.
    server::vm_proxy::serve(listener, app, db_handle, true)
        .await
        .expect("serve");
}
