//! Runs, as rows. Port of `src/server/runs.ts` and `src/server/run.ts`,
//! narrowed to what S1 needs: build the prompt (the caller's job, via
//! `crate::prompt`), stream the model, execute tool calls in a loop, persist
//! the row, emit events for subscribers, settle. NOT approvals, escalation,
//! routing, snapshots, interjections or jobs - those are S2+.
//!
//! A run outlives its HTTP request: `start` returns an id immediately and
//! drives the run on a spawned task, so a client that closes the tab (or a
//! routine with no client at all, once S2 adds those) never abandons it.

use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};

use futures::StreamExt;
use model::ladder::{Trigger, model_for_run};
use model::{
    FunctionCall, MessageContent, MessageToolCall, ModelEvent, ModelMessage, ModelPort,
    ModelRequest, ModelUsage, ToolCall,
};
use store::Db;
use uuid::Uuid;

use crate::changes::{ChangeBus, ChangeKind};
use crate::tools::{self, RoomHook, ToolBox};

/// How many tool steps a single run may take before it is stopped rather
/// than left to loop. TS's `MAX_STEPS` is 24; the ticket sets S1's at 12.
const MAX_STEPS: i64 = 12;

/// One event a run's subscribers see. Port of the TS `RunEvent`, minus
/// `approval_needed` - there are no approvals to need one in S1.
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
}

/// What starts a run. Port of the TS `StartOptions`, narrowed to S1: no
/// `notice`, `routineId`, `goalId` or tool-narrowing - those belong to
/// routines/goals, which are out of scope here.
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
    model: String,
    usage: Option<ModelUsage>,
    steps: i64,
}

enum Outcome {
    Answered(RunState),
    Failed { state: RunState, failure: String },
}

/// Fired once a run settles. Aliased so the `RunManager` field below does
/// not trip clippy's `type_complexity`.
type OnRunDone = Box<dyn Fn(&str, &str, &str) + Send + Sync>;

/// The run manager: builds and drives runs, and answers "who is working".
pub struct RunManager {
    db: Arc<Mutex<Db>>,
    port: Arc<dyn ModelPort>,
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
    /// Fired once a run settles, answered or failed alike. `None` until a
    /// caller wires one in - S1-06 sets this to chain a room round.
    on_run_done: Mutex<Option<OnRunDone>>,
    /// What `message_bot` calls when it posts into a room. `None` until
    /// S1-06 sets it.
    start_room_turn: RoomHook,
}

impl RunManager {
    pub fn new(db: Arc<Mutex<Db>>, port: Arc<dyn ModelPort>) -> Self {
        Self {
            db,
            port,
            changes: ChangeBus::new(),
            bus: Mutex::new(HashMap::new()),
            backlog: Mutex::new(HashMap::new()),
            activity: Mutex::new(HashMap::new()),
            stopping: Mutex::new(HashSet::new()),
            on_run_done: Mutex::new(None),
            start_room_turn: Arc::new(Mutex::new(None)),
        }
    }

    /// S1-06 wires this to chain a room round: fired for both an answered
    /// and a failed run, never for a run still going.
    pub fn set_on_run_done(&self, f: impl Fn(&str, &str, &str) + Send + Sync + 'static) {
        *self.on_run_done.lock().expect("on_run_done mutex poisoned") = Some(Box::new(f));
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

        let inserted = {
            let db = self.db();
            db.conn().execute(
                "INSERT INTO runs (id, bot_id, conversation_id, trigger, status, model, messages, text, created_at, updated_at)
                 VALUES (?1, ?2, ?3, ?4, 'running', ?5, ?6, '', ?7, ?8)",
                rusqlite::params![
                    id,
                    options.bot_id,
                    options.conversation_id,
                    trigger_str(options.trigger),
                    model,
                    messages_json,
                    now,
                    now,
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

        let manager = Arc::clone(self);
        let run_id = id.clone();
        let bot_id = options.bot_id.clone();
        let conversation_id = options.conversation_id.clone();
        let messages = options.messages;
        tokio::spawn(async move {
            manager
                .drive(run_id, bot_id, conversation_id, model, messages)
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
                    None,
                    state.tool.as_deref(),
                    state.wrote_text,
                ),
            });
        }
        Ok(out)
    }

    fn toolbox_for(self: &Arc<Self>, bot_id: &str) -> ToolBox {
        tools::build(
            Arc::clone(&self.db),
            Arc::clone(&self.port),
            bot_id.to_string(),
            Arc::clone(&self.start_room_turn),
        )
    }

    async fn drive(
        self: Arc<Self>,
        run_id: String,
        bot_id: String,
        conversation_id: String,
        model: String,
        messages: Vec<ModelMessage>,
    ) {
        let toolbox = self.toolbox_for(&bot_id);
        let outcome = self.run_turn(&run_id, model, messages, &toolbox).await;
        self.settle(&run_id, &bot_id, &conversation_id, outcome);
    }

    /// One turn: call the model, run any tools it asks for, call it again,
    /// until it answers or hits the step limit. Port of the TS `runTurn`,
    /// without the pause-for-approval branch (S1 has no approvals - every
    /// tool call just runs).
    async fn run_turn(
        &self,
        run_id: &str,
        model: String,
        mut messages: Vec<ModelMessage>,
        toolbox: &ToolBox,
    ) -> Outcome {
        let mut text = String::new();
        let mut resolved_model = model.clone();
        let mut usage: Option<ModelUsage> = None;
        let mut steps: i64 = 0;

        while steps < MAX_STEPS {
            if self.take_stop(run_id) {
                return Outcome::Failed {
                    state: RunState {
                        messages,
                        text,
                        model: resolved_model,
                        usage,
                        steps,
                    },
                    failure: "Stopped.".to_string(),
                };
            }
            steps += 1;

            let request = ModelRequest {
                model: model.clone(),
                messages: messages.clone(),
                tools: if toolbox.specs.is_empty() {
                    None
                } else {
                    Some(toolbox.specs.clone())
                },
                ..Default::default()
            };

            let mut calls: Option<Vec<ToolCall>> = None;
            let mut step_text = String::new();
            let mut stream = self.port.stream(request);
            while let Some(event) = stream.next().await {
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
                        calls = Some(c);
                        usage = add_usage(usage, u);
                    }
                    ModelEvent::Done {
                        model: m, usage: u, ..
                    } => {
                        resolved_model = m;
                        usage = add_usage(usage, u);
                    }
                    ModelEvent::Error { message, .. } => {
                        return Outcome::Failed {
                            state: RunState {
                                messages,
                                text,
                                model: resolved_model,
                                usage,
                                steps,
                            },
                            failure: message,
                        };
                    }
                }
            }

            let Some(calls) = calls else {
                return Outcome::Answered(RunState {
                    messages,
                    text,
                    model: resolved_model,
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

            for call in &calls {
                self.note(run_id, Some(call.name.clone()), false);
                self.emit(
                    run_id,
                    RunEvent::ToolCall {
                        name: call.name.clone(),
                        args: call.arguments.clone(),
                    },
                );
                let result = toolbox.run(&call.name, &call.arguments).await;
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
            }
        }

        Outcome::Failed {
            state: RunState {
                messages,
                text,
                model: resolved_model,
                usage,
                steps,
            },
            failure: format!("Stopped after {MAX_STEPS} tool steps without an answer."),
        }
    }

    /// Writes the run row, appends the assistant's message to the
    /// conversation, and tells subscribers. Port of the settle path in the
    /// TS `drive`, minus the unverified-claim guards, second opinion,
    /// notify/badge and routine-health bookkeeping - none of that exists in
    /// S1's scope.
    fn settle(&self, run_id: &str, bot_id: &str, conversation_id: &str, outcome: Outcome) {
        let (status, failure, state) = match outcome {
            Outcome::Answered(state) => ("done", None, state),
            Outcome::Failed { state, failure } => ("failed", Some(failure), state),
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
                    state.model,
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
            if let Some(hook) = self
                .on_run_done
                .lock()
                .expect("on_run_done mutex poisoned")
                .as_ref()
            {
                hook(run_id, bot_id, conversation_id);
            }
            return;
        }

        let saved_id: Option<String> = {
            let db = self.db();
            let owner = store::get_conversation(&db, conversation_id)
                .unwrap_or_else(|err| {
                    tracing::error!("run {run_id}: failed to read conversation: {err}");
                    None
                })
                .map(|c| c.bot_id);
            let extra = store::NewMessage {
                model: Some(state.model.clone()),
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

        if status == "failed" {
            self.emit(
                run_id,
                RunEvent::Error {
                    message: failure.unwrap_or_default(),
                    status: None,
                },
            );
        } else if let Some(saved_id) = saved_id {
            self.emit(
                run_id,
                RunEvent::Done {
                    model: state.model.clone(),
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
    }

    fn take_stop(&self, run_id: &str) -> bool {
        self.stopping
            .lock()
            .expect("stopping mutex poisoned")
            .remove(run_id)
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

/// Same format as JS `new Date().toISOString()` (millisecond precision, `Z`
/// suffix) - matches `store::conversations::now_iso`, which is
/// `pub(crate)` to that crate and so not reusable here.
fn now_iso() -> String {
    chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Millis, true)
}
