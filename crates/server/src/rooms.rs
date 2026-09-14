//! The round engine: chains a room's members through one after another,
//! each seeing what came before it. Port of `src/server/app.ts`'s
//! `pendingRooms`/`startRoomTurn`/the room branch of `onRunDone`
//! (app.ts:775-980).
//!
//! Installed onto a `RunManager`'s two room hooks (`set_on_run_done`,
//! `set_start_room_turn` - S1-05) at construction time, so the manager
//! itself stays ignorant of what a room even is: it just fires a callback
//! when a run settles, and offers a callback a tool can call to wake a
//! round. A route that starts a room's FIRST leg (the owner's run) still has
//! to tell this engine about it directly - see `register` - because that
//! leg is not itself chained from anything.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use model::ladder::{Trigger, default_model};
use shared::nothing_new::declared_nothing_new;
use store::Db;

use crate::prompt::{self, HistoryTurn};
use crate::runs::{RunManager, StartOptions};

/// One room round still in flight, keyed by the run id that will finish it.
/// No `conversation_id` field - `on_run_done` already gets the finished
/// run's own conversation id from `RunManager`'s hook, and every run in one
/// round shares it.
struct PendingRound {
    all_bot_ids: Vec<String>,
    remaining: Vec<String>,
    mandatory: bool,
}

pub struct RoomEngine {
    db: Arc<Mutex<Db>>,
    runs: Arc<RunManager>,
    pending: Mutex<HashMap<String, PendingRound>>,
}

impl RoomEngine {
    /// Builds a `RoomEngine` and wires it onto `runs`'s two room hooks.
    /// Mirrors how `app.ts` sets `runs.onRunDone` and `startRoomTurn` once,
    /// at server construction.
    pub fn install(db: Arc<Mutex<Db>>, runs: Arc<RunManager>) -> Arc<Self> {
        let engine = Arc::new(RoomEngine {
            db,
            runs: Arc::clone(&runs),
            pending: Mutex::new(HashMap::new()),
        });

        let for_done = Arc::clone(&engine);
        runs.set_on_run_done(move |run_id, bot_id, conversation_id| {
            for_done.on_run_done(run_id, bot_id, conversation_id);
        });

        let for_start = Arc::clone(&engine);
        runs.set_start_room_turn(move |conversation_id, mandatory| {
            for_start.start_room_turn(conversation_id, mandatory)
        });

        engine
    }

    /// Registers the round a caller (a route) just started the OWNER's leg
    /// of - the entry point every round begins from. `on_run_done` below
    /// carries it the rest of the way, reading the conversation id back off
    /// each finished run rather than off this record. `all_bot_ids[0]` must
    /// be the run's own `bot_id`.
    pub fn register(&self, run_id: &str, all_bot_ids: Vec<String>, mandatory: bool) {
        let remaining = all_bot_ids.get(1..).map(<[_]>::to_vec).unwrap_or_default();
        self.pending
            .lock()
            .expect("pending rooms mutex poisoned")
            .insert(
                run_id.to_string(),
                PendingRound {
                    all_bot_ids,
                    remaining,
                    mandatory,
                },
            );
    }

    /// H12: starts a room's round from whatever is already in the
    /// conversation's history. The entry point `message_bot` reaches
    /// through the `start_room_turn` hook when it posts into a room by
    /// title, the same way a person typing into it does. `false` when
    /// `conversation_id` is not actually a room.
    fn start_room_turn(&self, conversation_id: &str, mandatory: bool) -> bool {
        let started = {
            let db = self.db.lock().expect("db mutex poisoned");
            let Some(conversation) =
                store::get_conversation(&db, conversation_id).expect("get_conversation")
            else {
                return false;
            };
            if conversation.kind != "room" {
                return false;
            }
            let Some(owner) = store::get_bot(&db, &conversation.bot_id).expect("get_bot") else {
                return false;
            };
            let mut round = vec![conversation.bot_id.clone()];
            round.extend(conversation.members.clone());

            store::touch_thread(&db, conversation_id).expect("touch_thread");
            let history = history_turns(&db, conversation_id);
            let instruction = prompt::room_instruction(&db, &round, &owner.id, mandatory);
            let messages = prompt::with_room_instruction(
                prompt::build_prompt(&db, &owner, &history),
                &instruction,
            );
            let model = owner.model.clone().unwrap_or_else(|| default_model(&db));

            Some((owner.id, round, messages, model))
        };
        let Some((owner_id, round, messages, model)) = started else {
            return false;
        };

        let run_id = self.runs.start(StartOptions {
            bot_id: owner_id,
            conversation_id: conversation_id.to_string(),
            model,
            messages,
            trigger: Trigger::Chat,
            // H12: what keeps a round cheap - see `model_for_run`'s doc.
            room: true,
        });
        self.register(&run_id, round, mandatory);
        true
    }

    /// Fired for EVERY run that settles, answered or failed alike - a no-op
    /// for any run this engine never `register`-ed (an ordinary one-bot
    /// chat, a `message_bot` nested call, an `@mention` inside a room).
    fn on_run_done(&self, run_id: &str, _bot_id: &str, conversation_id: &str) {
        let pending = self
            .pending
            .lock()
            .expect("pending rooms mutex poisoned")
            .remove(run_id);
        let Some(pending) = pending else { return };

        // H12: silence. A member that declared NOTHING_NEW has its own
        // message removed rather than left standing - a round where every
        // member stays quiet just ends with nothing posted, never an error.
        {
            let db = self.db.lock().expect("db mutex poisoned");
            if let Some((status, text)) = run_outcome(&db, run_id)
                && status == "done"
                && declared_nothing_new(&text)
            {
                let messages = store::list_messages(&db, conversation_id).expect("list_messages");
                if let Some(last) = messages.last()
                    && last.role == "assistant"
                    && declared_nothing_new(&last.content)
                {
                    store::delete_message(&db, &last.id).expect("delete_message");
                }
            }
        }

        let mut remaining = pending.remaining;
        if remaining.is_empty() {
            return;
        }
        let next_bot_id = remaining.remove(0);

        let started = {
            let db = self.db.lock().expect("db mutex poisoned");
            // A member archived mid-round has nothing to say; the round
            // simply ends one bot short rather than failing the rest of it.
            let Some(member) = store::get_bot(&db, &next_bot_id).expect("get_bot") else {
                return;
            };
            let history = history_turns(&db, conversation_id);
            let instruction =
                prompt::room_instruction(&db, &pending.all_bot_ids, &member.id, pending.mandatory);
            let messages = prompt::with_room_instruction(
                prompt::build_prompt(&db, &member, &history),
                &instruction,
            );
            let model = member.model.clone().unwrap_or_else(|| default_model(&db));
            (member.id, messages, model)
        };
        let (member_id, messages, model) = started;

        let next_run_id = self.runs.start(StartOptions {
            bot_id: member_id,
            conversation_id: conversation_id.to_string(),
            model,
            messages,
            trigger: Trigger::Chat,
            room: true,
        });
        // Set even when `remaining` is now empty: this is the LAST member's
        // run, and `on_run_done` still needs an entry to check ITS answer
        // for silence when it in turn finishes.
        self.pending
            .lock()
            .expect("pending rooms mutex poisoned")
            .insert(
                next_run_id,
                PendingRound {
                    all_bot_ids: pending.all_bot_ids,
                    remaining,
                    mandatory: pending.mandatory,
                },
            );
    }
}

fn history_turns(db: &Db, conversation_id: &str) -> Vec<HistoryTurn> {
    store::list_messages(db, conversation_id)
        .expect("list_messages")
        .into_iter()
        .map(|m| HistoryTurn {
            role: m.role,
            content: m.content,
        })
        .collect()
}

/// `(status, text)` off the `runs` row. Raw SQL against `Db::conn()`, same
/// pattern `runs.rs` itself uses - no query API for that table exists yet.
fn run_outcome(db: &Db, run_id: &str) -> Option<(String, String)> {
    db.conn()
        .query_row(
            "SELECT status, text FROM runs WHERE id = ?1",
            rusqlite::params![run_id],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .ok()
}
