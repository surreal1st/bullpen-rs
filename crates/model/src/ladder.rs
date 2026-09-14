//! The escalation ladder: routing model selection by trigger type and escalation
//! tier. Port of `src/server/escalation.ts`.

use crate::CHEAP_DEFAULT_MODEL;
use serde::{Deserialize, Serialize};
use store::Db;

/// Reaching a better model when a cheap one is stuck.
///
/// **Why this is not a permission.** Permissions answer "may this bot use this
/// tool", and that answer deliberately cannot depend on what started the run:
/// making it depend on the trigger is the exact bug that left Rakazo's webhook
/// runs permanently mute. Which MODEL a run may use is a different question, and
/// it legitimately does depend on the trigger, because Josh's rule is about
/// unattended spending rather than about capability.
///
/// So the two live in different places on purpose, and the model rule is
/// enforced centrally rather than at each call site.
const PREMIUM_KEY: &str = "models.premium";
const DEFAULT_KEY: &str = "models.default";
const MID_KEY: &str = "models.mid";

/// The default escalation target.
///
/// Josh's standing rule is never Fable unless it is heavy logic or he asks for
/// it by name. Pressing escalate on a problem a cheaper model already failed is
/// both of those at once, which is what makes this a defensible default rather
/// than a violation of it.
pub const DEFAULT_PREMIUM_MODEL: &str = "anthropic/claude-fable-5.1";

/// Names Josh called out. Matched loosely because vendors rename slugs.
const PREMIUM_MARKERS: &[&str] = &["fable", "sol", "astra", "opus", "gpt-6", "pro"];

/// The rung between the specialists and the top one.
///
/// Josh, 2026-09-10: tier 1 the specialists, tier 2 Opus, tier 3 Fable. Opus is
/// half Fable's output price with the same million-token window, so a problem
/// that beat a specialist gets a genuinely stronger model before anything
/// reaches $50/M.
pub const DEFAULT_MID_MODEL: &str = "anthropic/claude-opus-5";

/// The specialist models for each escalation kind.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Tier1Models {
    pub code: &'static str,
    pub reason: &'static str,
    pub vision: &'static str,
}

pub const DEFAULT_TIER1: Tier1Models = Tier1Models {
    code: "x-ai/grok-build-0.1",
    reason: "anthropic/claude-sonnet-5",
    vision: "google/gemini-3.8-flash",
};

/// The specialist models for each escalation kind, as owned strings (for HTTP responses).
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Tier1ModelsResponse {
    pub code: String,
    pub reason: String,
    pub vision: String,
}

/// What started the run: a user chat, a scheduled timer, a webhook, or a goal.
/// Determines model floor and whether escalation is allowed.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Trigger {
    Chat,
    Routine,
    Webhook,
    Goal,
}

/// The kind of escalation being requested.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum EscalationKind {
    Code,
    Reason,
    Vision,
}

impl EscalationKind {
    pub fn as_str(&self) -> &'static str {
        match self {
            EscalationKind::Code => "code",
            EscalationKind::Reason => "reason",
            EscalationKind::Vision => "vision",
        }
    }

    fn settings_key(&self) -> &'static str {
        match self {
            EscalationKind::Code => "models.tier1.code",
            EscalationKind::Reason => "models.tier1.reason",
            EscalationKind::Vision => "models.tier1.vision",
        }
    }
}

/// What a bot runs when it has no pin of its own.
///
/// A constant until now, which made "cheap default" something Josh could read in
/// the UI and not change. It is the most consequential setting on the platform:
/// it is what fifteen bots and every routine actually run.
pub fn default_model(db: &Db) -> String {
    db.settings_get(DEFAULT_KEY)
        .ok()
        .flatten()
        .unwrap_or_else(|| CHEAP_DEFAULT_MODEL.to_string())
}

pub fn set_default_model(db: &Db, model: &str) -> String {
    let _ = db.settings_set(DEFAULT_KEY, model);
    model.to_string()
}

pub fn mid_model(db: &Db) -> String {
    db.settings_get(MID_KEY)
        .ok()
        .flatten()
        .unwrap_or_else(|| DEFAULT_MID_MODEL.to_string())
}

pub fn set_mid_model(db: &Db, model: &str) -> String {
    let _ = db.settings_set(MID_KEY, model);
    model.to_string()
}

pub fn premium_model(db: &Db) -> String {
    db.settings_get(PREMIUM_KEY)
        .ok()
        .flatten()
        .unwrap_or_else(|| DEFAULT_PREMIUM_MODEL.to_string())
}

pub fn set_premium_model(db: &Db, model: &str) -> String {
    let _ = db.settings_set(PREMIUM_KEY, model);
    model.to_string()
}

pub fn tier1_model(db: &Db, kind: EscalationKind) -> String {
    db.settings_get(kind.settings_key())
        .ok()
        .flatten()
        .unwrap_or_else(|| match kind {
            EscalationKind::Code => DEFAULT_TIER1.code.to_string(),
            EscalationKind::Reason => DEFAULT_TIER1.reason.to_string(),
            EscalationKind::Vision => DEFAULT_TIER1.vision.to_string(),
        })
}

pub fn set_tier1_model(db: &Db, kind: EscalationKind, model: &str) -> String {
    let _ = db.settings_set(kind.settings_key(), model);
    model.to_string()
}

/// Whether a model name looks premium (matches any of the PREMIUM_MARKERS).
pub fn looks_premium(model: &str) -> bool {
    let id = model.to_lowercase();
    PREMIUM_MARKERS.iter().any(|marker| id.contains(marker))
}

/// The only place a run's model is decided.
///
/// Anything not started by Josh in a conversation is forced to the cheap model.
/// Not warned, not defaulted: forced, whatever the bot is pinned to and whatever
/// anyone passes in.
///
/// 🔴 `room` forces the same floor on a room ROUND, which is a "chat" trigger
/// and stays one - a member speaking in a room keeps chat's permissions and
/// chat's escalation, and only its model is capped. The reason is arithmetic:
/// one room question wakes every member, so a four-bot room is four runs over
/// the same long shared history. On 2026-09-13 that was ~19,000 input tokens
/// EACH on `anthropic/claude-sonnet-5`, $0.154 for one question and $3.32 in a
/// night. A member's own pin is what this overrides - it is the pin that put
/// three of them on Sonnet. A narrowed `@mention` inside a room is NOT a round
/// and does not pass this: that is one bot answering one question, priced like
/// any other chat turn.
///
/// 🔴 A room ignores `requested` OUTRIGHT rather than flooring it the way a
/// timer run does, and the difference matters: "sonnet" is deliberately absent
/// from PREMIUM_MARKERS (see `looks_premium`), so the floor below would have
/// waved `anthropic/claude-sonnet-5` straight through - which is the exact
/// model, and the exact pin, that ran the bill up.
pub fn model_for_run(db: &Db, trigger: Trigger, requested: &str, room: bool) -> String {
    if room {
        return safe_fallback(db);
    }
    if trigger == Trigger::Chat {
        return requested.to_string();
    }
    // Both tests matter. The name markers catch a model Josh never configured,
    // and the configured target catches one whose name says nothing.
    if looks_premium(requested) || requested == premium_model(db) {
        return safe_fallback(db);
    }
    requested.to_string()
}

/// What a timer run drops to, with a floor that cannot be configured away.
///
/// The platform default became settable, and that quietly broke this: the
/// downgrade target IS the default, so setting the default to Fable made the
/// guard downgrade Fable to Fable and put a premium model on every 06:00 run.
/// The route refuses a premium default, and this refuses it a second time, at
/// the point of use, where a hand-edited database or a vendor rename cannot get
/// past it. Josh's rule is that no timer ever reaches Fable, Sol or Astra.
pub fn safe_fallback(db: &Db) -> String {
    let configured = default_model(db);
    if looks_premium(&configured) || configured == premium_model(db) {
        CHEAP_DEFAULT_MODEL.to_string()
    } else {
        configured
    }
}

/// Whether this run may reach for a better model mid-flight.
pub fn may_escalate(trigger: Trigger) -> bool {
    trigger == Trigger::Chat
}

/// Which rung a model sits on. Unknown models are tier 0: the ladder climbs, never skips.
///
/// A configured rung is checked FIRST, before the premium name markers. Josh
/// deliberately put Opus on a tier-1 rung under Fable, and the marker list
/// contains "opus" - reading the name first would have called Opus tier 2, so a
/// bot that had just failed ON Opus would be told there was nothing above it and
/// Fable would be unreachable. What a model IS configured as beats what its name
/// looks like.
pub fn tier_of(db: &Db, model: &str) -> u8 {
    if model == premium_model(db) {
        return 3;
    }
    if model == mid_model(db) {
        return 2;
    }
    // Check all tier 1 models
    if model == tier1_model(db, EscalationKind::Code)
        || model == tier1_model(db, EscalationKind::Reason)
        || model == tier1_model(db, EscalationKind::Vision)
    {
        return 1;
    }
    // Only after every configured rung has been checked. "opus" is in the name
    // markers AND is the configured tier 2, so reading names first would call it
    // the top rung and make Fable unreachable from it.
    if looks_premium(model) { 3 } else { 0 }
}

/// Getter functions using TS naming convention (for routes).
///
/// These mirror the TS `getDefaultModel`, `getMidModel`, etc. from app.ts.
pub fn get_default_model(db: &Db) -> String {
    default_model(db)
}

pub fn get_mid_model(db: &Db) -> String {
    mid_model(db)
}

pub fn get_premium_model(db: &Db) -> String {
    premium_model(db)
}

/// All tier1 models in one call, for GET /api/tier1-models.
pub fn tier1_models(db: &Db) -> Tier1ModelsResponse {
    Tier1ModelsResponse {
        code: tier1_model(db, EscalationKind::Code),
        reason: tier1_model(db, EscalationKind::Reason),
        vision: tier1_model(db, EscalationKind::Vision),
    }
}
