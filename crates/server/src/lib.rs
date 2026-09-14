//! The axum app. `build_app` returns a `Router` without binding a port; every
//! test drives it through `tower::ServiceExt::oneshot`.

pub mod approvals;
mod auth;
pub mod changes;
mod error;
pub mod permissions;
pub mod prompt;
mod rooms;
mod routes;
pub mod rules;
pub mod runs;
pub mod sandbox;
pub mod spend;
mod tools;

pub use error::{ApiResult, AppError};

use axum::extract::{Request, State};
use axum::response::{IntoResponse, Response};
use axum::{Router, http::StatusCode};
use model::Catalog;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use store::Db;
use tower::Service;
use tower_http::services::ServeDir;
use tower_http::trace::TraceLayer;

/// Shared state handed to every route: one guarded connection to `bullpen.db`,
/// plus the run manager and room engine every run-touching route drives.
/// A `Mutex` on the db (not a pool) because rusqlite's `Connection` is `Send`
/// but not `Sync` - one query runs at a time, same as the TS server's single
/// `better-sqlite3` handle.
#[derive(Clone)]
pub struct AppState {
    db: Arc<Mutex<Db>>,
    client_root: Arc<String>,
    /// S1-06: every route that starts, subscribes to, or stops a run reaches
    /// it through here - the same manager a room round chains through.
    runs: Arc<runs::RunManager>,
    /// S1-06: installed onto `runs`'s two room hooks at construction
    /// (`rooms::RoomEngine::install`), so a route only ever needs to
    /// REGISTER a round's first leg - the chain from there runs itself.
    room_engine: Arc<rooms::RoomEngine>,
    /// S1-F-05: `routes/auth.rs`'s login route reaches this to answer 429
    /// "Too many attempts" - one throttle per running server, fresh per
    /// `AppState` so `cargo test`'s many parallel instances never share a
    /// counter (see `auth::LoginThrottle`'s doc).
    login_throttle: Arc<auth::LoginThrottle>,
    /// S2-06: the model catalog for searching available models.
    pub catalog: Arc<dyn Catalog>,
    /// S2-F-04: the account's real OpenRouter balance - what the spend
    /// ceiling gate (`routes/messages.rs`) and the spend panel
    /// (`routes/spend.rs`) both read.
    pub credits: Arc<dyn spend::CreditsPort>,
    /// S6L-01: the sandbox for executing bot commands, chosen based on
    /// `BULLPEN_SANDBOX` env var at startup.
    pub sandbox: Arc<dyn sandbox::Sandbox>,
}

impl AppState {
    pub fn new(db: Db) -> Self {
        Self::build(
            db,
            default_client_root(),
            default_port(),
            default_catalog(),
            default_credits(),
            default_sandbox(),
        )
    }

    pub fn with_client_root(db: Db, client_root: String) -> Self {
        Self::build(
            db,
            client_root,
            default_port(),
            default_catalog(),
            default_credits(),
            default_sandbox(),
        )
    }

    /// S1-06: lets a test swap in a scripted `ModelPort` (`model::fake`, or
    /// a bespoke ad hoc one) while keeping the same `client_root`
    /// resolution `new` uses - the seam route tests need to drive real runs
    /// without an OpenRouter key.
    pub fn with_port(db: Db, port: Arc<dyn model::ModelPort>) -> Self {
        Self::build(
            db,
            default_client_root(),
            port,
            default_catalog(),
            default_credits(),
            default_sandbox(),
        )
    }

    /// S2-06: lets a test swap in a fixture catalog while keeping the same port.
    pub fn with_catalog(db: Db, catalog: Arc<dyn Catalog>) -> Self {
        Self::build(
            db,
            default_client_root(),
            default_port(),
            catalog,
            default_credits(),
            default_sandbox(),
        )
    }

    /// S2-F-04: lets a test swap in a scripted `CreditsPort` (`spend::FakeCredits`)
    /// while keeping the same port/catalog `new` uses - the 402/warning
    /// HTTP cases only need this to differ from `new`.
    pub fn with_credits(db: Db, credits: Arc<dyn spend::CreditsPort>) -> Self {
        Self::build(
            db,
            default_client_root(),
            default_port(),
            default_catalog(),
            credits,
            default_sandbox(),
        )
    }

    /// S6L-01: lets a test swap in a scripted `Sandbox` (e.g. `FakeRunner`)
    /// while keeping the same port/catalog/credits the `new` uses.
    pub fn with_sandbox(db: Db, sandbox: Arc<dyn sandbox::Sandbox>) -> Self {
        Self::build(
            db,
            default_client_root(),
            default_port(),
            default_catalog(),
            default_credits(),
            sandbox,
        )
    }

    /// S2-F-04: lets a test swap in BOTH a scripted `ModelPort` and a
    /// scripted `CreditsPort` - the spend-gate HTTP tests that need a run
    /// to actually drive (to see the starting notice, not just the 402
    /// path) need both at once.
    pub fn with_port_and_credits(
        db: Db,
        port: Arc<dyn model::ModelPort>,
        credits: Arc<dyn spend::CreditsPort>,
    ) -> Self {
        Self::build(
            db,
            default_client_root(),
            port,
            default_catalog(),
            credits,
            default_sandbox(),
        )
    }

    fn build(
        db: Db,
        client_root: String,
        port: Arc<dyn model::ModelPort>,
        catalog: Arc<dyn Catalog>,
        credits: Arc<dyn spend::CreditsPort>,
        sandbox: Arc<dyn sandbox::Sandbox>,
    ) -> Self {
        // F1: `routing_log` is self-creating (same convention as
        // `rules::ensure_table`), but nothing in production ever called it -
        // `GET`/`PUT /api/routing` 500'd on a real db with "no such table".
        // Every `AppState` constructor funnels through here, so this is the
        // one place a fresh server is guaranteed to pass through before its
        // first request.
        if let Err(err) = model::routing::ensure_routing_tables(&db) {
            tracing::error!("failed to ensure routing_log table exists: {err}");
        }
        let db = Arc::new(Mutex::new(db));
        // S6L-02: threaded through so a `with_sandbox` test double (or a
        // real `on` server) actually reaches `shell`/`sandbox_read` - without
        // this, `AppState.sandbox` was stored but never read, and
        // `RunManager` resolved its OWN default underneath it.
        let runs = Arc::new(runs::RunManager::with_sandbox(
            Arc::clone(&db),
            port,
            Arc::clone(&sandbox),
        ));
        let room_engine = rooms::RoomEngine::install(Arc::clone(&db), Arc::clone(&runs));
        AppState {
            db,
            client_root: Arc::new(client_root),
            runs,
            room_engine,
            login_throttle: Arc::new(auth::LoginThrottle::new()),
            catalog,
            credits,
            sandbox,
        }
    }

    /// B2: the one guard every route takes the db lock through. A poisoned
    /// `Mutex` (left behind by a panic under the lock elsewhere) used to
    /// mean every later `.expect("db mutex poisoned")` panicked too, turning
    /// one bad request into a dead server that only a restart fixed -
    /// `unwrap_or_else(PoisonError::into_inner)` recovers the guard instead.
    pub(crate) fn db(&self) -> MutexGuard<'_, Db> {
        self.db.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

fn default_client_root() -> String {
    std::env::var("BULLPEN_CLIENT_ROOT").unwrap_or_else(|_| "./dist/client".to_string())
}

/// The live OpenRouter port, reading its key the standard way
/// (`BULLPEN_OPENROUTER_KEY_FILE` then `BULLPEN_OPENROUTER_KEY`). What
/// `new`/`with_client_root` build with - production never calls `with_port`.
fn default_port() -> Arc<dyn model::ModelPort> {
    Arc::new(model::OpenRouterPort::new(model::KeySource::Env))
}

/// F2: the real OpenRouter catalogue when a key is configured - the only
/// way a production server's picker is ever non-empty - falling back to an
/// empty fixture only when no key is set at all (a dev box with nothing
/// configured yet, not a real deployment). Tests override this entirely
/// with their own fixture via `AppState::with_catalog`.
fn default_catalog() -> Arc<dyn Catalog> {
    if model::KeySource::Env.resolve().is_some() {
        Arc::new(model::OpenRouterCatalog::new(model::KeySource::Env))
    } else {
        Arc::new(model::FixtureCatalog::from_json("[]").unwrap())
    }
}

/// S2-F-04: the live OpenRouter credits reader - what a production
/// server's spend ceiling gate and spend panel read from. No "empty"
/// fallback like `default_catalog` needs: an unconfigured key just
/// answers every call with an error, which `gate_run`/`get_spend` both
/// already treat as "unreadable" rather than a panic.
fn default_credits() -> Arc<dyn spend::CreditsPort> {
    Arc::new(spend::OpenRouterCredits::new(model::KeySource::Env))
}

/// S6L-01: the sandbox for executing bot commands. Reads `BULLPEN_SANDBOX`
/// to decide whether to create a real `DockerSandbox` or an `UnavailableSandbox`.
/// A startup probe (`docker version`) failing under `on` logs once and falls
/// back to Unavailable. S6L-02 moved the body to `sandbox::default_sandbox`
/// so `RunManager` can resolve the same default; this stays as the name
/// every constructor above already calls.
fn default_sandbox() -> Arc<dyn sandbox::Sandbox> {
    sandbox::default_sandbox()
}

/// Fallback for anything the API router didn't match: `/api/*` gets a plain
/// 404 (never SPA html); a path with a file extension (`/assets/a.js`,
/// `/favicon.ico`) is handed to `ServeDir`, which serves it with a real MIME
/// type via `mime_guess` and answers a missing file with a real 404; an
/// extensionless path (an SPA route like `/settings`) gets `index.html` so
/// the client can handle routing.
async fn static_or_spa(State(state): State<AppState>, req: Request) -> Response {
    let path = req.uri().path().to_string();

    // Never fall through to static files or SPA for /api/* paths.
    if path.starts_with("/api") {
        return (StatusCode::NOT_FOUND, "Not Found").into_response();
    }

    let has_extension = Path::new(&path).extension().is_some();
    if has_extension {
        let mut serve_dir = ServeDir::new(state.client_root.as_str());
        // B21: `ServeDir::call`'s error type is `Infallible` today, not a
        // reason to `.expect()` it - matching on the empty type is exhaustive
        // and stays correct even if a future tower-http widens the error.
        return match serve_dir.call(req).await {
            Ok(response) => response.into_response(),
            Err(never) => match never {},
        };
    }

    // Extensionless path: serve index.html for SPA routing.
    let index_path = PathBuf::from(state.client_root.as_str()).join("index.html");

    match tokio::fs::read_to_string(&index_path).await {
        Ok(html) => axum::response::Html(html).into_response(),
        Err(e) => {
            tracing::error!("failed to read index.html from {:?}: {}", index_path, e);
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                "Client bundle not found. Run `dx build` first, or set BULLPEN_CLIENT_ROOT.",
            )
                .into_response()
        }
    }
}

/// Builds the router. No port binding here - `main.rs` reads
/// `BULLPEN_DATA_DIR` / `BULLPEN_PORT`, opens the db, and serves this.
pub fn build_app(state: AppState) -> Router {
    // B7: the session gate wraps ONLY the API routes, not the static/SPA
    // fallback below - "static files stay open" falls out of that ordering
    // for free, with no path check needed inside the layer itself.
    let api = routes::router().layer(axum::middleware::from_fn_with_state(
        state.clone(),
        auth::require_session,
    ));

    Router::new()
        .merge(api)
        .fallback(static_or_spa)
        .layer(TraceLayer::new_for_http())
        .with_state(state)
}
