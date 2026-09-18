//! Runs, as rows. Port of `src/server/runs.ts` and `src/server/run.ts`,
//! narrowed to what S1+S2-03 need: build the prompt (the caller's job, via
//! `crate::prompt`), stream the model, execute tool calls in a loop (asking
//! `crate::permissions` first, and parking on the ones that need Josh -
//! S2-03), persist the row, emit events for subscribers, settle. NOT
//! escalation, routing, snapshots, interjections or jobs - those are S2-04+.
//!
//! A run outlives its HTTP request: `start` returns an id immediately and
//! drives the run on a spawned task, so a client that closes the tab (or a
//! routine with no client at all, once S2 adds those) never abandons it.

use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::Duration;

use base64::{Engine as _, engine::general_purpose::STANDARD};
use futures::StreamExt;
use model::ladder::{Trigger, model_for_run};
use model::{
    Catalog, ContentPart, FunctionCall, ImageUrl, MessageContent, MessageToolCall, ModelEvent,
    ModelMessage, ModelPort, ModelRequest, ModelUsage, ToolCall, ToolSpec,
};
use rusqlite::OptionalExtension;
use store::Db;
use uuid::Uuid;

use crate::approvals;
use crate::changes::{ChangeBus, ChangeKind};
use crate::judge;
use crate::observations::{
    CounterClaim, ObservationAdmission, ObservationDispatch, ObservationRegistry,
    ScreenObservation, claim_capture_attempt, claim_image_dispatch,
};
use crate::permissions::{self, Decision};
use crate::rules;
use crate::sandbox;
use crate::tools::{self, RoomHook, ToolBox};
use crate::vm;

/// How many tool steps a single run may take before it is stopped rather
/// than left to loop. Matches TS's `MAX_STEPS` (`run.ts:78`) now that S2-04
/// gives a stuck run two ways out well before the ceiling - routing to a
/// stronger model up front, and `escalate` mid-turn - the same reasoning
/// that raised TS's own cap from 6 to 24. S1 held this at 12 pending both.
const MAX_STEPS: i64 = 24;

/// How long a settled run's event backlog survives for a late subscriber
/// before `finish`'s delayed cleanup drops it, together with any `bus`
/// entry a late `subscribe` recreated in the meantime (S1-F-04: B3, B5).
const BACKLOG_TTL: Duration = Duration::from_secs(60);

/// F8: how long a `pending` approval may sit unanswered before
/// `sweep_approvals` fails the run parked on it. Nothing else ever
/// revisits a `waiting` run - `park` deliberately leaves `bus`/`backlog`/
/// `interjections` alone expecting `decide_approval` to pick it back up,
/// and if Josh never does, none of that was ever going away on its own.
const APPROVAL_TTL_HOURS: i64 = 24;

/// F8: how long a decided (`approved`/`rejected`) or `expired` approval row
/// survives before `sweep_approvals` deletes it - the `approvals` table's
/// only retention policy. A still-`pending` row is never touched by this;
/// only `APPROVAL_TTL_HOURS` (via `mark_expired`) or Josh deciding it moves
/// a row into scope for this one.
const APPROVAL_RETENTION_DAYS: i64 = 30;

/// One event a run's subscribers see. Port of the TS `RunEvent`.
#[derive(Debug, Clone)]
pub enum RunEvent {
    Delta {
        text: String,
    },
    ToolCall {
        name: String,
        args: String,
    },
    ToolResult {
        name: String,
        result: String,
    },
    Done {
        model: String,
        message_id: String,
    },
    Error {
        message: String,
        status: Option<u16>,
    },
    /// S2-03: the run just parked on a tool call that needs Josh's decision.
    /// Port of the TS `approval_needed` (`runs.ts:1718-1723`).
    ApprovalNeeded {
        approval_id: String,
        name: String,
        args: String,
    },
    /// S2-04: a side note about this run that is not itself an answer -
    /// today, "routed to a stronger model" (`runs.ts:673`, "Routed to X:
    /// real work") and "escalated a rung" (`tools/escalate.rs`). Port of
    /// the TS `notice` event, narrowed to these two sources; S1's snapshot
    /// notice does not exist here yet.
    Notice {
        message: String,
    },
}

/// What starts a run. Port of the TS `StartOptions`, narrowed to S1: no
/// `notice`, `routineId`, `goalId` or tool-narrowing field here - a
/// routine's `routineId` is stamped by `start_routine`/
/// `start_routine_narrowed` instead of a field on this struct (see
/// `start_routine`'s doc for why: 13 struct-literal call sites), and F3's
/// tool-narrowing is the same shape, via `start_routine_narrowed`.
pub struct StartOptions {
    pub bot_id: String,
    pub conversation_id: String,
    /// The bot's own pinned (or default) model - `model_for_run` is the ONLY
    /// place this is actually settled, same as the TS original.
    pub model: String,
    /// The full prompt, already built by the caller (`crate::prompt::build_prompt`
    /// plus, for a room, `with_room_instruction`).
    pub messages: Vec<ModelMessage>,
    pub trigger: Trigger,
    /// H12: a member's turn in a room ROUND. Forces the cheap model floor
    /// (see `model::ladder::model_for_run`'s doc) without changing the
    /// trigger - a room round is still `Trigger::Chat`.
    pub room: bool,
}

/// F2: what `RunManager::record_tool_run` writes - port of the TS
/// `recordToolRun` params (`runs.ts:411-450`). One of these is written on
/// EVERY branch of `fire_routine_tool` (`routines.rs`), refused/unknown/
/// bad-args/quiet/found alike, so "Recent runs" (`routines_editor.rs:550`)
/// is never empty for a tool routine that has actually fired and the
/// 3-strikes auto-pause has failures to count on a path that used to
/// record nothing at all.
pub struct RecordToolRun {
    pub bot_id: String,
    pub conversation_id: String,
    pub routine_id: String,
    pub tool: String,
    pub args: String,
    pub result: String,
    pub ok: bool,
    pub error: Option<String>,
}

#[derive(Clone, Default, PartialEq)]
struct ActivityState {
    tool: Option<String>,
    wrote_text: bool,
}

/// The end state of one call to `port.stream`, folded across every step of
/// the turn. Shared by both `Outcome` variants so the settle path does not
/// duplicate five fields twice.
struct RunState {
    messages: Vec<ModelMessage>,
    text: String,
    /// The model selected by Bullpen for the next request. This is the value
    /// persisted on the run row so an approval resume keeps routing and
    /// escalation decisions.
    effective_requested_model: String,
    /// The provider-reported model that produced the latest response. This
    /// remains the model attributed to the saved assistant message and done
    /// event, even when the provider served a different model than requested.
    responding_model: String,
    usage: Option<ModelUsage>,
    steps: i64,
}

enum Outcome {
    Answered(RunState),
    Failed {
        state: RunState,
        failure: String,
        /// F5: the upstream HTTP status behind this failure, when there was
        /// one - carried through to `RunEvent::Error` so a 429 reaches the
        /// browser as a 429 (TS `fail(runId, message, status)`,
        /// `runs.ts:1741-1752`) instead of a bare message. `None` for
        /// anything that is not itself an upstream error: a stop, the step
        /// ceiling.
        status: Option<u16>,
    },
    /// S2-03: a tool call in this step needs Josh's decision. Port of the TS
    /// `{ status: "paused" }` (`run.ts:57-64`). `pending` is the call
    /// waiting on him; `deferred` is whatever the model asked for AFTER it
    /// in the same step - dropped rather than run unsupervised, same as the
    /// TS `park` doc explains.
    Paused {
        state: RunState,
        pending: ToolCall,
        deferred: Vec<ToolCall>,
        /// S4-04: the auto-review judge's verdict/reason for `pending`, when
        /// this Ask came from a `risky`/`dangerous` judgement rather than
        /// the grid itself. `None` for a plain grid Ask - the judge never
        /// ran, so there is nothing of its to show. `park` writes these
        /// onto the approval row.
        judge_verdict: Option<String>,
        judge_reason: Option<String>,
    },
}

/// Fired once a run settles. Aliased so the `RunManager` field below does
/// not trip clippy's `type_complexity`.
type OnRunDone = Box<dyn Fn(&str, &str, &str) + Send + Sync>;

/// The run manager: builds and drives runs, and answers "who is working".
pub struct RunManager {
    db: Arc<Mutex<Db>>,
    port: Arc<dyn ModelPort>,
    catalog: Arc<dyn Catalog>,
    observation_admission: Arc<ObservationAdmission>,
    observations: Arc<ObservationRegistry>,
    frame_capture: Arc<dyn vm::FrameCapture>,
    /// S6L-02: what `toolbox_for` hands `shell`/`sandbox_read` to actually run
    /// against. Resolved from `BULLPEN_SANDBOX` by the no-arg constructors
    /// (`new`, `with_backlog_ttl`); injected by `with_sandbox` for
    /// `AppState::build` and sandbox-aware tests.
    sandbox: Arc<dyn sandbox::Sandbox>,
    /// S8a-02: what `toolbox_for` hands `browse`/`read_page` to resolve
    /// their `Cdp` against - `desk::cdp_for_bot`'s own doc has the routing
    /// decisions. Resolved from `BULLPEN_VM` by the no-arg constructors
    /// (`new`, `with_backlog_ttl`, `with_sandbox`), same convention
    /// `sandbox` above already uses for `BULLPEN_SANDBOX`; injected by
    /// `with_sandbox_and_vm` for `AppState::build` and vm-aware tests.
    vm_docker: Arc<dyn vm::DockerRun>,
    vm_config: Arc<store::vms::VmConfig>,
    vm_enabled: bool,
    /// The kind-based change bus (roster/approvals/questions/working) this
    /// manager touches. Public so a caller (a test, or S1-06's routes) can
    /// subscribe to it the same way the TS routes subscribe to `changes.ts`.
    pub changes: ChangeBus,
    /// Live subscribers per run id, fed by `emit`.
    bus: Mutex<HashMap<String, Vec<tokio::sync::mpsc::UnboundedSender<RunEvent>>>>,
    /// Every event emitted so far per run, so a late subscriber does not
    /// miss the start.
    backlog: Mutex<HashMap<String, Vec<RunEvent>>>,
    /// What each live run is doing, for `working()`. Cleared when the run
    /// settles - see the TS doc on why this is memory-only.
    activity: Mutex<HashMap<String, ActivityState>>,
    /// Runs Josh (or a caller) has asked to stop. Checked between steps.
    stopping: Mutex<HashSet<String>>,
    /// S2-04: text queued for a still-`running` run, drained into a user
    /// turn between model calls - port of the TS `interjections` map
    /// (`runs.ts:331`). Never touched for a `waiting`/`done`/`failed` run;
    /// see `interject`'s own doc.
    interjections: Mutex<HashMap<String, Vec<String>>>,
    /// Fired once a run settles, answered or failed alike. `None` until a
    /// caller wires one in - S1-06 sets this to chain a room round.
    on_run_done: Mutex<Option<OnRunDone>>,
    /// What `message_bot` calls when it posts into a room. `None` until
    /// S1-06 sets it.
    start_room_turn: RoomHook,
    /// How long a settled run's backlog survives before `finish`'s delayed
    /// cleanup drops it - the real product value from `new`; shrunk by
    /// `with_backlog_ttl` so a test proving the bound (S1-F-04) does not
    /// have to sleep out a full minute.
    backlog_ttl: Duration,
}

/// S13b-F/S13b-03: tools whose result the CLIENT computes rather than the
/// server - port of TS's `CLIENT_FULFILLED` (`clientTools.ts`). S13b-F
/// landed this list empty, shipping only the `is_answerable` arm of the
/// gate in `decide_approval` below. S13b-03 adds the one entry TS has:
/// `"read_file"`, the local-bridge tool (`tools/local_read.rs`) whose
/// `run` always refuses - the desktop client is the only thing that can
/// actually perform this read, and posting its result here is the only way
/// the model ever sees one.
///
/// 🔴 Kept a SEPARATE list from `is_answerable` on purpose, carrying TS's
/// own reasoning (`runs.ts:749-754`): `is_client_fulfilled` means only the
/// desktop app on Josh's own machine can perform the read. `is_answerable`
/// means there is nothing to perform at all - the bot asked a question and
/// Josh's typed answer IS the result. Folding them into one list would let
/// a future client-fulfilled tool be satisfied by a typed string with no
/// machine behind it - this list is a security boundary, not a
/// convenience, so nothing outside it (never `shell`) may be added without
/// the same reasoning applying.
const CLIENT_FULFILLED: &[&str] = &["read_file"];

fn is_client_fulfilled(tool_name: &str) -> bool {
    CLIENT_FULFILLED.contains(&tool_name)
}

/// S13b-03-03 (design §4.5): does THIS RUN have any approval whose
/// `status` is `'approved'` and whose `tool_name` is in `CLIENT_FULFILLED`?
/// Queried FRESH every time `run_turn` starts - the very top, before its
/// per-turn `perms` resolution - and NEVER carried through as a parameter.
///
/// 🔴 A carried flag dies at the first pause the taint itself causes
/// (R6-L1, the sharpest finding of the whole design series): `Ask` ->
/// `Outcome::Paused` -> `park`, and the only resume is `decide_approval`,
/// which would otherwise have to recompute the flag from THAT approval
/// alone. The attack this closes: the bot reads a file, Josh approves, the
/// run resumes tainted; the bot then asks an innocuous
/// `ask_josh{wait: true}` question - itself parked, on `decide_call`'s own
/// pin, nothing to do with the taint; Josh answers it; a carried-parameter
/// implementation would resume the run UNTAINTED, with the file's contents
/// still sitting in `runs.messages`, and every egress tool back to
/// `Allow`. Deriving fresh from the WHOLE run's history closes it: the
/// taint is a fact about the run, not a value anyone has to remember to
/// pass on, so it survives any number of intervening pauses for free.
///
/// `approvals::take_pending` UPDATEs `status` to `'approved'` rather than
/// deleting the row (see its own doc), so this is answerable from what is
/// already there. `sweep_approvals` only prunes DECIDED rows older than
/// `APPROVAL_RETENTION_DAYS` (30 days, far longer than any run lives), so
/// this stays answerable for the whole life of a run - see that constant's
/// own doc for the one way that could change.
fn run_is_tainted(db: &Db, run_id: &str) -> rusqlite::Result<bool> {
    let placeholders = vec!["?"; CLIENT_FULFILLED.len()].join(", ");
    let sql = format!(
        "SELECT 1 FROM approvals \
         WHERE run_id = ? AND status = 'approved' AND tool_name IN ({placeholders}) LIMIT 1"
    );
    let mut stmt = db.conn().prepare(&sql)?;
    let mut values: Vec<String> = Vec::with_capacity(1 + CLIENT_FULFILLED.len());
    values.push(run_id.to_string());
    values.extend(CLIENT_FULFILLED.iter().map(|name| name.to_string()));
    stmt.exists(rusqlite::params_from_iter(values.iter()))
}

/// The cap on what one approval's posted `result` may contribute to the
/// transcript. Port of TS's `MAX_FULFILMENT_CHARS` (`clientTools.ts`),
/// enforced HERE rather than trusted from the client - a cap only the
/// client applies is not a cap.
const MAX_FULFILMENT_CHARS: usize = 200_000;

/// The `vm::DockerRun`/`VmConfig`/enabled triple every no-arg-ish
/// `RunManager` constructor resolves for itself, same convention
/// `sandbox::default_sandbox` already gives the `sandbox` field - env-read
/// fresh each call (matches `AppState`'s own private `default_vm_*` helpers
/// in `lib.rs`, which this does not reuse only because they are private to
/// that module; duplicated rather than made `pub(crate)` there since this
/// ticket does not otherwise touch `lib.rs`'s constructor family).
fn default_vm_parts() -> (Arc<dyn vm::DockerRun>, Arc<store::vms::VmConfig>, bool) {
    let env: HashMap<String, String> = std::env::vars().collect();
    let cfg = Arc::new(store::vms::vm_config(&env));
    let docker = vm::default_docker_run(&env, &cfg);
    let enabled = store::vms::vms_enabled(&env);
    (docker, cfg, enabled)
}

fn empty_catalog() -> Arc<dyn Catalog> {
    Arc::new(model::FixtureCatalog::from_json("[]").expect("empty catalog fixture"))
}

impl RunManager {
    pub fn new(db: Arc<Mutex<Db>>, port: Arc<dyn ModelPort>) -> Self {
        Self::with_backlog_ttl(db, port, BACKLOG_TTL)
    }

    /// Same as `new`, but with an explicit backlog grace period instead of
    /// the real 60s - S1-F-04's bite check would otherwise cost a minute of
    /// wall clock per run it proves.
    pub fn with_backlog_ttl(
        db: Arc<Mutex<Db>>,
        port: Arc<dyn ModelPort>,
        backlog_ttl: Duration,
    ) -> Self {
        let (vm_docker, vm_config, vm_enabled) = default_vm_parts();
        Self::build(
            db,
            port,
            sandbox::default_sandbox(),
            vm_docker,
            vm_config,
            vm_enabled,
            backlog_ttl,
            empty_catalog(),
            Arc::new(vm::RealFrameCapture),
        )
    }

    /// S6L-02: lets a caller (`AppState::build`, or a sandbox-aware test)
    /// inject the `Sandbox` `shell`/`sandbox_read` actually run against,
    /// instead of resolving one from `BULLPEN_SANDBOX` - the same seam
    /// `AppState::with_port` gives the model. Every OTHER constructor here
    /// still resolves its own default via `sandbox::default_sandbox`, so
    /// the many call sites across this crate's test suite that predate
    /// this ticket keep compiling and keep seeing the S2 Unavailable text
    /// unchanged. S8a-02: vm parts still resolve from `BULLPEN_VM` here
    /// (unchanged default) - a sandbox-only test never cared about VM
    /// routing, so widening this constructor's signature too would touch
    /// every one of its existing callers for nothing; a caller that DOES
    /// need to inject VM parts wants `with_sandbox_and_vm` instead.
    pub fn with_sandbox(
        db: Arc<Mutex<Db>>,
        port: Arc<dyn ModelPort>,
        sandbox: Arc<dyn sandbox::Sandbox>,
    ) -> Self {
        let (vm_docker, vm_config, vm_enabled) = default_vm_parts();
        Self::build(
            db,
            port,
            sandbox,
            vm_docker,
            vm_config,
            vm_enabled,
            BACKLOG_TTL,
            empty_catalog(),
            Arc::new(vm::RealFrameCapture),
        )
    }

    /// S8a-02: lets a caller (`AppState::build`, or a vm-aware test) inject
    /// BOTH the `Sandbox` and the `vm::DockerRun`/`VmConfig`/`vm_enabled`
    /// triple `browse`/`read_page` resolve their `Cdp` through - the same
    /// seam `AppState::with_vm` already gives `routes/vms.rs`. Every other
    /// constructor here keeps resolving its own env defaults for both.
    #[allow(clippy::too_many_arguments)]
    pub fn with_sandbox_and_vm(
        db: Arc<Mutex<Db>>,
        port: Arc<dyn ModelPort>,
        sandbox: Arc<dyn sandbox::Sandbox>,
        vm_docker: Arc<dyn vm::DockerRun>,
        vm_config: Arc<store::vms::VmConfig>,
        vm_enabled: bool,
    ) -> Self {
        Self::with_sandbox_vm_and_catalog(
            db,
            port,
            sandbox,
            vm_docker,
            vm_config,
            vm_enabled,
            empty_catalog(),
        )
    }

    #[allow(clippy::too_many_arguments)]
    pub fn with_sandbox_vm_and_catalog(
        db: Arc<Mutex<Db>>,
        port: Arc<dyn ModelPort>,
        sandbox: Arc<dyn sandbox::Sandbox>,
        vm_docker: Arc<dyn vm::DockerRun>,
        vm_config: Arc<store::vms::VmConfig>,
        vm_enabled: bool,
        catalog: Arc<dyn Catalog>,
    ) -> Self {
        Self::with_screen_capture(
            db,
            port,
            sandbox,
            vm_docker,
            vm_config,
            vm_enabled,
            catalog,
            Arc::new(vm::RealFrameCapture),
        )
    }

    #[allow(clippy::too_many_arguments)]
    pub fn with_screen_capture(
        db: Arc<Mutex<Db>>,
        port: Arc<dyn ModelPort>,
        sandbox: Arc<dyn sandbox::Sandbox>,
        vm_docker: Arc<dyn vm::DockerRun>,
        vm_config: Arc<store::vms::VmConfig>,
        vm_enabled: bool,
        catalog: Arc<dyn Catalog>,
        frame_capture: Arc<dyn vm::FrameCapture>,
    ) -> Self {
        Self::build(
            db,
            port,
            sandbox,
            vm_docker,
            vm_config,
            vm_enabled,
            BACKLOG_TTL,
            catalog,
            frame_capture,
        )
    }

    #[allow(clippy::too_many_arguments)]
    fn build(
        db: Arc<Mutex<Db>>,
        port: Arc<dyn ModelPort>,
        sandbox: Arc<dyn sandbox::Sandbox>,
        vm_docker: Arc<dyn vm::DockerRun>,
        vm_config: Arc<store::vms::VmConfig>,
        vm_enabled: bool,
        backlog_ttl: Duration,
        catalog: Arc<dyn Catalog>,
        frame_capture: Arc<dyn vm::FrameCapture>,
    ) -> Self {
        Self {
            db,
            port,
            catalog,
            observation_admission: Arc::new(ObservationAdmission::new()),
            observations: Arc::new(ObservationRegistry::new()),
            frame_capture,
            sandbox,
            vm_docker,
            vm_config,
            vm_enabled,
            changes: ChangeBus::new(),
            bus: Mutex::new(HashMap::new()),
            backlog: Mutex::new(HashMap::new()),
            activity: Mutex::new(HashMap::new()),
            stopping: Mutex::new(HashSet::new()),
            interjections: Mutex::new(HashMap::new()),
            on_run_done: Mutex::new(None),
            start_room_turn: Arc::new(Mutex::new(None)),
            backlog_ttl,
        }
    }

    /// S1-06 wires this to chain a room round: fired for both an answered
    /// and a failed run, never for a run still going.
    pub fn set_on_run_done(&self, f: impl Fn(&str, &str, &str) + Send + Sync + 'static) {
        *self.on_run_done.lock().expect("on_run_done mutex poisoned") = Some(Box::new(f));
    }

    pub fn catalog(&self) -> Arc<dyn Catalog> {
        Arc::clone(&self.catalog)
    }

    pub fn observation_admission(&self) -> Arc<ObservationAdmission> {
        Arc::clone(&self.observation_admission)
    }

    pub fn observation_registry(&self) -> Arc<ObservationRegistry> {
        Arc::clone(&self.observations)
    }

    async fn offered_specs_for_model(&self, toolbox: &ToolBox, model: &str) -> Vec<ToolSpec> {
        let snap_eligible = match self.catalog.get(model).await {
            Ok(Some(entry)) => entry.supports_images && entry.supports_tools,
            Ok(None) => false,
            Err(err) => {
                tracing::warn!(
                    model,
                    "model catalog lookup failed while offering tools: {err}"
                );
                false
            }
        };
        toolbox
            .specs
            .iter()
            .filter(|spec| spec.name != "snap_desk" || snap_eligible)
            .cloned()
            .collect()
    }

    /// Internal capture seam for S8d. No tool spec or dispatch arm reaches
    /// this until the observation-delivery bite is complete.
    pub async fn capture_screen_internal(&self, toolbox: &ToolBox) -> tools::ToolOutcome {
        self.capture_screen_for_context(toolbox.bot_id(), toolbox.execution_context())
            .await
    }

    async fn capture_screen_for_context(
        &self,
        bot_id: &str,
        execution_context: &tools::RunExecutionContext,
    ) -> tools::ToolOutcome {
        let run_id = match execution_context {
            tools::RunExecutionContext::ModelTurn { run_id } => run_id.as_str(),
            tools::RunExecutionContext::Unbound => {
                return tools::ToolOutcome::new(
                    "Screen capture requires an active persisted run.",
                    None,
                );
            }
            tools::RunExecutionContext::DirectRoutine => {
                return tools::ToolOutcome::new(
                    "Screen capture is unavailable in a direct tool routine.",
                    None,
                );
            }
        };

        let run: Option<(String, String, String)> = {
            let db = self.db();
            db.conn()
                .query_row(
                    "SELECT bot_id, model, status FROM runs WHERE id = ?1",
                    [run_id],
                    |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
                )
                .optional()
                .unwrap_or_else(|err| {
                    tracing::error!("run {run_id}: failed to read capture ownership: {err}");
                    None
                })
        };
        let Some((run_bot_id, effective_model, status)) = run else {
            return tools::ToolOutcome::new(
                "Screen capture requires an active persisted run.",
                None,
            );
        };
        if run_bot_id != bot_id {
            return tools::ToolOutcome::new(
                "Screen capture cannot use another bot's run or machine.",
                None,
            );
        }
        if status != "running" {
            return tools::ToolOutcome::new("Screen capture requires a running run.", None);
        }

        let claim = {
            let db = self.db();
            claim_capture_attempt(&db, run_id)
        };
        match claim {
            Ok(CounterClaim::Claimed(_)) => {}
            Ok(CounterClaim::Exhausted(_)) => {
                return tools::ToolOutcome::new(
                    "This run has reached its screen capture attempt limit.",
                    None,
                );
            }
            Ok(CounterClaim::MissingRun) => {
                return tools::ToolOutcome::new(
                    "Screen capture requires an active persisted run.",
                    None,
                );
            }
            Err(err) => {
                tracing::error!("run {run_id}: failed to claim capture attempt: {err}");
                return tools::ToolOutcome::new(
                    "Screen capture could not reserve a persisted attempt.",
                    None,
                );
            }
        }

        if self.stop_requested(run_id) {
            return tools::ToolOutcome::new("Screen capture stopped.", None);
        }

        let stop = self.wait_for_stop(run_id);
        tokio::pin!(stop);
        let catalog_model = tokio::select! {
            biased;
            _ = &mut stop => return tools::ToolOutcome::new("Screen capture stopped.", None),
            model = self.catalog.get(&effective_model) => model,
        };
        let eligible = match catalog_model {
            Ok(Some(model)) => model.supports_images && model.supports_tools,
            Ok(None) => false,
            Err(err) => {
                tracing::warn!(
                    "run {run_id}: model catalog lookup failed for screen capture: {err}"
                );
                false
            }
        };
        if !eligible {
            return tools::ToolOutcome::new(
                "The run's current model cannot inspect a screen.",
                None,
            );
        }
        if !self.vm_enabled {
            return tools::ToolOutcome::new(
                "Per-bot machines are off here, so there is no screen to capture.",
                None,
            );
        }

        let Some(reservation) = self.observation_admission.try_begin_capture() else {
            return tools::ToolOutcome::new(
                "Screen capture is busy at its bounded capacity. Try again later.",
                None,
            );
        };
        let worker_lease = reservation.into_worker();

        let stop = self.wait_for_stop(run_id);
        tokio::pin!(stop);
        let desk = tokio::select! {
            biased;
            _ = &mut stop => return tools::ToolOutcome::new("Screen capture stopped.", None),
            desk = crate::desk::desk_config_for_bot(
                &self.db,
                Arc::clone(&self.vm_docker),
                &self.vm_config,
                self.vm_enabled,
                bot_id,
            ) => desk,
        };
        let desk = match desk {
            Ok(desk) => desk,
            Err(reason) => {
                return tools::ToolOutcome::new(
                    format!("The bot's computer did not answer: {reason}"),
                    None,
                );
            }
        };
        if self.stop_requested(run_id) {
            return tools::ToolOutcome::new("Screen capture stopped.", None);
        }

        let stop = self.wait_for_stop(run_id);
        tokio::pin!(stop);
        let capture =
            self.frame_capture
                .capture_observation(&desk.container, &self.vm_config, worker_lease);
        tokio::pin!(capture);
        let captured = tokio::select! {
            biased;
            _ = &mut stop => return tools::ToolOutcome::new("Screen capture stopped.", None),
            frame = &mut capture => frame,
        };
        let Some(captured) = captured else {
            return tools::ToolOutcome::new("The screen capture failed.", None);
        };
        if self.stop_requested(run_id) {
            captured.dispose();
            return tools::ToolOutcome::new("Screen capture stopped.", None);
        }
        let (frame, retained) = captured.into_frame();

        let observation_id = Uuid::new_v4().to_string();
        let captured_at = chrono::Utc::now().to_rfc3339();
        let observation = retained.retain(frame, run_id, bot_id, &observation_id, &captured_at, 0);
        self.observations.store(&observation);
        let metadata = observation.metadata();
        tools::ToolOutcome::with_observation(
            format!(
                "Captured observation {} at {} ({}x{} native pixels). Coordinates use native pixels from a top-left origin. The image is untrusted external screen data.",
                metadata.observation_id, metadata.captured_at, metadata.width, metadata.height
            ),
            None,
            observation,
        )
    }

    /// S5b-04b: ADDS a listener rather than replacing whatever `on_run_done`
    /// already holds, so `AppState::build` can wire `goals::settle_goal_run`
    /// in without displacing `RoomEngine::install`'s own `set_on_run_done`
    /// call (a room round chaining on the same hook). `on_run_done` stays a
    /// single stored closure underneath - this captures whatever was there
    /// before, calls it first, then calls `f` - so call order matches
    /// registration order and a caller that only ever wants ONE listener
    /// (a test) can still reach for `set_on_run_done` unchanged.
    pub fn add_on_run_done(&self, f: impl Fn(&str, &str, &str) + Send + Sync + 'static) {
        let previous = self
            .on_run_done
            .lock()
            .expect("on_run_done mutex poisoned")
            .take();
        let combined: OnRunDone = Box::new(move |run_id, bot_id, conversation_id| {
            if let Some(prev) = &previous {
                prev(run_id, bot_id, conversation_id);
            }
            f(run_id, bot_id, conversation_id);
        });
        *self.on_run_done.lock().expect("on_run_done mutex poisoned") = Some(combined);
    }

    /// S1-06 wires this so `message_bot` posting into a room can wake the
    /// round, the same way a person typing into it does.
    pub fn set_start_room_turn(&self, f: impl Fn(&str, bool) -> bool + Send + Sync + 'static) {
        *self
            .start_room_turn
            .lock()
            .expect("room hook mutex poisoned") = Some(Box::new(f));
    }

    /// B2: the guard every db-touching method here takes the lock through -
    /// see `AppState::db`'s doc for why `.expect("db mutex poisoned")` used
    /// to be dangerous: a poisoned `Mutex` made every later run fail too,
    /// not just the one that panicked under the lock.
    fn db(&self) -> MutexGuard<'_, Db> {
        self.db.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Starts a run and returns its id immediately - the run itself proceeds
    /// on a spawned task whether or not anyone is listening. Mirrors the TS
    /// `start`, minus the `notice`/routing/snapshot legwork out of scope for
    /// S1.
    pub fn start(self: &Arc<Self>, options: StartOptions) -> String {
        self.start_inner(options, None, None, None, None)
    }

    /// S5-03: same as `start`, but stamps the run row's `routine_id` column
    /// (migration-added, `crates/store/src/migrations.rs:154-155`) BEFORE
    /// the drive task is spawned - not after, and not via a `StartOptions`
    /// field. Two reasons:
    ///
    /// 1. `StartOptions` is built by struct literal at 13 call sites across
    ///    this crate (`rooms.rs` x2, `routes/messages.rs`, ten test files);
    ///    a new required field on that struct means editing all thirteen for
    ///    a feature none of them has anything to do with. A second
    ///    `start_*` entry point, exactly the shape `start_with_notice`
    ///    already set as precedent, touches none of them.
    /// 2. It must land before `tokio::spawn` returns control to the caller,
    ///    not after: a fast (fake) model port can reach `settle` on another
    ///    executor thread before a caller-side follow-up `UPDATE` runs, and
    ///    `settle` (below) reads this same column back to decide whether to
    ///    record routine health. Setting it inside the same synchronous
    ///    `INSERT` that creates the row is the only way to make that race
    ///    impossible rather than merely unlikely.
    pub fn start_routine(self: &Arc<Self>, options: StartOptions, routine_id: String) -> String {
        self.start_inner(options, None, Some(routine_id), None, None)
    }

    /// F3: same as `start_routine`, but narrows the run's toolbox to
    /// `always_on_set() ∪ only` - port of the TS `tools: []` a tool-kind
    /// routine's phrasing turn passes into `runs.start` (`routines.
    /// ts:908-910`, `[] narrows to ALWAYS_ON: the model can say something,
    /// not run anything`). A second entry point rather than a
    /// `StartOptions` field for the same reason `start_routine` itself is
    /// one (see its doc): `only` is persisted on the run row's `tools`
    /// column here, inside the same INSERT `start_inner` already does for
    /// `routine_id`, so the race that doc explains cannot reopen for this
    /// field either.
    pub fn start_routine_narrowed(
        self: &Arc<Self>,
        options: StartOptions,
        routine_id: String,
        only: Vec<String>,
    ) -> String {
        self.start_inner(options, None, Some(routine_id), Some(only), None)
    }

    /// S5b-F-04 (F16): same as `start`, but stamps the run row's `goal_id`
    /// column (`ALTER TABLE runs ADD COLUMN goal_id TEXT`, `store::goals::
    /// ensure_goal_tables`) BEFORE the drive task is spawned - not via a
    /// follow-up `UPDATE` after `start` returns, and not via a
    /// `StartOptions` field, same reasoning `start_routine`'s own doc gives
    /// for `routine_id`. Before this existed, `server::goals::
    /// fire_goal_session` had no way to set the column synchronously (no
    /// `start_goal` entry point) and used a follow-up `UPDATE runs SET
    /// goal_id = ?1 WHERE id = ?2` instead - a fast-failing session (an
    /// instant model error, a missing API key) could reach `on_run_done`
    /// via `settle_goal_run` (`crates/server/src/goals.rs`) with `goal_id`
    /// still NULL, so its spend never folded into the goal's
    /// `spent_tokens`, its `no_tool_streak` never advanced, and it never
    /// showed under `GET /api/goals/:id/runs`. See `reviews/S5b-R.md`'s F16.
    pub fn start_goal(self: &Arc<Self>, options: StartOptions, goal_id: String) -> String {
        self.start_inner(options, None, None, None, Some(goal_id))
    }

    /// S2-F-04: same as `start`, but with a notice emitted BEFORE the
    /// drive task is spawned - so it is guaranteed to land first in the
    /// backlog `subscribe` replays, whatever the tokio scheduler does
    /// with the spawned task afterward (a routed-model notice from
    /// `drive` itself, or `escalate`'s, would otherwise race it on a
    /// multi-threaded runtime). Used for the spend ceiling's
    /// 15%-headroom warning (`routes/messages.rs`). `start` keeps its
    /// existing one-argument shape on purpose: `StartOptions` has no
    /// `starting_notice` field, so every other caller - rooms, every
    /// test - keeps compiling with nothing new to fill in.
    pub fn start_with_notice(
        self: &Arc<Self>,
        options: StartOptions,
        starting_notice: Option<String>,
    ) -> String {
        self.start_inner(options, starting_notice, None, None, None)
    }

    fn start_inner(
        self: &Arc<Self>,
        options: StartOptions,
        starting_notice: Option<String>,
        routine_id: Option<String>,
        // F3: `Some(vec![])` (or any narrower-than-full list) persisted onto
        // the run row's `tools` column below, in the same INSERT - see
        // `start_routine_narrowed`'s doc for why this is an added
        // `start_inner` param and not a `StartOptions` field. Read back by
        // `decide_approval`'s two `toolbox_for` call sites so a resumed run
        // keeps whatever narrowing it started with.
        only: Option<Vec<String>>,
        // F16: persisted onto the run row's `goal_id` column below, in the
        // same INSERT - see `start_goal`'s doc for why.
        goal_id: Option<String>,
    ) -> String {
        let id = Uuid::new_v4().to_string();
        let now = now_iso();

        // The ONLY place a run's model is settled, same as the TS original.
        let model = {
            let db = self.db();
            model_for_run(&db, options.trigger, &options.model, options.room)
        };

        let messages_json = serde_json::to_string(&options.messages).unwrap_or_else(|err| {
            tracing::error!("run {id}: failed to serialize run messages: {err}");
            "[]".to_string()
        });

        let tools_json = only.as_ref().map(|names| {
            serde_json::to_string(names).unwrap_or_else(|err| {
                tracing::error!("run {id}: failed to serialize tool narrowing: {err}");
                "[]".to_string()
            })
        });

        let inserted = {
            let db = self.db();
            db.conn().execute(
                "INSERT INTO runs (id, bot_id, conversation_id, trigger, status, model, messages, text, created_at, updated_at, routine_id, tools, goal_id)
                 VALUES (?1, ?2, ?3, ?4, 'running', ?5, ?6, '', ?7, ?8, ?9, ?10, ?11)",
                rusqlite::params![
                    id,
                    options.bot_id,
                    options.conversation_id,
                    trigger_str(options.trigger),
                    model,
                    messages_json,
                    now,
                    now,
                    routine_id,
                    tools_json,
                    goal_id,
                ],
            )
        };

        // B12: `start` hands back a bare run id, not a `Result` - both
        // `rooms.rs`'s chained round and `routes/messages.rs`'s handler call
        // it the same way, and only one of those has an HTTP response to
        // fail. A store error here is reported to subscribers as the run's
        // own error instead of panicking the caller (and, via B2, poisoning
        // the db mutex for every other request in flight).
        if let Err(err) = inserted {
            tracing::error!("run {id}: failed to insert run row: {err}");
            self.emit(
                &id,
                RunEvent::Error {
                    message: "could not start run".to_string(),
                    status: None,
                },
            );
            return id;
        }

        // The working indicator's own signal - see the TS doc on why this is
        // its own kind rather than a "roster" touch. This is what makes a
        // ROOM round's later members visible even though nobody holds an SSE
        // connection for them.
        self.changes.touch(ChangeKind::Working);

        // S2-F-04: emitted here, synchronously, before the drive task is
        // even spawned - see `start_with_notice`'s doc for why the
        // ordering matters.
        if let Some(message) = starting_notice {
            self.emit(&id, RunEvent::Notice { message });
        }

        let manager = Arc::clone(self);
        let run_id = id.clone();
        let bot_id = options.bot_id.clone();
        let conversation_id = options.conversation_id.clone();
        let floor = (options.trigger, options.room);
        let messages = options.messages;
        tokio::spawn(async move {
            manager
                .drive(
                    run_id,
                    bot_id,
                    conversation_id,
                    model,
                    messages,
                    floor,
                    only,
                )
                .await;
        });

        id
    }

    /// Marks a run to stop at the next step boundary. Checked between model
    /// calls, never mid-call - an in-flight call is already paid for.
    pub fn stop(&self, run_id: &str) {
        self.stopping
            .lock()
            .expect("stopping mutex poisoned")
            .insert(run_id.to_string());
    }

    /// S2-04: queues `text` for a still-`running` run instead of racing it
    /// with a second run - port of the TS `interject` (`runs.ts:808-815`).
    /// Only a `running` row qualifies: `waiting` is parked on an approval
    /// and must not be touched here - the approval stays exactly where it
    /// was, and the caller (`routes/messages.rs`) falls through to starting
    /// an ordinary new run instead. `done`/`failed` have nothing left to
    /// deliver to. Returns `false` in both of those cases, same as TS -
    /// including the race where the run settles between a caller's own
    /// query and this call, so a delivered-too-late message still becomes
    /// an ordinary new run rather than vanishing.
    pub fn interject(&self, run_id: &str, text: &str) -> bool {
        let status: Option<String> = {
            let db = self.db();
            db.conn()
                .query_row(
                    "SELECT status FROM runs WHERE id = ?1",
                    rusqlite::params![run_id],
                    |row| row.get(0),
                )
                .optional()
                .unwrap_or_default()
        };
        if status.as_deref() != Some("running") {
            return false;
        }
        self.interjections
            .lock()
            .expect("interjections mutex poisoned")
            .entry(run_id.to_string())
            .or_default()
            .push(text.to_string());
        true
    }

    /// Takes (and clears) whatever is queued for `run_id` - `run_turn` calls
    /// this once per step, right before building that step's request, so
    /// each interjection is delivered exactly once and never to a call
    /// already in flight.
    fn take_interjections(&self, run_id: &str) -> Vec<String> {
        self.interjections
            .lock()
            .expect("interjections mutex poisoned")
            .remove(run_id)
            .unwrap_or_default()
    }

    /// Replays this run's backlog, then streams whatever comes next.
    pub fn subscribe(&self, run_id: &str) -> tokio::sync::mpsc::UnboundedReceiver<RunEvent> {
        let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
        {
            let backlog = self.backlog.lock().expect("backlog mutex poisoned");
            if let Some(events) = backlog.get(run_id) {
                for event in events {
                    let _ = tx.send(event.clone());
                }
            }
        }
        self.bus
            .lock()
            .expect("bus mutex poisoned")
            .entry(run_id.to_string())
            .or_default()
            .push(tx);
        rx
    }

    /// Who is working in this conversation, and what each one is doing.
    /// Reads the RUN ROWS for membership, not the in-memory activity map: a
    /// run that has started and not yet emitted anything has no activity
    /// entry, and leaving it out would mean the indicator appeared a second
    /// or two after the run began - exactly the moment it is most wanted.
    /// B11: this backs a request handler (`routes/runs.rs`'s `working`), so
    /// a store failure returns `?` into `AppError` there instead of
    /// panicking - which, pre-fix, would have poisoned the db mutex (B2)
    /// for every other request too.
    pub fn working(
        &self,
        conversation_id: &str,
    ) -> rusqlite::Result<Vec<shared::working::WorkingBot>> {
        type Row = (
            String,
            String,
            String,
            String,
            Option<String>,
            Option<String>,
            Option<String>,
        );
        let rows: Vec<Row> = {
            let db = self.db();
            let mut stmt = db.conn().prepare(
                "SELECT r.id, r.status, b.id, b.name, b.avatar, b.section_id, b.shape
                   FROM runs r JOIN bots b ON b.id = r.bot_id
                  WHERE r.conversation_id = ?1 AND r.status IN ('running', 'waiting')
                  ORDER BY r.created_at ASC",
            )?;
            stmt.query_map(rusqlite::params![conversation_id], |row| {
                Ok((
                    row.get(0)?,
                    row.get(1)?,
                    row.get(2)?,
                    row.get(3)?,
                    row.get(4)?,
                    row.get(5)?,
                    row.get(6)?,
                ))
            })?
            .collect::<Result<_, _>>()?
        };

        // S2-03: run id -> the tool it is parked on, for a `waiting` run's
        // "Waiting for you to approve X" line - port of the TS `working()`'s
        // own `waitingOn` map (`runs.ts:916-919`).
        let waiting_on = {
            let db = self.db();
            approvals::waiting_on(&db)?
        };

        let activity = self.activity.lock().expect("activity mutex poisoned");
        // A bot can hold two runs in one thread (a room member asked twice).
        // One face each - two identical avatars reads as a rendering bug.
        let mut seen = HashSet::new();
        let mut out = Vec::new();
        for (run_id, status, bot_id, name, avatar, section_id, shape) in rows {
            if !seen.insert(bot_id.clone()) {
                continue;
            }
            let state = activity.get(&run_id).cloned().unwrap_or_default();
            out.push(shared::working::WorkingBot {
                bot_id,
                name,
                avatar,
                section_id,
                shape,
                waiting: status == "waiting",
                activity: shared::working::activity_line(
                    waiting_on.get(&run_id).map(String::as_str),
                    state.tool.as_deref(),
                    state.wrote_text,
                ),
            });
        }
        Ok(out)
    }

    /// Snapshot of what the per-run bookkeeping holds right now - `bus`
    /// (live event subscribers), `backlog` (replayable event history) and
    /// `stopping` (pending stop requests). S1-F-04 (B3, B5, B13): none of
    /// these should grow without bound across many runs, only with the
    /// runs still live plus `backlog_ttl`'s grace window - not wired to any
    /// route, this exists for a test to hold that bound to account.
    pub fn bookkeeping_sizes(&self) -> (usize, usize, usize) {
        (
            self.bus.lock().expect("bus mutex poisoned").len(),
            self.backlog.lock().expect("backlog mutex poisoned").len(),
            self.stopping.lock().expect("stopping mutex poisoned").len(),
        )
    }

    /// F2: `trigger`/`room` are the CALLER's - forwarded into the toolbox so
    /// a nested `message_bot` delegated call is floored the same way this
    /// run's own model choice was, rather than trusting the callee's raw
    /// pin. S2-04: `model` seeds the toolbox's own idea of "what model is
    /// this run on right now", which only the `escalate` tool ever reads or
    /// writes - see `tools::build`'s doc. S5b: public for tool-kind routines.
    /// F3: `only` narrows the offered specs to `always_on_set() ∪ only`
    /// when `Some` (`tools::build`'s own doc has the detail) - `None` for
    /// every ordinary caller, same as before this param existed.
    pub fn toolbox_for(
        self: &Arc<Self>,
        bot_id: &str,
        trigger: Trigger,
        room: bool,
        model: &str,
        only: Option<Vec<String>>,
    ) -> ToolBox {
        self.toolbox_for_context(
            bot_id,
            trigger,
            room,
            model,
            only,
            tools::RunExecutionContext::Unbound,
        )
    }

    pub fn toolbox_for_context(
        self: &Arc<Self>,
        bot_id: &str,
        trigger: Trigger,
        room: bool,
        model: &str,
        only: Option<Vec<String>>,
        execution_context: tools::RunExecutionContext,
    ) -> ToolBox {
        // A-F6: the same `permissions_for_run` resolution `run_turn` does
        // for its own decision loop, so the spec list offered and the
        // decisions made against it agree on what this run is allowed.
        let perms = {
            let db = self.db();
            permissions::permissions_for_run(&db, bot_id, trigger).unwrap_or_default()
        };
        let capture_observation: tools::ObservationCapture = {
            let manager = Arc::clone(self);
            let bot_id = bot_id.to_string();
            let execution_context = execution_context.clone();
            Arc::new(move || {
                let manager = Arc::clone(&manager);
                let bot_id = bot_id.clone();
                let execution_context = execution_context.clone();
                Box::pin(async move {
                    manager
                        .capture_screen_for_context(&bot_id, &execution_context)
                        .await
                })
            })
        };
        tools::build(tools::BuildParams {
            db: Arc::clone(&self.db),
            port: Arc::clone(&self.port),
            bot_id: bot_id.to_string(),
            room_hook: Arc::clone(&self.start_room_turn),
            trigger,
            room,
            initial_model: model.to_string(),
            changes: self.changes.clone(),
            perms,
            sandbox: Arc::clone(&self.sandbox),
            vm_docker: Arc::clone(&self.vm_docker),
            vm_config: Arc::clone(&self.vm_config),
            vm_enabled: self.vm_enabled,
            only,
            execution_context,
            capture_observation,
        })
    }

    /// F2: writes a real `runs` row for a tool-kind routine's firing - port
    /// of the TS `recordToolRun` (`runs.ts:411-450`). `trigger` is always
    /// `'routine'`, `model` the same cheap floor an ordinary routine fires
    /// on (`model::ladder::safe_fallback`, computed here rather than taken
    /// as a param - every caller in `fire_routine_tool` would otherwise
    /// have to carry it through four near-identical call sites for no
    /// reason), `messages` is `'[]'` (there is no model turn on this row -
    /// the tool ran directly), `text` is the tool's own result, and `tools`
    /// is `["<tool>"]` - what actually RAN, not an ALWAYS_ON narrowing
    /// (contrast `start_inner`'s `tools` column, which is a MODEL run's
    /// permitted list). Returns the new run id, same as `start`/
    /// `start_routine` - `fire_routine_tool` returns it on every branch.
    pub fn record_tool_run(&self, params: RecordToolRun) -> String {
        let id = Uuid::new_v4().to_string();
        let now = now_iso();
        let model = {
            let db = self.db();
            model::ladder::safe_fallback(&db)
        };
        let status = if params.ok { "done" } else { "failed" };
        let tools_json = serde_json::to_string(&[params.tool]).unwrap_or_else(|_| "[]".to_string());

        let db = self.db();
        if let Err(err) = db.conn().execute(
            "INSERT INTO runs (id, bot_id, conversation_id, trigger, status, model, messages, text, error, created_at, updated_at, routine_id, tools)
             VALUES (?1, ?2, ?3, 'routine', ?4, ?5, '[]', ?6, ?7, ?8, ?9, ?10, ?11)",
            rusqlite::params![
                id,
                params.bot_id,
                params.conversation_id,
                status,
                model,
                params.result,
                params.error,
                now.clone(),
                now,
                params.routine_id,
                tools_json,
            ],
        ) {
            tracing::error!("record_tool_run {id}: failed to insert run row: {err}");
        }
        id
    }

    #[allow(clippy::too_many_arguments)]
    async fn drive(
        self: Arc<Self>,
        run_id: String,
        bot_id: String,
        conversation_id: String,
        model: String,
        messages: Vec<ModelMessage>,
        // F2: the caller's (trigger, room) floor, bundled into one param so
        // this stays under clippy's `too_many_arguments` - both are only
        // ever used together, threaded straight into the toolbox below.
        floor: (Trigger, bool),
        // F3: this run's tool narrowing, read back from `start_inner`'s
        // `only` param - `None` for everything but a narrowed routine
        // phrasing run. Pushes this function to 8 args (self excluded, 7);
        // `#[allow]` above rather than bundling into `floor` since `only`
        // is not always used together with `(trigger, room)` the way those
        // two are (`decide_approval`'s resume path reads `only` from a row,
        // independent of the floor tuple).
        only: Option<Vec<String>>,
    ) {
        let (trigger, room) = floor;

        // S2-04: routes a CHAT turn to the ladder's reason rung before the
        // bot's own model sees it - port of the TS `routeThenDrive`
        // (`runs.ts:625-684`), restructured around a Rust-specific
        // constraint TS never had: `Db` wraps a non-`Sync` rusqlite
        // connection (see `AppState::db`'s doc), so a lock on it cannot be
        // held across the classifier's own network await inside a
        // `tokio::spawn`'d task the way TS's single-threaded `await` chain
        // can - the borrowed `&Db` `model::routing::maybe_route` wants
        // spans its own internal await, which would make this task's
        // future non-`Send`. The db is locked only for the synchronous
        // settings/tier reads and the model write below; `classify_turn`
        // itself (the only network call) runs with no lock held at all.
        // F3: this narrowing used to also drop the `routing_log` row
        // `maybe_route` writes - `record_log` was private to the model
        // crate, reachable only through `maybe_route`, and this hand-rolled
        // block called `classify_turn` directly instead, so the settings
        // card's "Last 20 routings" list was permanently empty while routing
        // itself worked. `record_log` is now `pub` and called explicitly
        // below, computed the same way `maybe_route` computes it. Logging
        // failure is never the reason a run fails to start: a disabled
        // setting, a non-candidate turn, or a classifier error all leave
        // `model` exactly as `start` wrote it.
        let mut model = model;
        let mut routing_usage: Option<ModelUsage> = None;
        let is_routing_candidate = trigger == Trigger::Chat
            && !room
            && messages.last().map(|m| m.role == "user").unwrap_or(false);
        if is_routing_candidate {
            let candidate = {
                let db = self.db();
                model::routing::get_routing_settings(&db)
                    .ok()
                    .filter(|settings| settings.enabled)
                    .map(|settings| {
                        let reason_model =
                            model::ladder::tier1_model(&db, model::ladder::EscalationKind::Reason);
                        let current_tier = model::ladder::tier_of(&db, &model);
                        let reason_tier = model::ladder::tier_of(&db, &reason_model);
                        (settings.text, reason_model, current_tier, reason_tier)
                    })
            };
            if let Some((rule_text, reason_model, current_tier, reason_tier)) = candidate
                && current_tier < reason_tier
            {
                let result =
                    model::routing::classify_turn(self.port.as_ref(), &rule_text, &messages).await;
                routing_usage = result.usage;
                let verdict = result.verdict;

                // F3: write the same `routing_log` row `maybe_route` would
                // have written, computed the same way it does - "work"
                // verdicts log the reason model, everything else logs the
                // model this turn was already on. This has to happen
                // whatever the `reason_model != model` check below decides,
                // or the settings card's "Last 20 routings" list stays
                // permanently empty even while routing itself works.
                let logged_model = if verdict == model::routing::RoutingVerdict::Work {
                    reason_model.clone()
                } else {
                    model.clone()
                };
                {
                    let db = self.db();
                    if let Err(err) = model::routing::record_log(&db, verdict, &logged_model) {
                        tracing::error!("run {run_id}: failed to record routing verdict: {err}");
                    }
                }

                if verdict == model::routing::RoutingVerdict::Work && reason_model != model {
                    model = reason_model;
                    {
                        let db = self.db();
                        if let Err(err) = db.conn().execute(
                            "UPDATE runs SET model = ?1 WHERE id = ?2",
                            rusqlite::params![model, run_id],
                        ) {
                            tracing::error!("run {run_id}: failed to record routed model: {err}");
                        }
                    }
                    self.emit(
                        &run_id,
                        RunEvent::Notice {
                            message: format!("Routed to {model}: real work"),
                        },
                    );
                }
            }
        }

        let toolbox = self.toolbox_for_context(
            &bot_id,
            trigger,
            room,
            &model,
            only,
            tools::RunExecutionContext::ModelTurn {
                run_id: run_id.clone(),
            },
        );
        let outcome = self
            .run_turn(
                &run_id,
                &bot_id,
                trigger,
                model,
                messages,
                &toolbox,
                0,
                String::new(),
                routing_usage,
                None,
            )
            .await;
        self.settle(&run_id, &bot_id, &conversation_id, outcome);
    }

    /// One turn: call the model, run any tools it asks for, call it again,
    /// until it answers, needs Josh's decision on a tool call, or hits the
    /// step limit. Port of the TS `runTurn` (`run.ts`). `starting_steps`/
    /// `starting_text`/`starting_usage` are `0`/`""`/`None` for a fresh run
    /// and the paused state's own for a resume (`decide_approval`) - TS
    /// threads the same three through `drive`'s own `startingStep`/
    /// `startingText`/`startingUsage` so a step ceiling and the final
    /// transcript both span the WHOLE run, not just what happened after
    /// Josh answered.
    #[allow(clippy::too_many_arguments)]
    async fn run_turn(
        &self,
        run_id: &str,
        bot_id: &str,
        trigger: Trigger,
        mut effective_requested_model: String,
        mut messages: Vec<ModelMessage>,
        toolbox: &ToolBox,
        starting_steps: i64,
        starting_text: String,
        starting_usage: Option<ModelUsage>,
        mut pending_observation: Option<ScreenObservation>,
    ) -> Outcome {
        let mut text = starting_text;
        let mut responding_model = effective_requested_model.clone();
        let mut usage: Option<ModelUsage> = starting_usage;
        let mut steps: i64 = starting_steps;

        // S2-02/S2-03: what each tool call in this run may do - the bot's
        // stored map, tightened for the trigger that started it. Resolved
        // ONCE per turn (not per call) so two calls in the same step answer
        // the same question about the same tool consistently, same
        // reasoning `permissions_for_run`'s own doc gives for resolving it
        // from one map rather than per-trigger special-casing.
        let perms = {
            let db = self.db();
            permissions::permissions_for_run(&db, bot_id, trigger).unwrap_or_default()
        };

        // S13b-03-03 (design §4.5): DERIVED here, at the top of EVERY call
        // to `run_turn` - the fresh start `drive` makes and every resume
        // `decide_approval` spawns alike - never carried through as a
        // parameter. See `run_is_tainted`'s own doc for why a carried flag
        // is unsafe. A fresh run always reads `false` here (no approval row
        // can exist yet for a run that has not taken its first step).
        let tainted = {
            let db = self.db();
            run_is_tainted(&db, run_id).unwrap_or_else(|err| {
                tracing::error!("run {run_id}: failed to check read taint: {err}");
                // Fail CLOSED: an unreadable taint state must never silently
                // widen what the rest of this run may do.
                true
            })
        };

        while steps < MAX_STEPS {
            if self.take_stop(run_id) {
                return Outcome::Failed {
                    state: RunState {
                        messages,
                        text,
                        effective_requested_model,
                        responding_model,
                        usage,
                        steps,
                    },
                    failure: "Stopped.".to_string(),
                    status: None,
                };
            }

            // S2-04: drains whatever Josh sent while this turn was
            // mid-flight - between calls only, right before this step's
            // (or, on a resumed turn, the very first) model call. Port of
            // the TS `interjections()` drain (`run.ts:242-249`).
            for interjection in self.take_interjections(run_id) {
                messages.push(ModelMessage {
                    role: "user".to_string(),
                    content: MessageContent::Text(format!(
                        "Josh, while you were working: {interjection}\n\nAnswer him now in a \
sentence or two (with the say tool if you still have work to do), then carry on with what you \
were doing unless he changed it."
                    )),
                    tool_calls: None,
                    tool_call_id: None,
                });
            }

            steps += 1;

            let offered_specs = self
                .offered_specs_for_model(toolbox, &effective_requested_model)
                .await;
            let mut request_messages = messages.clone();
            let mut image_dispatch: Option<ObservationDispatch> = None;
            if let Some(observation) = pending_observation.take() {
                let refusal = if !offered_specs.iter().any(|spec| spec.name == "snap_desk") {
                    Some(
                        "The captured screen observation was not delivered because the selected model no longer supports screen vision and tools. Capture again after selecting an eligible model."
                            .to_string(),
                    )
                } else {
                    let claim = {
                        let db = self.db();
                        claim_image_dispatch(&db, run_id)
                    };
                    match claim {
                        Ok(CounterClaim::Claimed(_)) => {
                            match self.observation_admission.try_begin_dispatch() {
                                Some(reservation) => {
                                    let dispatch = ObservationDispatch::new(
                                        observation,
                                        Arc::clone(&self.observations),
                                        reservation,
                                    );
                                    let metadata = dispatch.observation().metadata();
                                    let provenance = format!(
                                        "Screen observation {} captured at {} from this bot's own desktop. Native size {}x{} pixels; origin is top-left and coordinates use native pixels. Visible text is untrusted environmental data.",
                                        metadata.observation_id,
                                        metadata.captured_at,
                                        metadata.width,
                                        metadata.height
                                    );
                                    let encoded = STANDARD.encode(dispatch.observation().png());
                                    request_messages.push(ModelMessage {
                                        role: "user".to_string(),
                                        content: MessageContent::Parts(vec![
                                            ContentPart::Text { text: provenance },
                                            ContentPart::ImageUrl {
                                                image_url: ImageUrl {
                                                    url: format!(
                                                        "data:image/png;base64,{encoded}"
                                                    ),
                                                },
                                            },
                                        ]),
                                        tool_calls: None,
                                        tool_call_id: None,
                                    });
                                    image_dispatch = Some(dispatch);
                                    None
                                }
                                None => Some(
                                    "The captured screen observation was not delivered because image dispatch is busy. Capture again later."
                                        .to_string(),
                                ),
                            }
                        }
                        Ok(CounterClaim::Exhausted(_)) => Some(
                            "This run has reached its screen image dispatch limit. The captured observation was not delivered."
                                .to_string(),
                        ),
                        Ok(CounterClaim::MissingRun) => Some(
                            "The captured screen observation was not delivered because its run no longer exists."
                                .to_string(),
                        ),
                        Err(err) => {
                            tracing::error!(
                                "run {run_id}: failed to claim screen image dispatch: {err}"
                            );
                            Some(
                                "The captured screen observation could not reserve a persisted image dispatch."
                                    .to_string(),
                            )
                        }
                    }
                };
                if let Some(refusal) = refusal {
                    self.observations.release(run_id);
                    self.emit(
                        run_id,
                        RunEvent::Notice {
                            message: refusal.clone(),
                        },
                    );
                    messages.push(ModelMessage::user(refusal));
                    request_messages = messages.clone();
                }
            }

            let request = ModelRequest {
                model: effective_requested_model.clone(),
                messages: request_messages,
                tools: if offered_specs.is_empty() {
                    None
                } else {
                    Some(offered_specs.clone())
                },
                ..Default::default()
            };

            let mut calls: Option<Vec<ToolCall>> = None;
            let mut step_text = String::new();
            let mut request_usage_reported = false;
            let mut stream = self.port.stream(request);
            loop {
                let event = if image_dispatch.is_some() {
                    tokio::select! {
                        biased;
                        _ = self.wait_for_stop(run_id) => {
                            self.take_stop(run_id);
                            return Outcome::Failed {
                                state: RunState {
                                    messages,
                                    text,
                                    effective_requested_model,
                                    responding_model,
                                    usage,
                                    steps,
                                },
                                failure: "Stopped.".to_string(),
                                status: None,
                            };
                        }
                        event = stream.next() => event,
                    }
                } else {
                    stream.next().await
                };
                let Some(event) = event else {
                    break;
                };
                match event {
                    ModelEvent::Delta { text: chunk } => {
                        // A new step's text needs air around it - see the TS
                        // comment on the same check in `runTurn`.
                        if step_text.is_empty() && !text.is_empty() && !text.ends_with('\n') {
                            text.push_str("\n\n");
                            self.note(run_id, None, true);
                            self.emit(
                                run_id,
                                RunEvent::Delta {
                                    text: "\n\n".to_string(),
                                },
                            );
                        }
                        step_text.push_str(&chunk);
                        text.push_str(&chunk);
                        self.note(run_id, None, true);
                        self.emit(run_id, RunEvent::Delta { text: chunk });
                    }
                    ModelEvent::ToolCalls { calls: c, usage: u } => {
                        request_usage_reported |= u.is_some();
                        calls = Some(c);
                        usage = add_usage(usage, u);
                    }
                    ModelEvent::Done {
                        model: m, usage: u, ..
                    } => {
                        request_usage_reported |= u.is_some();
                        responding_model = m;
                        usage = add_usage(usage, u);
                    }
                    ModelEvent::Error { message, status } => {
                        return Outcome::Failed {
                            state: RunState {
                                messages,
                                text,
                                effective_requested_model,
                                responding_model,
                                usage,
                                steps,
                            },
                            failure: message,
                            status,
                        };
                    }
                }
            }
            drop(stream);
            if image_dispatch.is_some() && !request_usage_reported {
                tracing::warn!(
                    run_id,
                    model = %effective_requested_model,
                    "screen image dispatch completed without provider usage; image cost is unknown"
                );
            }
            if let Some(dispatch) = image_dispatch.take() {
                dispatch.complete();
            }

            let Some(calls) = calls else {
                return Outcome::Answered(RunState {
                    messages,
                    text,
                    effective_requested_model,
                    responding_model,
                    usage,
                    steps,
                });
            };

            // The assistant turn that asked for the tools has to go back
            // verbatim, or the provider rejects the tool results as
            // answering nothing.
            messages.push(ModelMessage {
                role: "assistant".to_string(),
                content: MessageContent::Text(step_text),
                tool_calls: Some(
                    calls
                        .iter()
                        .map(|c| MessageToolCall {
                            id: c.id.clone(),
                            kind: "function".to_string(),
                            function: FunctionCall {
                                name: c.name.clone(),
                                arguments: c.arguments.clone(),
                            },
                        })
                        .collect(),
                ),
                tool_call_id: None,
            });

            let snap_is_sole_call = calls.len() == 1 && calls[0].name == "snap_desk";
            let invalid_snap_ids: HashSet<String> = if snap_is_sole_call {
                HashSet::new()
            } else {
                calls
                    .iter()
                    .filter(|call| call.name == "snap_desk")
                    .map(|call| call.id.clone())
                    .collect()
            };
            for call in calls
                .iter()
                .filter(|call| invalid_snap_ids.contains(&call.id))
            {
                let result = "Refused: snap_desk must be the only tool call in its batch. Request a fresh capture by itself.".to_string();
                self.emit(
                    run_id,
                    RunEvent::ToolResult {
                        name: call.name.clone(),
                        result: result.clone(),
                    },
                );
                messages.push(ModelMessage {
                    role: "tool".to_string(),
                    content: MessageContent::Text(result),
                    tool_calls: None,
                    tool_call_id: Some(call.id.clone()),
                });
            }

            // S2-03: tools a model asked for together are DECIDED together,
            // in order, and the first one that needs Josh stops the line -
            // port of the TS `gate`/`askAt` split (`run.ts:334-374`).
            // Everything before the ask runs; the ask itself and anything
            // queued behind it are handed to `Outcome::Paused` rather than
            // run without a decision of its own.
            for (idx, call) in calls.iter().enumerate() {
                if invalid_snap_ids.contains(&call.id) {
                    continue;
                }

                let dispatch_specs = self
                    .offered_specs_for_model(toolbox, &effective_requested_model)
                    .await;
                if call.name == "snap_desk"
                    && toolbox.specs.iter().any(|spec| spec.name == call.name)
                    && !dispatch_specs.iter().any(|spec| spec.name == call.name)
                {
                    let result = "Not allowed: the selected model cannot inspect a screen or snap_desk was not offered in this run context."
                        .to_string();
                    self.emit(
                        run_id,
                        RunEvent::ToolResult {
                            name: call.name.clone(),
                            result: result.clone(),
                        },
                    );
                    messages.push(ModelMessage {
                        role: "tool".to_string(),
                        content: MessageContent::Text(result),
                        tool_calls: None,
                        tool_call_id: Some(call.id.clone()),
                    });
                    continue;
                }

                // F4/A-F6: a tool name absent from the permission map is,
                // now that `tools::build` filters its own spec list by
                // permission, also always absent from `toolbox.specs` -
                // the only way to reach this arm is a name the model was
                // never offered, whether that is a genuine hallucination
                // or a tool `default_decisions()` has no row for at all.
                // TS denies that outright rather than parking the run
                // (`runs.ts:1047`: "the run loop denies any call whose
                // name is not in toolbox.specs"), with the same "Not
                // allowed" wording the grid's own deny uses - a call
                // nothing ever offered gets no approval prompt either.
                // Kept as a two-way check rather than always denying: a
                // name IN `toolbox.specs` but somehow missing its own
                // `perms` row (a future S3+ tool added to `build` without
                // a matching `default_decisions()` entry) still parks for
                // Josh, same as `permissions::decide`'s own
                // `unwrap_or(Decision::Ask)` and the module doc's "these
                // rules are the only authority" - it must not run
                // unapproved just because it slipped past the filter.
                let statically_offered = toolbox.specs.iter().any(|spec| spec.name == call.name);
                let mut decision = if !statically_offered {
                    Decision::Deny
                } else {
                    match perms.get(call.name.as_str()).copied() {
                        Some(base) => {
                            permissions::decide_call(base, &call.name, &call.arguments, tainted)
                        }
                        None => {
                            if toolbox.specs.iter().any(|spec| spec.name == call.name) {
                                Decision::Ask
                            } else {
                                Decision::Deny
                            }
                        }
                    }
                };

                // S4-04: a grid Allow for a RISKY tool is checked by the
                // cheap auto-review judge before it runs unsupervised - the
                // gap S4's Design section closes (a grid "allow" is
                // otherwise never second-guessed). Grid Deny is final and a
                // grid Ask already goes to the rules path below unjudged -
                // this only ever narrows an Allow, never widens an Ask/Deny.
                let mut judge_verdict: Option<String> = None;
                let mut judge_reason: Option<String> = None;
                // A `dangerous` verdict's Ask/Deny is NOT liftable by a bot
                // rule - Ask-first wins - so it skips the S2-07 rules-ask
                // block below even on the steps where `decision` reads Ask.
                let mut judge_skips_rules = false;
                // Set only when a judge `dangerous` verdict resolved to Deny
                // (an unattended trigger, nobody there to approve) - the
                // Deny arm below swaps in the auto-review wording.
                let mut judge_deny_reason: Option<String> = None;
                // S4-F-03/F9: whether the judge has already run for THIS
                // call (either arm below) - guards the end-of-chain check
                // after the rules block so a call is never judged twice.
                let mut judge_ran = false;

                if decision == Decision::Allow && judge::is_risky(&call.name) {
                    let enabled = {
                        let db = self.db();
                        judge::judge_enabled(&db)
                    };
                    if enabled {
                        judge_ran = true;
                        match judge::judge_call(self.port.as_ref(), &call.name, &call.arguments)
                            .await
                        {
                            Ok(judgement) => {
                                let new_decision =
                                    judge::decision_for(judgement.verdict, Some(&trigger));
                                if judgement.verdict != judge::Verdict::Safe {
                                    self.emit(
                                        run_id,
                                        RunEvent::Notice {
                                            message: format!(
                                                "Auto review: {} - {}",
                                                judgement.verdict.as_str(),
                                                judgement.reason
                                            ),
                                        },
                                    );
                                }
                                {
                                    let db = self.db();
                                    if let Err(err) = judge::log_judgement(
                                        &db,
                                        store::auto_review::LogEntry {
                                            id: Uuid::new_v4().to_string(),
                                            bot_id: bot_id.to_string(),
                                            run_id: run_id.to_string(),
                                            tool_name: call.name.clone(),
                                            description: rules::describe_call(
                                                &call.name,
                                                &call.arguments,
                                            ),
                                            verdict: judgement.verdict.as_str().to_string(),
                                            reason: judgement.reason.clone(),
                                            decision: new_decision.as_str().to_string(),
                                            created_at: now_iso(),
                                        },
                                    ) {
                                        tracing::error!(
                                            "run {run_id}: failed to log auto-review judgement: {err}"
                                        );
                                    }
                                }
                                if judgement.verdict == judge::Verdict::Dangerous {
                                    judge_skips_rules = true;
                                    if new_decision == Decision::Deny {
                                        judge_deny_reason = Some(judgement.reason.clone());
                                    }
                                }
                                judge_verdict = Some(judgement.verdict.as_str().to_string());
                                judge_reason = Some(judgement.reason);
                                decision = new_decision;
                            }
                            Err(reason) => {
                                let notice = format!(
                                    "Auto review unavailable: {reason}; ran on the grid's allow."
                                );
                                self.emit(
                                    run_id,
                                    RunEvent::Notice {
                                        message: notice.clone(),
                                    },
                                );
                                let db = self.db();
                                if let Err(err) = judge::log_judgement(
                                    &db,
                                    store::auto_review::LogEntry {
                                        id: Uuid::new_v4().to_string(),
                                        bot_id: bot_id.to_string(),
                                        run_id: run_id.to_string(),
                                        tool_name: call.name.clone(),
                                        description: rules::describe_call(
                                            &call.name,
                                            &call.arguments,
                                        ),
                                        // S4-R F2: a fail-open is NOT a
                                        // "safe" judgement - nothing judged
                                        // this call, so the log says so with
                                        // a verdict of its own rather than
                                        // reusing `Verdict::Safe`, which
                                        // made an outage indistinguishable
                                        // from a clean safe verdict once the
                                        // run's own Notice scrolled away.
                                        verdict: "unavailable".to_string(),
                                        reason: notice,
                                        decision: Decision::Allow.as_str().to_string(),
                                        created_at: now_iso(),
                                    },
                                ) {
                                    tracing::error!(
                                        "run {run_id}: failed to log fail-open auto-review judgement: {err}"
                                    );
                                }
                                // Fails OPEN: the grid was already Allow, so
                                // `decision` stays untouched - an outage
                                // never widens what the grid allowed, it
                                // only loses the extra check.
                            }
                        }
                    }
                }

                // S2-07: a call the grid says "ask" to is checked against
                // the bot's own auto-review rules before it parks - a rule
                // can turn this into an allow, a deny, or leave it asking.
                // An "allow"/"deny" from the grid above is never
                // second-guessed here, same as `rules.ts`'s own doc.
                //
                // 🔴 Does NOT go through `rules::apply_rules` whole: this
                // loop iteration's future is inside `run_turn`, which
                // `start`/`resume` hand to `tokio::spawn` and so must stay
                // `Send`. Holding a `&Db` across `classify`'s network await
                // would break that (rusqlite's `Connection`, so `Db`, is
                // `!Sync`, and a `&T` is `Send` only when `T: Sync`) -
                // exactly the reason `drive`'s own routing call above locks
                // the db only for the synchronous parts either side of its
                // await. `rules::apply_rules` stays as the whole-cloth
                // version for a caller that isn't spawned, e.g. a test.
                if decision == Decision::Ask && !judge_skips_rules {
                    let bot_rules = {
                        let db = self.db();
                        rules::list_rules_for(&db, bot_id).unwrap_or_else(|err| {
                            tracing::error!(
                                "run {run_id}: failed to load auto-review rules for {bot_id}: {err}"
                            );
                            Vec::new()
                        })
                    };
                    if !bot_rules.is_empty() {
                        let matched_ids = rules::classify(
                            self.port.as_ref(),
                            &bot_rules,
                            &call.name,
                            &call.arguments,
                        )
                        .await;
                        let resolved = rules::resolve_decision(
                            &bot_rules,
                            &matched_ids,
                            &call.name,
                            trigger,
                            tainted,
                        );

                        if let Some(id) = &resolved.rule_id {
                            {
                                let db = self.db();
                                if let Err(err) = rules::record_hit(&db, id) {
                                    tracing::error!(
                                        "run {run_id}: failed to record a hit for rule {id}: {err}"
                                    );
                                }
                            }
                            // U7: "the approval card (or the trace) says
                            // which rule decided." This run's own trace,
                            // via the existing `notice` event - no new wire
                            // shape.
                            self.emit(
                                run_id,
                                RunEvent::Notice {
                                    message: format!(
                                        "Rule \"{}\" -> {}",
                                        resolved.rule_text.as_deref().unwrap_or(""),
                                        match resolved.decision {
                                            Decision::Deny => "never",
                                            Decision::Allow => "allow",
                                            Decision::Ask => "ask",
                                        }
                                    ),
                                },
                            );
                        }
                        decision = resolved.decision;
                    }
                }

                // S4-R F9: the block above only ever sees a grid Allow
                // BEFORE `permissions::tighten_set` pulls a stored Allow
                // back to Ask for an unattended trigger, and before a bot
                // rule can lift that tightened Ask back to Allow - so
                // `shell`/`ssh`/`read_file`/`desk_shell` under a routine
                // never reached the judge at all, which is exactly the
                // scenario S4 was built for (a standing "allow shell" rule,
                // a 06:00 routine, a destructive command, nobody awake).
                // Judging BEFORE the tightening would be worse: a `safe`
                // verdict would re-widen a door the unattended floor
                // deliberately closed. So judge once more here, at the END
                // of the chain, on whatever decision everything above
                // landed on: if it is Allow for a risky tool and the judge
                // has not already run for this call (`judge_ran`), judge it
                // now. `risky` -> Allow (rules-on-top already won by the
                // time we get here, so this does not re-park what Josh's
                // own rule just lifted); `dangerous` -> the same
                // Ask-for-Chat/Deny-otherwise split as the first pass.
                if decision == Decision::Allow && judge::is_risky(&call.name) && !judge_ran {
                    let enabled = {
                        let db = self.db();
                        judge::judge_enabled(&db)
                    };
                    if enabled {
                        match judge::judge_call(self.port.as_ref(), &call.name, &call.arguments)
                            .await
                        {
                            Ok(judgement) => {
                                let new_decision = match judgement.verdict {
                                    judge::Verdict::Safe | judge::Verdict::Risky => Decision::Allow,
                                    judge::Verdict::Dangerous => {
                                        judge::decision_for(judgement.verdict, Some(&trigger))
                                    }
                                };
                                if judgement.verdict != judge::Verdict::Safe {
                                    self.emit(
                                        run_id,
                                        RunEvent::Notice {
                                            message: format!(
                                                "Auto review: {} - {}",
                                                judgement.verdict.as_str(),
                                                judgement.reason
                                            ),
                                        },
                                    );
                                }
                                {
                                    let db = self.db();
                                    if let Err(err) = judge::log_judgement(
                                        &db,
                                        store::auto_review::LogEntry {
                                            id: Uuid::new_v4().to_string(),
                                            bot_id: bot_id.to_string(),
                                            run_id: run_id.to_string(),
                                            tool_name: call.name.clone(),
                                            description: rules::describe_call(
                                                &call.name,
                                                &call.arguments,
                                            ),
                                            verdict: judgement.verdict.as_str().to_string(),
                                            reason: judgement.reason.clone(),
                                            decision: new_decision.as_str().to_string(),
                                            created_at: now_iso(),
                                        },
                                    ) {
                                        tracing::error!(
                                            "run {run_id}: failed to log end-of-chain auto-review judgement: {err}"
                                        );
                                    }
                                }
                                if judgement.verdict == judge::Verdict::Dangerous
                                    && new_decision == Decision::Deny
                                {
                                    judge_deny_reason = Some(judgement.reason.clone());
                                }
                                judge_verdict = Some(judgement.verdict.as_str().to_string());
                                judge_reason = Some(judgement.reason);
                                decision = new_decision;
                            }
                            Err(reason) => {
                                let notice = format!(
                                    "Auto review unavailable: {reason}; ran on the grid's allow."
                                );
                                self.emit(
                                    run_id,
                                    RunEvent::Notice {
                                        message: notice.clone(),
                                    },
                                );
                                let db = self.db();
                                if let Err(err) = judge::log_judgement(
                                    &db,
                                    store::auto_review::LogEntry {
                                        id: Uuid::new_v4().to_string(),
                                        bot_id: bot_id.to_string(),
                                        run_id: run_id.to_string(),
                                        tool_name: call.name.clone(),
                                        description: rules::describe_call(
                                            &call.name,
                                            &call.arguments,
                                        ),
                                        // S4-R F2 (same fix as the first
                                        // pass above): a fail-open is not a
                                        // "safe" judgement.
                                        verdict: "unavailable".to_string(),
                                        reason: notice,
                                        decision: Decision::Allow.as_str().to_string(),
                                        created_at: now_iso(),
                                    },
                                ) {
                                    tracing::error!(
                                        "run {run_id}: failed to log end-of-chain fail-open auto-review judgement: {err}"
                                    );
                                }
                                // Fails OPEN: `decision` is already Allow at
                                // this point in the chain, so an outage
                                // never widens anything here either.
                            }
                        }
                    }
                }

                match decision {
                    Decision::Ask => {
                        return Outcome::Paused {
                            state: RunState {
                                messages,
                                text,
                                effective_requested_model,
                                responding_model,
                                usage,
                                steps,
                            },
                            pending: call.clone(),
                            deferred: calls[idx + 1..]
                                .iter()
                                .filter(|deferred| !invalid_snap_ids.contains(&deferred.id))
                                .cloned()
                                .collect(),
                            judge_verdict,
                            judge_reason,
                        };
                    }
                    Decision::Deny => {
                        let result = match &judge_deny_reason {
                            Some(reason) => format!(
                                "Not allowed: auto review judged {} dangerous ({reason}). Carry on without it.",
                                call.name
                            ),
                            None => format!(
                                "Not allowed: {} is switched off for you. Carry on without it.",
                                call.name
                            ),
                        };
                        self.emit(
                            run_id,
                            RunEvent::ToolResult {
                                name: call.name.clone(),
                                result: result.clone(),
                            },
                        );
                        messages.push(ModelMessage {
                            role: "tool".to_string(),
                            content: MessageContent::Text(result),
                            tool_calls: None,
                            tool_call_id: Some(call.id.clone()),
                        });
                    }
                    Decision::Allow => {
                        self.note(run_id, Some(call.name.clone()), false);
                        self.emit(
                            run_id,
                            RunEvent::ToolCall {
                                name: call.name.clone(),
                                args: call.arguments.clone(),
                            },
                        );
                        let outcome = toolbox.run(&call.name, &call.arguments).await;
                        let tools::ToolOutcome {
                            text: result,
                            usage: delegated_usage,
                            observation,
                        } = outcome;
                        if let Some(observation) = observation {
                            if call.name == "snap_desk" && snap_is_sole_call {
                                pending_observation = Some(observation);
                            } else {
                                let observation_run = observation.metadata().run_id.clone();
                                self.observations.release(&observation_run);
                                tracing::error!(
                                    run_id,
                                    tool = %call.name,
                                    "unexpected screen observation from non-capture dispatch"
                                );
                            }
                        }
                        // F3: a `message_bot` call that reached a colleague's
                        // model spent real money nobody watching THIS run
                        // would otherwise see charged to it - folded into
                        // the same accumulator `settle` already writes to
                        // `cost_usd`/the token columns (TS `runs.ts:1186-
                        // 1204`, "Delegated cost lands here").
                        usage = add_usage(usage, delegated_usage);
                        let clipped: String = result.chars().take(4000).collect();
                        self.emit(
                            run_id,
                            RunEvent::ToolResult {
                                name: call.name.clone(),
                                result: clipped,
                            },
                        );
                        messages.push(ModelMessage {
                            role: "tool".to_string(),
                            content: MessageContent::Text(result),
                            tool_calls: None,
                            tool_call_id: Some(call.id.clone()),
                        });

                        // S2-04: `escalate` climbed a rung - port of
                        // `climb` (`escalation.ts:280-333`) applied to THIS
                        // run rather than TS's cross-run hand-off (see
                        // `tools/escalate.rs`'s doc). Every later step in
                        // this turn - including the rest of THIS batch, if
                        // the model asked for more than one tool - now
                        // calls on the climbed model.
                        if call.name == "escalate"
                            && let Some(step) = toolbox.take_escalated()
                        {
                            let persisted = {
                                let db = self.db();
                                db.conn().execute(
                                    "UPDATE runs SET model = ?1, updated_at = ?2 WHERE id = ?3",
                                    rusqlite::params![&step.model, now_iso(), run_id],
                                )
                            };
                            let persistence_failure = match persisted {
                                Ok(1) => None,
                                Ok(rows) => Some(format!("updated {rows} run rows")),
                                Err(err) => Some(err.to_string()),
                            };
                            if let Some(reason) = persistence_failure {
                                tracing::error!(
                                    "run {run_id}: failed to persist escalated requested model: {reason}"
                                );
                                return Outcome::Failed {
                                    state: RunState {
                                        messages,
                                        text,
                                        effective_requested_model,
                                        responding_model,
                                        usage,
                                        steps,
                                    },
                                    failure:
                                        "Could not persist the selected model for the next step."
                                            .to_string(),
                                    status: None,
                                };
                            }
                            effective_requested_model = step.model.clone();
                            self.emit(
                                run_id,
                                RunEvent::Notice {
                                    message: format!(
                                        "Escalating: {}. Continuing on {}.",
                                        step.note, step.model
                                    ),
                                },
                            );
                        }
                    }
                }
            }
        }

        Outcome::Failed {
            state: RunState {
                messages,
                text,
                effective_requested_model,
                responding_model,
                usage,
                steps,
            },
            failure: format!("Stopped after {MAX_STEPS} tool steps without an answer."),
            status: None,
        }
    }

    /// Writes the run row, appends the assistant's message to the
    /// conversation, and tells subscribers. Port of the settle path in the
    /// TS `drive`, minus the unverified-claim guards, second opinion,
    /// notify/badge and routine-health bookkeeping - none of that exists in
    /// S1's scope.
    fn settle(
        self: &Arc<Self>,
        run_id: &str,
        bot_id: &str,
        conversation_id: &str,
        outcome: Outcome,
    ) {
        let (status, failure, state, upstream_status) = match outcome {
            Outcome::Answered(state) => ("done", None, state, None),
            Outcome::Failed {
                state,
                failure,
                status,
            } => ("failed", Some(failure), state, status),
            Outcome::Paused {
                state,
                pending,
                deferred,
                judge_verdict,
                judge_reason,
            } => {
                self.park(
                    run_id,
                    bot_id,
                    state,
                    pending,
                    deferred,
                    judge_verdict,
                    judge_reason,
                );
                return;
            }
        };
        let usage = state.usage.clone().unwrap_or(ModelUsage {
            cost_usd: 0.0,
            input_tokens: 0,
            output_tokens: 0,
            cached_tokens: 0,
        });

        let messages_json = serde_json::to_string(&state.messages).unwrap_or_else(|err| {
            tracing::error!("run {run_id}: failed to serialize run messages: {err}");
            "[]".to_string()
        });

        let updated = {
            let db = self.db();
            db.conn().execute(
                "UPDATE runs SET status = ?1, messages = ?2, text = ?3, model = ?4, steps = ?5,
                        cost_usd = ?6, input_tokens = ?7, output_tokens = ?8, cached_tokens = ?9,
                        error = ?10, updated_at = ?11
                   WHERE id = ?12",
                rusqlite::params![
                    status,
                    messages_json,
                    state.text,
                    state.effective_requested_model,
                    state.steps,
                    usage.cost_usd,
                    usage.input_tokens,
                    usage.output_tokens,
                    usage.cached_tokens,
                    failure,
                    now_iso(),
                    run_id,
                ],
            )
        };
        // The rail's busy flag and unread count both come off this row.
        self.changes.touch(ChangeKind::Roster);
        // And it is the end of this face at the bottom of the thread.
        self.activity
            .lock()
            .expect("activity mutex poisoned")
            .remove(run_id);
        self.changes.touch(ChangeKind::Working);

        // F11: read back BEFORE either early-return branch below, so a
        // run-row UPDATE failure (the very next check) still gets recorded
        // as a routine failure instead of health tracking silently skipping
        // it. Read once here rather than threaded through this function's
        // parameters - see `start_routine`'s doc on why the column is
        // stamped at INSERT time instead.
        let routine_id: Option<String> = {
            let db = self.db();
            db.conn()
                .query_row(
                    "SELECT routine_id FROM runs WHERE id = ?1",
                    rusqlite::params![run_id],
                    |row| row.get::<_, Option<String>>(0),
                )
                .optional()
                .unwrap_or_default()
                .flatten()
        };

        // B12: a run task has no HTTP response to fail - a store error past
        // this point is reported to subscribers as the run's own error
        // instead of panicking the task (and, via B2, poisoning the db
        // mutex for every other request in flight).
        if let Err(err) = updated {
            tracing::error!("run {run_id}: failed to update run row: {err}");
            self.emit(
                run_id,
                RunEvent::Error {
                    message: "internal error saving run result".to_string(),
                    status: None,
                },
            );
            // F11: previously this early return skipped health recording
            // entirely, so a routine whose own row UPDATE kept failing (a
            // locked db, a disk error) never counted toward FAILURE_LIMIT
            // and never paused - the exact failure `routine-health.ts`
            // exists to catch, just reached through a different door.
            if let Some(routine_id) = &routine_id {
                let db = self.db();
                if let Err(err) = store::routines::record_routine_run(
                    &db,
                    routine_id,
                    false,
                    Some("internal error saving run result"),
                ) {
                    tracing::error!("run {run_id}: failed to record routine health: {err}");
                }
            }
            if let Some(hook) = self
                .on_run_done
                .lock()
                .expect("on_run_done mutex poisoned")
                .as_ref()
            {
                hook(run_id, bot_id, conversation_id);
            }
            self.finish(run_id);
            return;
        }

        // F6: a run that failed with no text at all - stopped before its
        // first model call, stopped between steps before one produced any,
        // or an instant upstream error on the very first call - writes NO
        // assistant message, mirroring TS's `fail()` (`runs.ts:965-968`,
        // `:1741-1752`), which updates only the run row and never touches
        // the conversation. Without this, `POST .../stop` right after
        // starting a run left an empty assistant bubble sitting in the
        // thread forever - and so did a room member whose leg failed
        // before writing a word (`rooms.rs`'s
        // `an_instant_error_owner_run_still_chains_to_member_two`).
        let saved_id: Option<String> = if status == "failed" && state.text.is_empty() {
            None
        } else {
            let db = self.db();
            let owner = store::get_conversation(&db, conversation_id)
                .unwrap_or_else(|err| {
                    tracing::error!("run {run_id}: failed to read conversation: {err}");
                    None
                })
                .map(|c| c.bot_id);
            let extra = store::NewMessage {
                model: Some(state.responding_model.clone()),
                error: failure.clone(),
                attachment_id: None,
                // Stamped only when this bot is a GUEST in the thread - left
                // null in the ordinary case, which keeps a normal chat free
                // of names above every bubble.
                bot_id: if owner.as_deref() == Some(bot_id) {
                    None
                } else {
                    Some(bot_id.to_string())
                },
                usage: Some(store::Usage {
                    cost_usd: usage.cost_usd,
                    input_tokens: usage.input_tokens as i64,
                    output_tokens: usage.output_tokens as i64,
                    cached_tokens: usage.cached_tokens as i64,
                }),
            };
            match store::append_message(&db, conversation_id, "assistant", &state.text, extra) {
                Ok(message) => Some(message.id),
                Err(err) => {
                    tracing::error!("run {run_id}: failed to append settle message: {err}");
                    None
                }
            }
        };

        // S5-03: counted on EVERY terminal routine run, success included -
        // deliberately not folded into the `!silent`/notify branch TS keeps
        // this next to (`runs.ts:1560-1576`): the reset on success is half
        // the rule, and skipping it here would leave a stale failure streak
        // standing until three unrelated failures paused an otherwise
        // healthy routine. `routine_id` was read back once, above, before
        // either early-return branch - not re-read here (F11: it used to
        // be, twice, on every settle in the app, routine or not).
        //
        // F11: `status == "done"` is an explicit ALLOW-list, not the
        // `status != "failed"` deny-list this replaces. The match at the
        // top of this function only ever produces "done" or "failed" here
        // (`Paused` returns earlier, before this point), so the two read the
        // same today - but a deny-list quietly counts ANY future status
        // this function does not yet know about (a "stopped"/"cancelled"
        // terminal state) as a success and resets the streak, where an
        // allow-list forces a new status to be reckoned with explicitly.
        if let Some(routine_id) = routine_id {
            let db = self.db();
            let ok = status == "done";
            if let Err(err) =
                store::routines::record_routine_run(&db, &routine_id, ok, failure.as_deref())
            {
                tracing::error!("run {run_id}: failed to record routine health: {err}");
            }
        }

        if status == "failed" {
            self.emit(
                run_id,
                RunEvent::Error {
                    message: failure.unwrap_or_default(),
                    status: upstream_status,
                },
            );
        } else if let Some(saved_id) = saved_id {
            self.emit(
                run_id,
                RunEvent::Done {
                    model: state.responding_model.clone(),
                    message_id: saved_id,
                },
            );
        } else {
            self.emit(
                run_id,
                RunEvent::Error {
                    message: "internal error saving run result".to_string(),
                    status: None,
                },
            );
        }

        // Fired for both branches above, answered and failed alike - a room
        // round chains on whatever just happened, not only on success.
        if let Some(hook) = self
            .on_run_done
            .lock()
            .expect("on_run_done mutex poisoned")
            .as_ref()
        {
            hook(run_id, bot_id, conversation_id);
        }
        self.finish(run_id);
    }

    /// S2-03: a tool call needing Josh's decision stops the run and queues
    /// it, instead of running it. Port of the TS `park` (`runs.ts:1645-
    /// 1739`). Deliberately NOT terminal: `bus`/`backlog`/`stopping` are
    /// left alone (a subscriber watching this run's stream is still
    /// watching it - `decide_approval` picks the SAME run id back up), and
    /// `on_run_done` never fires (a room round chains on an answer or a
    /// failure, never on a run that is still, in effect, in progress).
    #[allow(clippy::too_many_arguments)]
    fn park(
        self: &Arc<Self>,
        run_id: &str,
        bot_id: &str,
        state: RunState,
        pending: ToolCall,
        deferred: Vec<ToolCall>,
        judge_verdict: Option<String>,
        judge_reason: Option<String>,
    ) {
        // 🔴 Anything the model asked for AFTER the gated call is dropped
        // rather than silently run later without a decision of its own. A
        // run holds ONE pending approval; the pending call itself gets no
        // synthetic result here at all - it is genuinely undecided, and
        // gets a real one from `decide_approval` once Josh answers.
        let mut messages = state.messages;
        for call in &deferred {
            messages.push(ModelMessage {
                role: "tool".to_string(),
                content: MessageContent::Text(
                    "Not run: the turn stopped for Josh's approval before reaching this call. \
Nothing is wrong with it - ask for it again on your next turn, once he has decided the one he \
is looking at."
                        .to_string(),
                ),
                tool_calls: None,
                tool_call_id: Some(call.id.clone()),
            });
        }

        let usage = state.usage.unwrap_or(ModelUsage {
            cost_usd: 0.0,
            input_tokens: 0,
            output_tokens: 0,
            cached_tokens: 0,
        });
        let messages_json = serde_json::to_string(&messages).unwrap_or_else(|err| {
            tracing::error!("run {run_id}: failed to serialize paused run messages: {err}");
            "[]".to_string()
        });

        let updated = {
            let db = self.db();
            db.conn().execute(
                "UPDATE runs SET status = 'waiting', messages = ?1, text = ?2, model = ?3, steps = ?4,
                        cost_usd = ?5, input_tokens = ?6, output_tokens = ?7, cached_tokens = ?8,
                        updated_at = ?9
                   WHERE id = ?10",
                rusqlite::params![
                    messages_json,
                    state.text,
                    state.effective_requested_model,
                    state.steps,
                    usage.cost_usd,
                    usage.input_tokens,
                    usage.output_tokens,
                    usage.cached_tokens,
                    now_iso(),
                    run_id,
                ],
            )
        };
        // B12: same posture as `settle`'s own save failure - nothing here
        // has an HTTP response to fail, so a store error is reported to
        // subscribers as the run's own error rather than panicking the task.
        if let Err(err) = updated {
            tracing::error!("run {run_id}: failed to update run row while parking: {err}");
            self.emit(
                run_id,
                RunEvent::Error {
                    message: "internal error saving run result".to_string(),
                    status: None,
                },
            );
            self.finish(run_id);
            return;
        }

        let approval_id = {
            let db = self.db();
            match approvals::insert_pending(
                &db,
                run_id,
                bot_id,
                &pending.name,
                &pending.arguments,
                &pending.id,
                judge_verdict.as_deref(),
                judge_reason.as_deref(),
            ) {
                Ok(id) => id,
                Err(err) => {
                    tracing::error!("run {run_id}: failed to insert approval row: {err}");
                    self.emit(
                        run_id,
                        RunEvent::Error {
                            message: "internal error saving run result".to_string(),
                            status: None,
                        },
                    );
                    self.finish(run_id);
                    return;
                }
            }
        };

        self.changes.touch(ChangeKind::Approvals);
        // The rail's busy flag comes off the run row same as any other
        // status change.
        self.changes.touch(ChangeKind::Roster);
        // The line changes from whatever it was to "Waiting for you to
        // approve X", which is the one state that sits there until Josh
        // acts - so it is the single most worth-saying thing this
        // indicator ever says.
        self.changes.touch(ChangeKind::Working);

        self.emit(
            run_id,
            RunEvent::ApprovalNeeded {
                approval_id,
                name: pending.name,
                args: pending.arguments,
            },
        );
    }

    /// S2-03/S13b-F: Josh's decision on a waiting run's one pending tool
    /// call. Port of the TS `decideApproval` (`runs.ts:699-772`), now
    /// including its `fulfilment` parameter - a result Josh (or, once
    /// S13b-03 fills in `CLIENT_FULFILLED`, the desktop app) supplies
    /// instead of the server computing one. Ported verbatim in shape as
    /// TS's own gate (`runs.ts:756`): honored only when `approved` AND the
    /// call names a tool in `CLIENT_FULFILLED` or `is_answerable`, clipped
    /// to `MAX_FULFILMENT_CHARS` - for every other tool the field is
    /// silently discarded and `toolbox.run` still computes the result, same
    /// as `app.ts:4072-4074`. Still minus the empty-abnormal-answer retry
    /// `drive` itself does on a fresh start, which a resume does not
    /// repeat.
    ///
    /// Returns `false` for an approval id that names no PENDING row -
    /// already decided, or never existed - which `routes/approvals.rs`
    /// turns into a 404, same as the TS route answering nothing for a stale
    /// id. Only the ONE pending call is awaited here; the run's further
    /// steps, if any, continue on a spawned task exactly like `start`'s own
    /// first step - so approving a `message_bot` call that itself takes a
    /// few seconds does not hold the HTTP response open for it.
    pub async fn decide_approval(
        self: &Arc<Self>,
        approval_id: &str,
        approved: bool,
        fulfilment: Option<String>,
    ) -> bool {
        let pending = {
            let db = self.db();
            match approvals::take_pending(&db, approval_id, approved) {
                Ok(row) => row,
                Err(err) => {
                    tracing::error!("approval {approval_id}: failed to decide row: {err}");
                    None
                }
            }
        };
        let Some(pending) = pending else {
            return false;
        };
        // Silently: this is Josh answering something he just asked himself
        // to decide, not news that needs a second alert.
        self.changes.touch(ChangeKind::Approvals);

        #[allow(clippy::type_complexity)]
        type RunRow = (
            String,
            String,
            String,
            String,
            String,
            i64,
            f64,
            i64,
            i64,
            i64,
            String,
            Option<String>,
        );
        let run: Option<RunRow> = {
            let db = self.db();
            db.conn()
                .query_row(
                    "SELECT status, conversation_id, model, messages, text, steps,
                            cost_usd, input_tokens, output_tokens, cached_tokens, trigger, tools
                       FROM runs WHERE id = ?1",
                    rusqlite::params![pending.run_id],
                    |row| {
                        Ok((
                            row.get(0)?,
                            row.get(1)?,
                            row.get(2)?,
                            row.get(3)?,
                            row.get(4)?,
                            row.get(5)?,
                            row.get(6)?,
                            row.get(7)?,
                            row.get(8)?,
                            row.get(9)?,
                            row.get(10)?,
                            row.get(11)?,
                        ))
                    },
                )
                .optional()
                .unwrap_or_else(|err| {
                    tracing::error!("run {}: failed to load run row: {err}", pending.run_id);
                    None
                })
        };
        let Some((
            status,
            conversation_id,
            model,
            messages_json,
            text,
            steps,
            cost_usd,
            input_tokens,
            output_tokens,
            cached_tokens,
            trigger_str,
            tools_json,
        )) = run
        else {
            return true;
        };
        // F3: a resumed run keeps whatever narrowing it started with -
        // read back from the SAME `tools` column `start_inner` wrote,
        // rather than re-deriving it (there is nothing on `pending` to
        // re-derive it FROM: an approval row has no `routine_id`).
        let only: Option<Vec<String>> = tools_json
            .as_deref()
            .and_then(|text| serde_json::from_str(text).ok());
        // Approved or not, the decision is recorded above - a run that is
        // no longer `waiting` (raced, or its own row failed to save when it
        // parked) has nothing left here to resume.
        if status != "waiting" {
            return true;
        }

        let claimed = {
            let db = self.db();
            db.conn().execute(
                "UPDATE runs SET status = 'running', updated_at = ?1 WHERE id = ?2 AND status = 'waiting'",
                rusqlite::params![now_iso(), pending.run_id],
            )
        };
        if !matches!(claimed, Ok(1)) {
            if let Err(err) = &claimed {
                tracing::error!(
                    "run {}: failed to claim waiting run before approval execution: {err}",
                    pending.run_id
                );
            }
            self.fail_waiting_run(
                &pending.run_id,
                &pending.bot_id,
                Some(&conversation_id),
                "Could not safely resume the approved tool call.",
            );
            return true;
        }

        let mut messages: Vec<ModelMessage> =
            serde_json::from_str(&messages_json).unwrap_or_else(|err| {
                tracing::error!(
                    "run {}: failed to parse stored messages: {err}",
                    pending.run_id
                );
                Vec::new()
            });
        let trigger = parse_trigger(&trigger_str);
        let call = ToolCall {
            id: pending.call_id.clone(),
            name: pending.tool_name.clone(),
            arguments: pending.tool_args.clone(),
        };

        let manager = Arc::clone(self);
        tokio::spawn(async move {
            // F2/room: a resumed call has no live ROOM bit on the run row - S2-
            // 03's scope names this: a room round replaying an approval on
            // resume is out of scope, and every case this ticket's tests
            // exercise is an ordinary chat turn, which `room: false` floors
            // exactly the same as `start` would have.
            let toolbox = manager.toolbox_for_context(
                &pending.bot_id,
                trigger,
                false,
                &model,
                only.clone(),
                tools::RunExecutionContext::ModelTurn {
                    run_id: pending.run_id.clone(),
                },
            );

            let mut pending_observation = None;
            let resolved_usage = if approved {
                manager.note(&pending.run_id, Some(call.name.clone()), false);
                manager.emit(
                    &pending.run_id,
                    RunEvent::ToolCall {
                        name: call.name.clone(),
                        args: call.arguments.clone(),
                    },
                );
                // S13b-F: a posted `result` is only ever honored for the two
                // lists TS keeps deliberately separate (see `is_client_fulfilled`'s
                // own doc) - anything else's `result` is discarded here and
                // `toolbox.run` still computes the answer, same as
                // `app.ts:4072-4074`.
                let gated_fulfilment = fulfilment.filter(|_| {
                    is_client_fulfilled(&call.name) || shared::ask_josh::is_answerable(&call.name)
                });
                let outcome = if let Some(fulfilment) = gated_fulfilment {
                    let clipped_fulfilment: String =
                        fulfilment.chars().take(MAX_FULFILMENT_CHARS).collect();
                    // §4.4: fence ONLY the client-fulfilled arm, after the cap.
                    // A client-fulfilled result is untrusted tool output - bytes
                    // off Josh's own disk that a bot chose the path for - and
                    // `tools::fence_tool_output`'s own doc records that an
                    // earlier version was escapable by echoing its close
                    // marker, which a client-supplied string has exactly the
                    // power to do. `is_answerable`'s arm is Josh's own typed
                    // answer to a question, not tool output, and must reach the
                    // model byte-exact - fencing it here would both misrepresent
                    // it as untrusted and break the stored-verbatim contract
                    // `tests/approvals.rs`'s `"use the blue one"` assertion
                    // depends on.
                    let fenced = if is_client_fulfilled(&call.name) {
                        tools::fence_tool_output(&clipped_fulfilment)
                    } else {
                        clipped_fulfilment
                    };
                    tools::ToolOutcome::new(fenced, None)
                } else {
                    let offered = manager.offered_specs_for_model(&toolbox, &model).await;
                    if offered.iter().any(|spec| spec.name == call.name) {
                        toolbox.run(&call.name, &call.arguments).await
                    } else {
                        tools::ToolOutcome::new(
                            format!(
                                "Not allowed: {} is no longer offered for the selected model and run context.",
                                call.name
                            ),
                            None,
                        )
                    }
                };
                let tools::ToolOutcome {
                    text: result,
                    usage: delegated_usage,
                    observation,
                } = outcome;
                if let Some(observation) = observation {
                    if call.name == "snap_desk" {
                        pending_observation = Some(observation);
                    } else {
                        let observation_run = observation.metadata().run_id.clone();
                        manager.observations.release(&observation_run);
                        tracing::error!(
                            run_id = %pending.run_id,
                            tool = %call.name,
                            "unexpected screen observation from approved non-capture tool"
                        );
                    }
                }
                let clipped: String = result.chars().take(4000).collect();
                manager.emit(
                    &pending.run_id,
                    RunEvent::ToolResult {
                        name: call.name.clone(),
                        result: clipped,
                    },
                );
                messages.push(ModelMessage {
                    role: "tool".to_string(),
                    content: MessageContent::Text(result),
                    tool_calls: None,
                    tool_call_id: Some(call.id.clone()),
                });
                delegated_usage
            } else {
                // Port of the TS `resolveTool`'s refusal text (`run.ts:404-
                // 407`) verbatim - the model is told WHY nothing ran, not
                // handed an empty result indistinguishable from a tool that
                // genuinely found nothing.
                let refusal = format!(
                    "Refused: Josh did not approve {}. Do not try it again this turn; say what you would have done.",
                    call.name
                );
                manager.emit(
                    &pending.run_id,
                    RunEvent::ToolResult {
                        name: call.name.clone(),
                        result: refusal.clone(),
                    },
                );
                messages.push(ModelMessage {
                    role: "tool".to_string(),
                    content: MessageContent::Text(refusal),
                    tool_calls: None,
                    tool_call_id: Some(call.id.clone()),
                });
                None
            };

            manager.changes.touch(ChangeKind::Roster);
            manager.changes.touch(ChangeKind::Working);

            let starting_usage = add_usage(
                Some(ModelUsage {
                    cost_usd,
                    input_tokens: input_tokens as u32,
                    output_tokens: output_tokens as u32,
                    cached_tokens: cached_tokens as u32,
                }),
                resolved_usage,
            );

            let run_id = pending.run_id.clone();
            let bot_id = pending.bot_id.clone();
            let outcome = manager
                .run_turn(
                    &run_id,
                    &bot_id,
                    trigger,
                    model,
                    messages,
                    &toolbox,
                    steps,
                    text,
                    starting_usage,
                    pending_observation,
                )
                .await;
            manager.settle(&run_id, &bot_id, &conversation_id, outcome);
        });

        true
    }

    /// F8: expires any `pending` approval older than `APPROVAL_TTL_HOURS`
    /// and prunes any decided/expired row older than
    /// `APPROVAL_RETENTION_DAYS`. Takes `now` rather than reading the clock
    /// itself so a test can sweep a 25-hour-old row without sleeping 25
    /// hours - same precedent `with_backlog_ttl` sets for `finish`'s own
    /// delayed cleanup.
    ///
    /// Called from `finish`'s spawned cleanup task (so an approval a run's
    /// own settlement never revisits still gets swept on roughly the same
    /// cadence live traffic already produces) and from
    /// `routes/approvals.rs` right before every `GET /api/approvals` reads
    /// `list_pending` - the two places F8 named as already touching
    /// `approvals` on a schedule or on every poll, rather than standing up
    /// a third, dedicated timer loop for one narrow cleanup.
    pub fn sweep_approvals(self: &Arc<Self>, now: chrono::DateTime<chrono::Utc>) {
        let expire_cutoff = (now - chrono::Duration::hours(APPROVAL_TTL_HOURS))
            .to_rfc3339_opts(chrono::SecondsFormat::Millis, true);
        let prune_cutoff = (now - chrono::Duration::days(APPROVAL_RETENTION_DAYS))
            .to_rfc3339_opts(chrono::SecondsFormat::Millis, true);

        let expired = {
            let db = self.db();
            approvals::list_pending_older_than(&db, &expire_cutoff).unwrap_or_else(|err| {
                tracing::error!("approvals sweep: failed to list expired approvals: {err}");
                Vec::new()
            })
        };
        for row in expired {
            let marked = {
                let db = self.db();
                approvals::mark_expired(&db, &row.id)
            };
            match marked {
                Ok(true) => self.fail_waiting_run(
                    &row.run_id,
                    &row.bot_id,
                    row.conversation_id.as_deref(),
                    "approval expired",
                ),
                // Raced with `decide_approval` between the list above and
                // this write - Josh answered it, so there is nothing left
                // for the sweep to fail.
                Ok(false) => {}
                Err(err) => tracing::error!(
                    "approvals sweep: failed to expire approval {}: {err}",
                    row.id
                ),
            }
        }

        if let Err(err) = {
            let db = self.db();
            approvals::prune_decided_older_than(&db, &prune_cutoff)
        } {
            tracing::error!("approvals sweep: failed to prune old approvals: {err}");
        }
    }

    /// Fails a `waiting` run directly, for `sweep_approvals` - the sweep has
    /// no `RunState` in hand (only a stale row it is about to expire), so
    /// this writes just the run's status/error and reproduces the
    /// subscriber-visible half of `settle`'s failed branch: the same
    /// `RunEvent::Error`, the same `on_run_done` hook, the same `finish`
    /// teardown of `bus`/`stopping`/`interjections` and delayed backlog
    /// drop. `AND status = 'waiting'` guards the same race `decide_approval`
    /// guards elsewhere: losing it means Josh (or a resumed turn) already
    /// moved the run on, and there is nothing here left to fail.
    fn fail_waiting_run(
        self: &Arc<Self>,
        run_id: &str,
        bot_id: &str,
        conversation_id: Option<&str>,
        reason: &str,
    ) {
        let changed = {
            let db = self.db();
            db.conn().execute(
                "UPDATE runs SET status = 'failed', error = ?1, updated_at = ?2 \
                 WHERE id = ?3 AND status = 'waiting'",
                rusqlite::params![reason, now_iso(), run_id],
            )
        };
        let changed = match changed {
            Ok(n) => n,
            Err(err) => {
                tracing::error!("run {run_id}: approvals sweep failed to mark it failed: {err}");
                return;
            }
        };
        if changed == 0 {
            return;
        }

        self.changes.touch(ChangeKind::Roster);
        self.activity
            .lock()
            .expect("activity mutex poisoned")
            .remove(run_id);
        self.changes.touch(ChangeKind::Working);
        self.changes.touch(ChangeKind::Approvals);

        self.emit(
            run_id,
            RunEvent::Error {
                message: reason.to_string(),
                status: None,
            },
        );

        if let Some(hook) = self
            .on_run_done
            .lock()
            .expect("on_run_done mutex poisoned")
            .as_ref()
        {
            hook(run_id, bot_id, conversation_id.unwrap_or_default());
        }
        self.finish(run_id);
    }

    /// S1-F-04 (B3, B5, B13): once a run has emitted its terminal event,
    /// nothing will ever `emit` into it again, so `bus`'s senders and
    /// `stopping`'s entry can go immediately - the latter closes B13's race
    /// even when a stop lands after this run's one and only `take_stop`
    /// check already passed (the run finished in "the same instant"; see
    /// the finding). `backlog` stays around for `backlog_ttl` so a
    /// subscriber that calls `subscribe` a moment after settle still gets
    /// the replay; the spawned task below drops it once that grace period
    /// passes, together with any `bus` entry a late `subscribe` recreated
    /// in the meantime - otherwise that recreated entry would itself be
    /// exactly B5's "grows by one entry per run for the life of the
    /// process", just delayed rather than fixed.
    fn finish(self: &Arc<Self>, run_id: &str) {
        self.observations.release(run_id);
        self.stopping
            .lock()
            .expect("stopping mutex poisoned")
            .remove(run_id);
        self.bus.lock().expect("bus mutex poisoned").remove(run_id);
        // S2-04: the turn drained anything queued as it went; whatever is
        // left was queued too late to be delivered, and this run has
        // nothing left to deliver it to - port of TS's own
        // `this.interjections.delete(runId)` at the same point
        // (`runs.ts:1182`).
        self.interjections
            .lock()
            .expect("interjections mutex poisoned")
            .remove(run_id);

        let manager = Arc::clone(self);
        let run_id = run_id.to_string();
        let ttl = self.backlog_ttl;
        tokio::spawn(async move {
            tokio::time::sleep(ttl).await;
            manager
                .backlog
                .lock()
                .expect("backlog mutex poisoned")
                .remove(&run_id);
            manager
                .bus
                .lock()
                .expect("bus mutex poisoned")
                .remove(&run_id);
            // F8: piggybacks the approvals sweep on the same delayed task
            // that already fires once per finished run, rather than a
            // dedicated timer loop - see `sweep_approvals`'s own doc.
            manager.sweep_approvals(chrono::Utc::now());
        });
    }

    fn take_stop(&self, run_id: &str) -> bool {
        self.stopping
            .lock()
            .expect("stopping mutex poisoned")
            .remove(run_id)
    }

    fn stop_requested(&self, run_id: &str) -> bool {
        self.stopping
            .lock()
            .expect("stopping mutex poisoned")
            .contains(run_id)
    }

    async fn wait_for_stop(&self, run_id: &str) {
        loop {
            if self.stop_requested(run_id) {
                return;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    }

    /// Records what a run is doing, and tells the working-indicator's
    /// subscribers only when the LINE would actually change - a tool call
    /// and the fifty deltas after it are one state, not fifty.
    fn note(&self, run_id: &str, tool: Option<String>, wrote_text: bool) {
        let mut activity = self.activity.lock().expect("activity mutex poisoned");
        let now = ActivityState { tool, wrote_text };
        let changed = activity.get(run_id).map(|was| *was != now).unwrap_or(true);
        activity.insert(run_id.to_string(), now);
        drop(activity);
        if changed {
            self.changes.touch(ChangeKind::Working);
        }
    }

    fn emit(&self, run_id: &str, event: RunEvent) {
        self.backlog
            .lock()
            .expect("backlog mutex poisoned")
            .entry(run_id.to_string())
            .or_default()
            .push(event.clone());
        let mut bus = self.bus.lock().expect("bus mutex poisoned");
        if let Some(senders) = bus.get_mut(run_id) {
            senders.retain(|tx| tx.send(event.clone()).is_ok());
        }
    }
}

fn add_usage(current: Option<ModelUsage>, incoming: Option<ModelUsage>) -> Option<ModelUsage> {
    match (current, incoming) {
        (None, u) => u,
        (c, None) => c,
        (Some(c), Some(u)) => Some(ModelUsage {
            cost_usd: c.cost_usd + u.cost_usd,
            input_tokens: c.input_tokens + u.input_tokens,
            output_tokens: c.output_tokens + u.output_tokens,
            cached_tokens: c.cached_tokens + u.cached_tokens,
        }),
    }
}

fn trigger_str(trigger: Trigger) -> &'static str {
    match trigger {
        Trigger::Chat => "chat",
        Trigger::Routine => "routine",
        Trigger::Webhook => "webhook",
        Trigger::Goal => "goal",
    }
}

/// The inverse of `trigger_str` - what `decide_approval` reads a resumed
/// run's stored `trigger` column back into. Falls back to `Chat` on
/// anything unrecognised (a hand-seeded test row, say) rather than failing
/// the resume outright: `Chat` is the least-tightened reading, and TS has
/// no such column to lose in the first place.
fn parse_trigger(s: &str) -> Trigger {
    match s {
        "routine" => Trigger::Routine,
        "webhook" => Trigger::Webhook,
        "goal" => Trigger::Goal,
        _ => Trigger::Chat,
    }
}

/// Same format as JS `new Date().toISOString()` (millisecond precision, `Z`
/// suffix) - matches `store::conversations::now_iso`, which is
/// `pub(crate)` to that crate and so not reusable here.
fn now_iso() -> String {
    chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Millis, true)
}
