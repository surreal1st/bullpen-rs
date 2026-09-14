//! The tools a run's model may call. S1's first six: `say`, `ask_josh`,
//! `remember`, `message_bot`, and the two Grok gaps, `create_room` and
//! `add_to_room`. Port of the relevant slices of `src/server/bot-tools.ts`,
//! `conversing.ts`, `delegate.ts` and `app.ts`'s `message_bot`-into-a-room
//! branch. Every one of these six is `allow` for S1 - no approvals, no
//! gating - so there is no `gate`/`Decision` seam here yet; that is S2.

mod add_to_room;
mod ask_josh;
mod create_room;
mod message_bot;
mod remember;
mod say;

use std::future::Future;
use std::pin::Pin;
use std::sync::{Arc, Mutex};

use model::ladder::Trigger;
use model::{ModelPort, ModelUsage, ToolSpec};
use store::Db;

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
}

impl ToolBox {
    /// Runs one tool call. An unknown name is never a panic - a model can
    /// hallucinate a tool name same as anything else, and the run should
    /// hear about that as an ordinary tool result, not crash over it.
    pub async fn run(&self, name: &str, args: &str) -> (String, Option<ModelUsage>) {
        (self.handler)(name.to_string(), args.to_string()).await
    }
}

/// Builds the S1 toolbox for one bot's run. F2: `trigger`/`room` are the
/// CALLER's, forwarded only to `message_bot`'s bot branch so a nested
/// delegated call is floored the same way the caller's own model choice is.
pub fn build(
    db: Arc<Mutex<Db>>,
    port: Arc<dyn ModelPort>,
    bot_id: String,
    room_hook: RoomHook,
    trigger: Trigger,
    room: bool,
) -> ToolBox {
    let specs = vec![
        say::spec(),
        ask_josh::spec(),
        remember::spec(),
        message_bot::spec(),
        create_room::spec(),
        add_to_room::spec(),
    ];

    let handler: Arc<dyn Fn(String, String) -> ToolFuture + Send + Sync> =
        Arc::new(move |name, args| {
            let db = Arc::clone(&db);
            let port = Arc::clone(&port);
            let bot_id = bot_id.clone();
            let room_hook = Arc::clone(&room_hook);
            Box::pin(async move {
                match name.as_str() {
                    "say" => (say::run(&db, &bot_id, &args), None),
                    "ask_josh" => (ask_josh::run(&db, &bot_id, &args), None),
                    "remember" => (remember::run(&db, &bot_id, &args), None),
                    "create_room" => (create_room::run(&db, &bot_id, &args), None),
                    "add_to_room" => (add_to_room::run(&db, &args), None),
                    "message_bot" => {
                        message_bot::run(&db, &port, &bot_id, &room_hook, trigger, room, &args)
                            .await
                    }
                    other => (format!("Unknown tool: {other}"), None),
                }
            })
        });

    ToolBox { specs, handler }
}
