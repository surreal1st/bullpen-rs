//! The tools a run's model may call. S1's first six: `say`, `ask_josh`,
//! `remember`, `message_bot`, and the two Grok gaps, `create_room` and
//! `add_to_room` - all `allow` by default. S2-03 adds `shell`, `ask` by
//! default (S2-02's `default_decisions`), the one tool this toolbox holds
//! that actually needs the approval plumbing `crate::runs`'s tool loop now
//! gates every call through. Port of the relevant slices of
//! `src/server/bot-tools.ts`, `conversing.ts`, `delegate.ts` and `app.ts`'s
//! `message_bot`-into-a-room branch.

mod add_to_room;
mod ask_josh;
mod create_room;
pub(crate) mod escalate;
mod message_bot;
mod remember;
mod say;
mod shell;

use std::future::Future;
use std::pin::Pin;
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};

use model::ladder::Trigger;
use model::{ModelPort, ModelUsage, ToolSpec};
use store::Db;

/// B2: the one guard every tool uses to lock the db. A poisoned `Mutex`
/// (left behind by a panic under the lock elsewhere) used to mean every
/// later `.expect("db mutex poisoned")` panicked too, turning one bad
/// request into a dead server that only a restart fixed -
/// `unwrap_or_else(PoisonError::into_inner)` recovers the guard instead.
pub(crate) fn lock_db(db: &Arc<Mutex<Db>>) -> MutexGuard<'_, Db> {
    db.lock().unwrap_or_else(PoisonError::into_inner)
}

/// The hook `message_bot` calls when it posts into a ROOM: `(conversation_id,
/// mandatory) -> bool`. `None` until S1-06 wires room rounds in - a
/// `message_bot` call into a room still posts the message either way, it
/// just cannot wake the round yet.
pub type RoomHook = Arc<Mutex<Option<Box<dyn Fn(&str, bool) -> bool + Send + Sync>>>>;

/// A tool's text result, plus (F3) whatever a delegated model call inside it
/// spent - `None` for every tool but `message_bot`'s bot branch. `runs.rs`'s
/// tool loop folds this into the run's own usage total the same way it folds
/// its own steps, so a colleague `message_bot` asked is money this run's
/// `cost_usd` actually accounts for.
type ToolFuture = Pin<Box<dyn Future<Output = (String, Option<ModelUsage>)> + Send>>;

/// What a run offers the model: the specs it sees, one executor keyed by
/// name. Port of the TS `ToolBox`.
pub struct ToolBox {
    pub specs: Vec<ToolSpec>,
    handler: Arc<dyn Fn(String, String) -> ToolFuture + Send + Sync>,
    /// S2-04: set by the `escalate` tool when it climbs a rung; taken
    /// (cleared) by `runs.rs`'s tool loop right after the call that set it,
    /// which applies `model` to the run's next request and folds `note`
    /// into a notice. See `escalate`'s own doc for why this run keeps going
    /// on the climbed model instead of TS's cross-run hand-off.
    escalated: Arc<Mutex<Option<escalate::Climb>>>,
}

impl ToolBox {
    /// Runs one tool call. An unknown name is never a panic - a model can
    /// hallucinate a tool name same as anything else, and the run should
    /// hear about that as an ordinary tool result, not crash over it.
    pub async fn run(&self, name: &str, args: &str) -> (String, Option<ModelUsage>) {
        (self.handler)(name.to_string(), args.to_string()).await
    }

    /// S2-04: takes (clears) whatever `escalate` decided during the last
    /// `run` call, if it climbed a rung.
    pub(crate) fn take_escalated(&self) -> Option<escalate::Climb> {
        self.escalated
            .lock()
            .expect("escalated mutex poisoned")
            .take()
    }
}

/// Parameters for building a toolbox. Reduces parameter count to avoid
/// clippy's `too_many_arguments` lint.
pub struct BuildParams {
    pub db: Arc<Mutex<Db>>,
    pub port: Arc<dyn ModelPort>,
    pub bot_id: String,
    pub room_hook: RoomHook,
    pub trigger: Trigger,
    pub room: bool,
    pub initial_model: String,
    pub changes: crate::changes::ChangeBus,
}

/// Builds the S1 toolbox for one bot's run. F2: `trigger`/`room` are the
/// CALLER's, forwarded only to `message_bot`'s bot branch so a nested
/// delegated call is floored the same way the caller's own model choice is.
/// S2-04: `initial_model` seeds a shared cell only the `escalate` arm below
/// reads or writes - "what model is THIS run on right now", so an escalate
/// tier is computed from wherever a previous climb (this same turn) left
/// it, not from the run's starting model every time.
pub fn build(params: BuildParams) -> ToolBox {
    let db = params.db;
    let port = params.port;
    let bot_id = params.bot_id;
    let room_hook = params.room_hook;
    let trigger = params.trigger;
    let room = params.room;
    let initial_model = params.initial_model;
    let changes = params.changes;
    let specs = vec![
        say::spec(),
        ask_josh::spec(),
        remember::spec(),
        message_bot::spec(),
        create_room::spec(),
        add_to_room::spec(),
        shell::spec(),
        escalate::spec(),
    ];

    let current_model = Arc::new(Mutex::new(initial_model.to_string()));
    let escalated: Arc<Mutex<Option<escalate::Climb>>> = Arc::new(Mutex::new(None));

    let handler: Arc<dyn Fn(String, String) -> ToolFuture + Send + Sync> = {
        let current_model = Arc::clone(&current_model);
        let escalated = Arc::clone(&escalated);
        Arc::new(move |name, args| {
            let db = Arc::clone(&db);
            let port = Arc::clone(&port);
            let bot_id = bot_id.clone();
            let room_hook = Arc::clone(&room_hook);
            let current_model = Arc::clone(&current_model);
            let escalated = Arc::clone(&escalated);
            let changes = changes.clone();
            Box::pin(async move {
                match name.as_str() {
                    "say" => (say::run(&db, &bot_id, &args), None),
                    "ask_josh" => (ask_josh::run(&db, &bot_id, &args, changes), None),
                    "remember" => (remember::run(&db, &bot_id, &args), None),
                    "create_room" => (create_room::run(&db, &bot_id, &args), None),
                    "add_to_room" => (add_to_room::run(&db, &args), None),
                    "shell" => (shell::run(&args), None),
                    "escalate" => {
                        let model_now = current_model
                            .lock()
                            .expect("current model mutex poisoned")
                            .clone();
                        let (text, climb) = {
                            let db = lock_db(&db);
                            escalate::run(&db, trigger, &model_now, &args)
                        };
                        if let Some(step) = &climb {
                            *current_model.lock().expect("current model mutex poisoned") =
                                step.model.clone();
                        }
                        *escalated.lock().expect("escalated mutex poisoned") = climb;
                        (text, None)
                    }
                    "message_bot" => {
                        message_bot::run(&db, &port, &bot_id, &room_hook, trigger, room, &args)
                            .await
                    }
                    other => (format!("Unknown tool: {other}"), None),
                }
            })
        })
    };

    ToolBox {
        specs,
        handler,
        escalated,
    }
}
