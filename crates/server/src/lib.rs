//! The axum app. `build_app` returns a `Router` without binding a port; every
//! test drives it through `tower::ServiceExt::oneshot`.

pub mod approvals;
mod auth;
pub mod away;
pub mod catalogue;
pub mod changes;
pub mod delegate;
pub mod deliverables;
pub mod desk;
pub mod egress;
pub mod egress_proxy;
mod error;
mod export_scrub;
pub mod goals;
pub mod helpers;
pub mod hooks;
pub mod import_open;
pub mod jev;
pub mod job_runner;
pub mod judge;
pub mod marketplace;
pub mod mcp;
pub mod oauth;
pub mod observations;
pub mod permissions;
pub mod prompt;
pub mod push;
pub mod repo;
mod rooms;
mod routes;
pub mod routine_paths;
pub mod routines;
pub mod rules;
pub mod runs;
pub mod sandbox;
pub mod sandbox_routing;
pub mod schedule;
pub mod scope;
pub mod settings_secrets;
pub mod share;
pub mod slack;
pub mod spend;
pub mod teams;
// `pub` rather than crate-private so `tests/browse_tools.rs` can reach
// `server::tools::browse`: Rust visibility is not transitive around a
// private ancestor, so no amount of `pub` on the items mattered while this
// module stayed private (S6-W-03).
pub mod bot_tools;
pub mod tools;
pub mod vm;
pub mod vm_proxy;
pub mod workers;

pub use error::{ApiResult, AppError};

use axum::extract::{Request, State};
use axum::response::{IntoResponse, Response};
use axum::{Router, http::StatusCode};
use model::Catalog;
use rusqlite::OptionalExtension;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use store::Db;
use tower::Service;
use tower_http::services::ServeDir;
use tower_http::trace::TraceLayer;

/// S5c-03: what the `on_run_done` reply-posting hook (wired in `AppState::
/// build` below) needs once a Slack DM/mention run settles - the bot token
/// (plaintext, held only in memory for the run's lifetime; never logged,
/// never written back to disk here) plus the channel/thread to post back
/// into. Port of the TS `pendingSlackReplies` map's value shape
/// (`app.ts:2688`). `pub(crate)` so `routes/slack.rs` can construct one when
/// it starts a run from a DM/mention.
#[derive(Clone)]
pub(crate) struct PendingSlackReply {
    pub(crate) bot_token: String,
    pub(crate) channel: String,
    pub(crate) thread_ts: String,
}

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
    /// S5c-03: the Slack API seam (`auth.test`/`chat.postMessage`/
    /// `chat.delete`) - a real `slack::ReqwestSlackApi` in production,
    /// swapped for a scripted fake in tests via `with_slack_api`/
    /// `with_port_and_slack_api`. `+ Send + Sync` spelled out (not just
    /// `dyn SlackApi`) because the reply-posting `on_run_done` hook below
    /// moves a clone of this into a `tokio::spawn` task.
    pub slack_api: Arc<dyn slack::SlackApi + Send + Sync>,
    /// S5c-03: run id -> what to post back to Slack once that run settles.
    /// Registered by `routes/slack.rs`'s DM/mention branch immediately
    /// after `runs.start` returns (no `.await` in between); drained by the
    /// `on_run_done` hook `build` wires below. See that hook's doc for the
    /// race this ordering does and does not close.
    pending_slack_replies: Arc<Mutex<HashMap<String, PendingSlackReply>>>,
    /// S6-W-01: the production `DockerRun` for `vm.rs`, chosen by
    /// `BULLPEN_VM` (`vm::default_docker_run`) - `vm::DisabledDockerRun` when
    /// off, so a route that forgets its own `vm_enabled` check still cannot
    /// reach a real container.
    pub vm_docker: Arc<dyn vm::DockerRun>,
    /// S6-W-01: `Arc`, not a bare `store::vms::VmConfig` - the struct itself
    /// derives no `Clone`, and `AppState` (which does) would not compile
    /// holding one by value.
    pub vm_config: Arc<store::vms::VmConfig>,
    /// S6-W-01: whether this server runs VMs at all - `store::vms::vms_enabled`
    /// read once at construction, same convention `sandbox`'s `BULLPEN_SANDBOX`
    /// check already uses.
    pub vm_enabled: bool,
    pub desktop_states: Arc<observations::DesktopStateRegistry>,
    pub observations: Arc<observations::ObservationRegistry>,
    /// S7-02: last-seen MCP tool lists per connector id ( refreshed on create/tools ).
    connector_catalogue: Arc<Mutex<HashMap<String, Vec<mcp::ConnectorTool>>>>,
    mcp_transport: Arc<dyn mcp::McpTransport>,
    mcp_resolver: Arc<dyn egress::Resolver>,
    oauth_http: Arc<dyn oauth::OAuthHttp>,
}

impl AppState {
    pub fn new(db: Db) -> Self {
        let vm_config = default_vm_config();
        let vm_docker = default_vm_docker(&vm_config);
        let (sandbox, job_sandbox) = sandbox::default_sandbox_pair();
        Self::build(
            db,
            default_client_root(),
            default_port(),
            default_catalog(),
            default_credits(),
            sandbox,
            job_sandbox,
            default_slack_api(),
            vm_docker,
            vm_config,
            default_vm_enabled(),
        )
    }

    pub fn with_client_root(db: Db, client_root: String) -> Self {
        let vm_config = default_vm_config();
        let vm_docker = default_vm_docker(&vm_config);
        let (sandbox, job_sandbox) = sandbox::default_sandbox_pair();
        Self::build(
            db,
            client_root,
            default_port(),
            default_catalog(),
            default_credits(),
            sandbox,
            job_sandbox,
            default_slack_api(),
            vm_docker,
            vm_config,
            default_vm_enabled(),
        )
    }

    /// S1-06: lets a test swap in a scripted `ModelPort` (`model::fake`, or
    /// a bespoke ad hoc one) while keeping the same `client_root`
    /// resolution `new` uses - the seam route tests need to drive real runs
    /// without an OpenRouter key.
    pub fn with_port(db: Db, port: Arc<dyn model::ModelPort>) -> Self {
        let vm_config = default_vm_config();
        let vm_docker = default_vm_docker(&vm_config);
        let (sandbox, job_sandbox) = sandbox::default_sandbox_pair();
        Self::build(
            db,
            default_client_root(),
            port,
            default_catalog(),
            default_credits(),
            sandbox,
            job_sandbox,
            default_slack_api(),
            vm_docker,
            vm_config,
            default_vm_enabled(),
        )
    }

    /// S2-06: lets a test swap in a fixture catalog while keeping the same port.
    pub fn with_catalog(db: Db, catalog: Arc<dyn Catalog>) -> Self {
        let vm_config = default_vm_config();
        let vm_docker = default_vm_docker(&vm_config);
        let (sandbox, job_sandbox) = sandbox::default_sandbox_pair();
        Self::build(
            db,
            default_client_root(),
            default_port(),
            catalog,
            default_credits(),
            sandbox,
            job_sandbox,
            default_slack_api(),
            vm_docker,
            vm_config,
            default_vm_enabled(),
        )
    }

    /// S2-F-04: lets a test swap in a scripted `CreditsPort` (`spend::FakeCredits`)
    /// while keeping the same port/catalog `new` uses - the 402/warning
    /// HTTP cases only need this to differ from `new`.
    pub fn with_credits(db: Db, credits: Arc<dyn spend::CreditsPort>) -> Self {
        let vm_config = default_vm_config();
        let vm_docker = default_vm_docker(&vm_config);
        let (sandbox, job_sandbox) = sandbox::default_sandbox_pair();
        Self::build(
            db,
            default_client_root(),
            default_port(),
            default_catalog(),
            credits,
            sandbox,
            job_sandbox,
            default_slack_api(),
            vm_docker,
            vm_config,
            default_vm_enabled(),
        )
    }

    /// S6L-01: lets a test swap in a scripted `Sandbox` (e.g. `FakeRunner`)
    /// while keeping the same port/catalog/credits the `new` uses.
    pub fn with_sandbox(db: Db, sandbox: Arc<dyn sandbox::Sandbox>) -> Self {
        let vm_config = default_vm_config();
        let vm_docker = default_vm_docker(&vm_config);
        Self::build(
            db,
            default_client_root(),
            default_port(),
            default_catalog(),
            default_credits(),
            sandbox,
            Arc::new(job_runner::UnavailableJobSandbox),
            default_slack_api(),
            vm_docker,
            vm_config,
            default_vm_enabled(),
        )
    }

    /// S6-W-01: lets a test swap in a scripted `vm::DockerRun`
    /// (`RecordingDockerRun`) and drive `vm_enabled` directly, without
    /// touching the real `BULLPEN_VM` env var - the two-worlds bite
    /// (enabled vs `BULLPEN_VM` off) needs both states in one process.
    pub fn with_vm(
        db: Db,
        docker: Arc<dyn vm::DockerRun>,
        vm_config: store::vms::VmConfig,
        vm_enabled: bool,
    ) -> Self {
        let (sandbox, job_sandbox) = sandbox::default_sandbox_pair();
        Self::build(
            db,
            default_client_root(),
            default_port(),
            default_catalog(),
            default_credits(),
            sandbox,
            job_sandbox,
            default_slack_api(),
            docker,
            Arc::new(vm_config),
            vm_enabled,
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
        let vm_config = default_vm_config();
        let vm_docker = default_vm_docker(&vm_config);
        let (sandbox, job_sandbox) = sandbox::default_sandbox_pair();
        Self::build(
            db,
            default_client_root(),
            port,
            default_catalog(),
            credits,
            sandbox,
            job_sandbox,
            default_slack_api(),
            vm_docker,
            vm_config,
            default_vm_enabled(),
        )
    }

    /// S5c-03: lets a test swap in a scripted `SlackApi` fake while keeping
    /// the same port/catalog/credits/sandbox `new` uses - the config/status/
    /// connect route tests that only care about the Slack seam.
    pub fn with_slack_api(db: Db, slack_api: Arc<dyn slack::SlackApi + Send + Sync>) -> Self {
        let vm_config = default_vm_config();
        let vm_docker = default_vm_docker(&vm_config);
        let (sandbox, job_sandbox) = sandbox::default_sandbox_pair();
        Self::build(
            db,
            default_client_root(),
            default_port(),
            default_catalog(),
            default_credits(),
            sandbox,
            job_sandbox,
            slack_api,
            vm_docker,
            vm_config,
            default_vm_enabled(),
        )
    }

    /// S5c-03: lets a test swap in BOTH a scripted `ModelPort` and a
    /// scripted `SlackApi` - the DM/mention bite (a run actually drives, and
    /// its answer actually reaches `chat.postMessage`) needs both at once.
    pub fn with_port_and_slack_api(
        db: Db,
        port: Arc<dyn model::ModelPort>,
        slack_api: Arc<dyn slack::SlackApi + Send + Sync>,
    ) -> Self {
        let vm_config = default_vm_config();
        let vm_docker = default_vm_docker(&vm_config);
        let (sandbox, job_sandbox) = sandbox::default_sandbox_pair();
        Self::build(
            db,
            default_client_root(),
            port,
            default_catalog(),
            default_credits(),
            sandbox,
            job_sandbox,
            slack_api,
            vm_docker,
            vm_config,
            default_vm_enabled(),
        )
    }

    #[allow(clippy::too_many_arguments)]
    fn build(
        db: Db,
        client_root: String,
        port: Arc<dyn model::ModelPort>,
        catalog: Arc<dyn Catalog>,
        credits: Arc<dyn spend::CreditsPort>,
        sandbox: Arc<dyn sandbox::Sandbox>,
        job_sandbox: Arc<dyn job_runner::JobSandbox>,
        slack_api: Arc<dyn slack::SlackApi + Send + Sync>,
        vm_docker: Arc<dyn vm::DockerRun>,
        vm_config: Arc<store::vms::VmConfig>,
        vm_enabled: bool,
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
        // S8a-01: same F1 shape, same reason. `desk::ensure_desk_tables`
        // (`desk.rs:214`) is self-creating but lives in THIS crate, not
        // `store` - `store::Db::open` already wires up every other
        // self-creating table it can reach (`slack::ensure_slack_tables`,
        // `vms::ensure_vm_tables`, `goals::ensure_goal_tables`), but `store`
        // has no dependency on `server` (see `store/Cargo.toml`) and never
        // can, so a desk-table call cannot live there without a dependency
        // cycle - exactly why `model::routing::ensure_routing_tables` above
        // is called here instead of from `store::Db::open` too. Before this,
        // nothing in production called it at all: `browse`'s first
        // `existing_window` (`desk.rs:259`) hit "no such table:
        // desk_windows" on a live server's first request, and only
        // `tests/desk.rs`/`tests/browse_tools.rs` created the table
        // themselves. `AppState::build` is the one place every constructor
        // (`new`, every `with_*` test helper) funnels through before a
        // route or a test runs, so wiring it here - not into `MIGRATIONS`,
        // which stays byte-identical to the TS source so a live `bullpen.db`
        // opens unchanged - covers the real server and every test that
        // builds an `AppState` in one place.
        if let Err(err) = desk::ensure_desk_tables(&db) {
            tracing::error!("failed to ensure desk_windows table exists: {err}");
        }
        match store::reap_orphaned_jobs(&db) {
            Ok(0) => {}
            Ok(n) => tracing::info!("reaped {n} background job(s) lost to a restart"),
            Err(err) => tracing::error!("failed to reap orphaned jobs: {err}"),
        }
        if let Err(err) = workers::ensure_bot_worker_column(&db) {
            tracing::error!("failed to ensure bots.worker column exists: {err}");
        }
        if let Err(err) = repo::ensure_bot_repo_column(&db) {
            tracing::error!("failed to ensure bots.repo column exists: {err}");
        }
        let db = Arc::new(Mutex::new(db));
        let push_db = Arc::clone(&db);
        let desktop_states = Arc::new(observations::DesktopStateRegistry::new());
        let observations = Arc::new(observations::ObservationRegistry::new());
        // S6L-02: threaded through so a `with_sandbox` test double (or a
        // real `on` server) actually reaches `shell`/`sandbox_read` - without
        // this, `AppState.sandbox` was stored but never read, and
        // `RunManager` resolved its OWN default underneath it.
        // S8a-02: `with_sandbox_and_vm`, not `with_sandbox` - `toolbox_for`
        // needs its own copies of `vm_docker`/`vm_config`/`vm_enabled` to
        // resolve `browse`/`read_page`'s `Cdp` per calling bot
        // (`desk::cdp_for_bot`), the same reason `sandbox` is threaded
        // through here rather than left to its own `BULLPEN_SANDBOX`
        // default. Cloned rather than moved: `vm_docker`/`vm_config` are
        // still needed below, for `AppState`'s own fields.
        let runs = Arc::new(runs::RunManager::with_shared_desktop_state(
            Arc::clone(&db),
            port,
            Arc::clone(&sandbox),
            Arc::clone(&job_sandbox),
            Arc::clone(&vm_docker),
            Arc::clone(&vm_config),
            vm_enabled,
            Arc::clone(&catalog),
            Arc::clone(&observations),
            Arc::clone(&desktop_states),
        ));
        let room_engine = rooms::RoomEngine::install(Arc::clone(&db), Arc::clone(&runs));

        // S5c-F-02 (F5): `store::Db::open` now calls `slack::
        // ensure_slack_tables` itself (`store/src/lib.rs`, beside
        // `goals::ensure_goal_tables`) - every `Db` reaches this constructor
        // only via `Db::open` (its field is private to the `store` crate),
        // so `slack_threads` already exists by the time any `AppState`
        // constructor runs. The redundant call that used to live here
        // (S5c-03's stopgap, landed before the central wiring did) is gone.

        let state = AppState {
            db,
            client_root: Arc::new(client_root),
            runs,
            room_engine,
            login_throttle: Arc::new(auth::LoginThrottle::new()),
            catalog,
            credits,
            sandbox,
            slack_api,
            pending_slack_replies: Arc::new(Mutex::new(HashMap::new())),
            vm_docker,
            vm_config,
            vm_enabled,
            desktop_states,
            observations,
            connector_catalogue: Arc::new(Mutex::new(HashMap::new())),
            mcp_transport: Arc::new(mcp::ReqwestMcpTransport::new()),
            mcp_resolver: Arc::new(desk::RealResolver),
            oauth_http: Arc::new(oauth::ReqwestOAuthHttp::new()),
        };

        // S5b-04b: chains `settle_goal_run` onto `on_run_done` ADDITIVELY,
        // via `RunManager::add_on_run_done` - `RoomEngine::install` above
        // already claimed the hook via `set_on_run_done` to chain a room
        // round, and this must not displace that. Without this, a goal's
        // work session finished and nothing ever folded its spend, advanced
        // its no-progress streak, or paused it - see `goals::settle_goal_run`'s
        // own doc. Fired for every run regardless of what started it, same
        // as TS's `onRunDone`; `settle_goal_run` itself no-ops for a run
        // whose `goal_id` is null, so an ordinary chat/routine/room run
        // costs one no-op lookup here.
        state.runs.set_badger(move |alert| {
            push::spawn_badge_update(Arc::clone(&push_db), alert);
        });

        let goal_state = state.clone();
        state
            .runs
            .add_on_run_done(move |run_id, _bot_id, _conversation_id| {
                goals::settle_goal_run(&goal_state, run_id, chrono::Utc::now());
            });

        // S5c-03: the reply half of the TS `onRunDone` (`app.ts:886-918`) -
        // a run started from a Slack DM/mention posts its answer back to
        // Slack instead of a browser SSE stream. `add_on_run_done` (not
        // `set_on_run_done`) so this chains AFTER `settle_goal_run` above
        // without displacing it, same additive posture that doc explains.
        //
        // Fire-and-forget by design, matching the TS comment on
        // `pendingSlackReplies` verbatim: "this callback is sync, and a
        // failed post is nothing more than a missed reply - there is nobody
        // watching a stream to show an error to." The hook itself stays
        // synchronous (it runs inside `RunManager`'s own settle path); the
        // actual `chat.postMessage` network call happens on a `tokio::spawn`
        // task so a slow/failing Slack API call never blocks the run
        // manager's own settle machinery. No `recordUndo` (W10 is not
        // ported yet) - the TS comment at `app.ts:905-907` names exactly
        // where that would go: the channel + ts `chat.postMessage`'s own
        // response carries, once posted.
        //
        // Judgment call (see this ticket's Results): `run_id` only exists
        // once `runs.start`/`start_routine` RETURNS, so the pending entry
        // cannot be registered before that call the way `RoomEngine::
        // register`'s own doc explains a `conversation_id`-keyed map can
        // (`rooms.rs:74-76` - "registered BEFORE runs.start ... cannot fire
        // on_run_done before this entry exists"). This ticket's design
        // fixes the map's key as `run_id` (`Arc<Mutex<HashMap<run_id,
        // PendingReply>>>`), so `routes/slack.rs` instead registers
        // immediately after `start` returns, with no `.await` in between -
        // the same ordering the TS `pendingSlackReplies.set(runId, ...)`
        // uses right after `runs.start({...})`. TS never races this (single-
        // threaded); here the window is real but narrow: every model call
        // this run manager drives needs at least one genuine async
        // suspension (an HTTP request to OpenRouter, or a scripted test
        // port's own await point) before it can reach `settle`, and no
        // `.await` sits between `start` returning and the registration
        // call, so nothing on this thread yields control in between. A
        // pathological multi-threaded scheduling that lets `settle` run on
        // another OS thread before this thread's very next synchronous line
        // executes would still lose the race; closing that for real needs a
        // `start_with_pending_slack_reply`-shaped entry point on
        // `RunManager` (the same treatment `start_routine`/`start_goal`
        // already got for `routine_id`/`goal_id`, see those docs), which
        // `runs.rs` is out of this ticket's owned files to add.
        let slack_state = state.clone();
        state
            .runs
            .add_on_run_done(move |run_id, _bot_id, _conversation_id| {
                let pending = slack_state
                    .pending_slack_replies
                    .lock()
                    .unwrap_or_else(PoisonError::into_inner)
                    .remove(run_id);
                let Some(pending) = pending else { return };

                let finished: Option<(String, String)> = {
                    let db = slack_state.db();
                    db.conn()
                        .query_row(
                            "SELECT text, status FROM runs WHERE id = ?1",
                            rusqlite::params![run_id],
                            |row| Ok((row.get(0)?, row.get(1)?)),
                        )
                        .optional()
                        .unwrap_or(None)
                };
                let Some((text, status)) = finished else {
                    return;
                };
                // Only a `done` run with something to say gets posted - a
                // `failed` run (or a `done` run whose model said nothing
                // usable) is a missed reply, silently, exactly matching the
                // TS `finished.status === "done" && finished.text.trim() !==
                // ""` guard.
                if status != "done" || text.trim().is_empty() {
                    return;
                }

                let api = Arc::clone(&slack_state.slack_api);
                let run_id_owned = run_id.to_string();
                tokio::spawn(async move {
                    if let Err(err) = slack::post_slack_message(
                        api.as_ref(),
                        &pending.bot_token,
                        &pending.channel,
                        &text,
                        Some(&pending.thread_ts),
                    )
                    .await
                    {
                        tracing::warn!("missed Slack reply for run {run_id_owned}: {err}");
                    }
                });
            });

        state.sync_connector_hooks();
        state
    }

    fn sync_connector_hooks(&self) {
        self.runs
            .set_connector_hooks(Arc::new(runs::ConnectorHooks {
                catalogue: Arc::clone(&self.connector_catalogue),
                transport: Arc::clone(&self.mcp_transport),
                resolver: Arc::clone(&self.mcp_resolver),
                oauth_http: Arc::clone(&self.oauth_http),
            }));
    }

    /// B2: the one guard every route takes the db lock through. A poisoned
    /// `Mutex` (left behind by a panic under the lock elsewhere) used to
    /// mean every later `.expect("db mutex poisoned")` panicked too, turning
    /// one bad request into a dead server that only a restart fixed -
    /// `unwrap_or_else(PoisonError::into_inner)` recovers the guard instead.
    pub(crate) fn db(&self) -> MutexGuard<'_, Db> {
        self.db.lock().unwrap_or_else(PoisonError::into_inner)
    }

    // S6-07: `vm_proxy::serve` needs the `Arc<Mutex<Db>>` itself, not a
    // guard, so it can clone one per accepted connection. `pub` rather than
    // `pub(crate)` like `db()` above because `main.rs` is a separate crate
    // and is the only caller.
    pub fn db_handle(&self) -> Arc<Mutex<Db>> {
        Arc::clone(&self.db)
    }

    /// S10-09: point W5 at the same on-disk db and data directory the server uses.
    pub fn configure_w5(&self, db_path: String, data_dir: String) {
        self.runs.set_w5_paths(db_path, data_dir);
    }

    /// On-disk attachment bytes live under `{data_dir}/attachments/`.
    pub fn data_dir(&self) -> String {
        self.runs.data_dir()
    }

    /// S5c-03: registers what `routes/slack.rs`'s DM/mention branch should
    /// post back to Slack once `run_id` settles. See the `add_on_run_done`
    /// hook above (in `build`) for why this is called immediately after
    /// `runs.start` returns, with no `.await` in between, and what race that
    /// does and does not close.
    pub(crate) fn register_pending_slack_reply(&self, run_id: String, reply: PendingSlackReply) {
        self.pending_slack_replies
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .insert(run_id, reply);
    }

    /// S7-03: usable access token without holding the db lock across await.
    pub async fn bearer_for_connector(&self, connector_id: &str) -> Option<String> {
        oauth::bearer_for_shared(&self.db, connector_id, self.oauth_http.as_ref()).await
    }

    /// S7-02: lists tools from the MCP server and caches them on success.
    pub async fn refresh_connector_tools(&self, connector_id: &str) -> mcp::ListToolsOutcome {
        let full = {
            let db = self.db();
            match store::get_connector(&db, connector_id) {
                Ok(Some(row)) => row,
                Ok(None) => {
                    return mcp::ListToolsOutcome {
                        ok: false,
                        tools: vec![],
                        error: Some("no such connector".to_string()),
                        needs_auth: false,
                        challenge: None,
                    };
                }
                Err(err) => {
                    return mcp::ListToolsOutcome {
                        ok: false,
                        tools: vec![],
                        error: Some(err.to_string()),
                        needs_auth: false,
                        challenge: None,
                    };
                }
            }
        };

        let bearer = self.bearer_for_connector(connector_id).await;

        let outcome = mcp::list_connector_tools(
            &full,
            mcp::McpCallOptions {
                transport: self.mcp_transport.as_ref(),
                resolver: self.mcp_resolver.as_ref(),
                bearer: bearer.as_deref(),
            },
        )
        .await;

        if outcome.ok {
            self.connector_catalogue
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .insert(connector_id.to_string(), outcome.tools.clone());
        }

        outcome
    }

    /// S7-02: test seam — inject fake MCP transport/resolver (see `mcp.test.ts`).
    pub fn with_mcp(
        db: Db,
        transport: Arc<dyn mcp::McpTransport>,
        resolver: Arc<dyn egress::Resolver>,
    ) -> Self {
        let mut state = Self::new(db);
        state.mcp_transport = transport;
        state.mcp_resolver = resolver;
        state.sync_connector_hooks();
        state
    }

    pub fn with_mcp_and_oauth(
        db: Db,
        transport: Arc<dyn mcp::McpTransport>,
        resolver: Arc<dyn egress::Resolver>,
        oauth_http: Arc<dyn oauth::OAuthHttp>,
    ) -> Self {
        let mut state = Self::with_mcp(db, transport, resolver);
        state.oauth_http = oauth_http;
        state.sync_connector_hooks();
        state
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

/// S5c-03: the real Slack API - what a production server's `PUT /api/slack`
/// and the reply-posting `on_run_done` hook call through. No key/config
/// needed at construction time (unlike `default_credits`/`default_catalog`):
/// `slack::ReqwestSlackApi` takes the bot token per-call, read out of
/// storage by the caller.
fn default_slack_api() -> Arc<dyn slack::SlackApi + Send + Sync> {
    Arc::new(slack::ReqwestSlackApi)
}

/// S6-W-01: this process's own environment, snapshotted once - what every
/// `vm::*`/`store::vms::*` env-gated default below reads from, matching the
/// `&HashMap` signature `store::vms::vms_enabled`/`vm_config` already take.
fn env_snapshot() -> HashMap<String, String> {
    std::env::vars().collect()
}

/// The `VmConfig` a production server (or any constructor that does not
/// override it) runs with - `BULLPEN_VM_*` env vars, same defaults
/// `store::vms::vm_config` already documents.
fn default_vm_config() -> Arc<store::vms::VmConfig> {
    Arc::new(store::vms::vm_config(&env_snapshot()))
}

/// Whether this server runs VMs at all - `BULLPEN_VM=on`, off by default
/// exactly like `BULLPEN_SANDBOX`.
fn default_vm_enabled() -> bool {
    store::vms::vms_enabled(&env_snapshot())
}

/// The production `vm::DockerRun` - real `docker` CLI calls when
/// `BULLPEN_VM=on`, `vm::DisabledDockerRun` otherwise. See
/// `vm::default_docker_run`'s own doc for why the choice is made twice (once
/// here via `cfg`'s caller, once inside `DockerRun::call` itself).
fn default_vm_docker(cfg: &store::vms::VmConfig) -> Arc<dyn vm::DockerRun> {
    vm::default_docker_run(&env_snapshot(), cfg)
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
    let api = routes::router()
        .layer(axum::middleware::from_fn_with_state(
            state.clone(),
            scope::require_scope,
        ))
        .layer(axum::middleware::from_fn_with_state(
            state.clone(),
            auth::require_session,
        ));

    Router::new()
        .merge(api)
        .fallback(static_or_spa)
        .layer(TraceLayer::new_for_http())
        .with_state(state)
}

#[cfg(test)]
mod screen_catalog_tests {
    use super::*;

    #[tokio::test]
    async fn app_and_run_manager_share_the_same_capability_catalog() {
        let catalog: Arc<dyn model::Catalog> =
            Arc::new(model::FixtureCatalog::from_json("[]").unwrap());
        let state = AppState::with_catalog(Db::open(":memory:").unwrap(), Arc::clone(&catalog));
        assert!(Arc::ptr_eq(&catalog, &state.runs.catalog()));
    }

    #[tokio::test]
    async fn app_desktop_mutation_invalidates_a_run_owned_observation() {
        let state = AppState::with_catalog(
            Db::open(":memory:").unwrap(),
            Arc::new(model::FixtureCatalog::from_json("[]").unwrap()),
        );
        let admission = Arc::new(observations::ObservationAdmission::new());
        let observation = admission.try_begin_capture().unwrap().retain(
            vm::CapturedFrame {
                png: vec![1],
                width: 1,
                height: 1,
            },
            "shared-run",
            "shared-bot",
            "shared-observation",
            "2026-09-18T00:00:00Z",
            0,
        );
        state.runs.observation_registry().store(&observation);
        let desktop = state.desktop_states.for_bot("shared-bot");
        let mut guard = desktop.lock().await;
        state
            .observations
            .record_desktop_mutation("shared-bot", &mut guard);
        drop(guard);
        assert!(
            state
                .runs
                .observation_registry()
                .metadata("shared-run")
                .is_none()
        );
        assert_eq!(
            state
                .runs
                .desktop_state_registry()
                .for_bot("shared-bot")
                .lock()
                .await
                .generation(),
            1
        );
    }
}
