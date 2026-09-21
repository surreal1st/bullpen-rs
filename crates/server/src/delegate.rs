//! One bot asking another — depth limits and roster lookup.
//!
//! Port of `projects/bullpen-night/src/server/delegate.ts`. Full multi-step
//! delegated turns (nested toolbox) follow `message_bot`'s current one-call
//! shape until a shared `run_turn` seam exists for nested runs.

use std::sync::{Arc, Mutex};

use model::ladder::Trigger;
use model::{ModelPort, ModelUsage};
use store::Db;

pub const MAX_DELEGATION_DEPTH: u32 = 1;

pub const DEPTH_REFUSAL: &str = "You were asked a question by another bot, so you cannot pass it on again. Answer it yourself or say plainly that it is not your area.";

#[derive(Debug, Clone, PartialEq)]
pub struct AskResult {
    pub reply: String,
    pub usage: Option<ModelUsage>,
    pub error: Option<String>,
}

/// Resolves a roster name or id to a bot — port of TS `findBot` without scope
/// (same roster as `message_bot` today: `list_bots(false)`).
pub fn find_bot(db: &Db, name_or_id: &str) -> Option<(String, String)> {
    let needle = name_or_id.trim().to_lowercase();
    if needle.is_empty() {
        return None;
    }
    let all = store::list_bots(db, false).ok()?;
    if let Some(direct) = all.iter().find(|b| b.id == needle && !b.archived) {
        return Some((direct.id.clone(), direct.name.clone()));
    }
    if let Some(by_name) = all
        .iter()
        .find(|b| b.name.eq_ignore_ascii_case(name_or_id.trim()))
    {
        return Some((by_name.id.clone(), by_name.name.clone()));
    }
    if let Some(by_purpose) = all
        .iter()
        .find(|b| b.purpose.eq_ignore_ascii_case(name_or_id.trim()))
    {
        return Some((by_purpose.id.clone(), by_purpose.name.clone()));
    }
    let partial: Vec<_> = all
        .iter()
        .filter(|b| b.name.to_lowercase().starts_with(&needle))
        .collect();
    if partial.len() == 1 {
        return Some((partial[0].id.clone(), partial[0].name.clone()));
    }
    None
}

pub async fn ask_colleague(
    db: &Arc<Mutex<Db>>,
    port: &Arc<dyn ModelPort>,
    caller_bot_id: &str,
    to_bot_id: &str,
    question: &str,
    trigger: Trigger,
    room: bool,
) -> AskResult {
    let outcome = crate::tools::message_bot::ask_colleague(
        db,
        port,
        caller_bot_id,
        to_bot_id,
        question,
        trigger,
        room,
    )
    .await;
    AskResult {
        reply: outcome.reply,
        usage: outcome.usage,
        error: outcome.error,
    }
}

pub use crate::tools::message_bot::ColleagueAskOutcome;
