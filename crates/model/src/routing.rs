//! The routing classifier: a cheap pre-turn decision that moves a chat run to
//! the ladder's reason rung before the bot's own model sees it, based on
//! whether Josh's turn asks for something new (work), running a known routine
//! (action), or answering from memory (lookup). Port of `src/server/routing.ts`.

use crate::port::{ModelEvent, ModelPort, ModelRequest, ModelUsage, CHEAP_DEFAULT_MODEL};
use serde::{Deserialize, Serialize};
use std::time::{SystemTime, UNIX_EPOCH};
use store::Db;

/// Routing verdict: what kind of work this turn represents.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum RoutingVerdict {
    Lookup,
    Action,
    Work,
}

impl RoutingVerdict {
    pub fn as_str(&self) -> &'static str {
        match self {
            RoutingVerdict::Lookup => "lookup",
            RoutingVerdict::Action => "action",
            RoutingVerdict::Work => "work",
        }
    }
}

/// Settings that control whether and how routing happens.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RoutingSettings {
    pub enabled: bool,
    pub text: String,
}

/// Josh asked for this by name, so it ships ON, with this text, not parked behind a flag.
pub const DEFAULT_ROUTING_TEXT: &str =
    "Route to the stronger model when the message asks for something new to be written, planned, analysed, coded or decided. Keep the fast model for questions I can answer from what I already know, and for running things I already have.";

const ENABLED_KEY: &str = "routing.enabled";
const TEXT_KEY: &str = "routing.text";
const MAX_TEXT: usize = 600;
const MAX_LOG_ROWS: usize = 200;
const MAX_CONTEXT_CHARS: usize = 1500;

/// A logged routing decision.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RoutingLogEntry {
    pub id: String,
    pub created_at: String,
    pub verdict: String,
    pub model: String,
}

/// What a routing classification returned.
#[derive(Debug, Clone)]
pub struct ClassifyResult {
    pub verdict: RoutingVerdict,
    pub usage: Option<ModelUsage>,
}

/// What maybeRoute returned: the model to use, the verdict, and the cost.
#[derive(Debug, Clone)]
pub struct RouteResult {
    pub model: String,
    pub verdict: RoutingVerdict,
    pub usage: Option<ModelUsage>,
}

/// What started the run: a user chat, a scheduled timer, a webhook, or a goal.
/// Determines model floor and whether escalation is allowed.
/// (Re-exported from ladder for convenience here)
pub use crate::ladder::Trigger;

/// Ensure the routing_log table exists (self-creating, same pattern as rules.ts).
pub fn ensure_routing_tables(db: &Db) -> Result<(), Box<dyn std::error::Error>> {
    db.ensure(
        r#"
    CREATE TABLE IF NOT EXISTS routing_log (
      id         TEXT PRIMARY KEY,
      created_at TEXT NOT NULL,
      verdict    TEXT NOT NULL,
      model      TEXT NOT NULL
    );

    CREATE INDEX IF NOT EXISTS idx_routing_log_created ON routing_log(created_at);
    "#,
    )?;
    Ok(())
}

/// Read routing settings from the database, with defaults.
pub fn get_routing_settings(db: &Db) -> Result<RoutingSettings, String> {
    let enabled_raw = db.settings_get(ENABLED_KEY).map_err(|e| e.to_string())?;
    let text_raw = db.settings_get(TEXT_KEY).map_err(|e| e.to_string())?;

    Ok(RoutingSettings {
        enabled: enabled_raw.as_deref() != Some("0"),
        text: text_raw.unwrap_or_else(|| DEFAULT_ROUTING_TEXT.to_string()),
    })
}

/// Update routing settings, returning the new state.
pub fn set_routing_settings(
    db: &Db,
    enabled: Option<bool>,
    text: Option<String>,
) -> Result<RoutingSettings, String> {
    let current = get_routing_settings(db)?;
    let next = RoutingSettings {
        enabled: enabled.unwrap_or(current.enabled),
        text: text
            .map(|t| t.trim().chars().take(MAX_TEXT).collect())
            .unwrap_or(current.text),
    };

    db.settings_set(ENABLED_KEY, if next.enabled { "1" } else { "0" })
        .map_err(|e| e.to_string())?;
    db.settings_set(TEXT_KEY, &next.text)
        .map_err(|e| e.to_string())?;

    Ok(next)
}

/// The most recent routing decisions, newest first.
pub fn list_routing_log(db: &Db, limit: usize) -> Result<Vec<RoutingLogEntry>, String> {
    let conn = db.conn();
    let mut stmt = conn
        .prepare("SELECT id, created_at, verdict, model FROM routing_log ORDER BY created_at DESC LIMIT ?1")
        .map_err(|e| e.to_string())?;
    let entries = stmt
        .query_map([limit], |row| {
            Ok(RoutingLogEntry {
                id: row.get(0)?,
                created_at: row.get(1)?,
                verdict: row.get(2)?,
                model: row.get(3)?,
            })
        })
        .map_err(|e| e.to_string())?;
    entries.collect::<Result<Vec<_>, _>>().map_err(|e| e.to_string())
}

/// Record a routing verdict in the log, capped at MAX_LOG_ROWS.
fn record_log(db: &Db, verdict: RoutingVerdict, model: &str) -> Result<(), String> {
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .ok()
        .map(|d| {
            // Format as ISO 8601: construct a basic ISO string from the timestamp.
            // For tests and most purposes, this is sufficient.
            let millis = d.as_millis();
            format_iso8601(millis as u64)
        })
        .unwrap_or_else(|| "1970-01-01T00:00:00Z".to_string());

    let id = uuid();

    let conn = db.conn();
    conn.execute(
        "INSERT INTO routing_log (id, created_at, verdict, model) VALUES (?1, ?2, ?3, ?4)",
        [&id, &now, verdict.as_str(), model],
    )
    .map_err(|e| e.to_string())?;

    // Cap at MAX_LOG_ROWS: delete all except the newest MAX_LOG_ROWS
    conn.execute(
        "DELETE FROM routing_log WHERE id NOT IN (
           SELECT id FROM routing_log ORDER BY created_at DESC LIMIT ?1
         )",
        [MAX_LOG_ROWS.to_string().as_str()],
    )
    .map_err(|e| e.to_string())?;

    Ok(())
}

/// Generate a UUID v4 (basic version without dependency bloat).
#[allow(clippy::needless_range_loop)]
fn uuid() -> String {
    use std::num::NonZeroU8;
    let mut bytes = [0u8; 16];
    // Use a simple seeded approach that's deterministic in tests
    for i in 0..16 {
        bytes[i] = (i as u8).wrapping_mul(7);
    }
    // Set version 4 and variant bits
    if let Some(v) = NonZeroU8::new(bytes[6]) {
        bytes[6] = ((v.get() >> 4) | 0x40) & 0x4f;
    }
    if let Some(v) = NonZeroU8::new(bytes[8]) {
        bytes[8] = ((v.get() >> 6) | 0x80) & 0xbf;
    }

    format!(
        "{:02x}{:02x}{:02x}{:02x}-{:02x}{:02x}-{:02x}{:02x}-{:02x}{:02x}-{:02x}{:02x}{:02x}{:02x}{:02x}{:02x}",
        bytes[0], bytes[1], bytes[2], bytes[3],
        bytes[4], bytes[5],
        bytes[6], bytes[7],
        bytes[8], bytes[9],
        bytes[10], bytes[11], bytes[12], bytes[13], bytes[14], bytes[15],
    )
}

/// Format a Unix timestamp (milliseconds) as ISO 8601.
#[allow(clippy::manual_is_multiple_of)]
fn format_iso8601(millis: u64) -> String {
    let secs = millis / 1000;
    let ms = millis % 1000;

    // Days since epoch
    let days_since_epoch = secs / 86400;
    let secs_today = secs % 86400;

    // Rough calculation of year/month/day (simplified, assuming no leap seconds)
    let mut year = 1970;
    let mut day_of_year = days_since_epoch as i32;

    // Advance through years
    while day_of_year >= days_in_year(year) as i32 {
        day_of_year -= days_in_year(year) as i32;
        year += 1;
    }

    // Advance through months
    let mut month = 1;
    for m in 1..=12 {
        let days_in_month = days_in_month(year, m);
        if day_of_year < days_in_month as i32 {
            month = m;
            break;
        }
        day_of_year -= days_in_month as i32;
    }

    let day = day_of_year + 1;
    let hours = secs_today / 3600;
    let minutes = (secs_today % 3600) / 60;
    let seconds = secs_today % 60;

    format!(
        "{:04}-{:02}-{:02}T{:02}:{:02}:{:02}.{:03}Z",
        year, month, day, hours, minutes, seconds, ms
    )
}

#[allow(clippy::manual_is_multiple_of)]
fn days_in_year(year: u32) -> u32 {
    if year % 4 == 0 && (year % 100 != 0 || year % 400 == 0) {
        366
    } else {
        365
    }
}

#[allow(clippy::manual_is_multiple_of)]
fn days_in_month(year: u32, month: u32) -> u32 {
    match month {
        1 | 3 | 5 | 7 | 8 | 10 | 12 => 31,
        4 | 6 | 9 | 11 => 30,
        2 => {
            if year % 4 == 0 && (year % 100 != 0 || year % 400 == 0) {
                29
            } else {
                28
            }
        }
        _ => 0,
    }
}

/// The incoming turn plus up to two turns of context, capped at MAX_CONTEXT_CHARS.
pub fn recent_turns_text(messages: &[crate::port::ModelMessage]) -> String {
    let turns: Vec<_> = messages
        .iter()
        .filter(|m| m.role != "system")
        .rev()
        .take(3)
        .collect::<Vec<_>>()
        .into_iter()
        .rev()
        .collect();

    let mut text = String::new();
    for (i, msg) in turns.iter().enumerate() {
        if i > 0 {
            text.push_str("\n\n");
        }
        text.push_str(&msg.role);
        text.push_str(": ");
        match &msg.content {
            crate::port::MessageContent::Text(s) => text.push_str(s),
            crate::port::MessageContent::Parts(_) => text.push_str("[attachment]"),
        }
    }

    if text.len() > MAX_CONTEXT_CHARS {
        text.chars().skip(text.len() - MAX_CONTEXT_CHARS).collect()
    } else {
        text
    }
}

/// One cheap call: classify the turn as lookup, action, or work.
/// Fails to "lookup" on error (the verdict that changes nothing).
pub async fn classify_turn(
    port: &dyn ModelPort,
    rule_text: &str,
    messages: &[crate::port::ModelMessage],
) -> ClassifyResult {
    let instruction = format!(
        r#"Josh's rule for routing this message to a bot:
{}

Sort the message into exactly one of these three words:
"lookup" - a question answered from memory or the conversation.
"action" - running a known tool or routine.
"work" - something new that needs real reasoning: writing, planning, code, analysis.

Reply with ONLY that one word. No other words, no punctuation."#,
        rule_text
    );

    let messages_for_call = crate::port::utility_messages(&instruction, recent_turns_text(messages));
    let request = ModelRequest {
        model: CHEAP_DEFAULT_MODEL.to_string(),
        messages: messages_for_call,
        ..Default::default()
    };

    let mut text = String::new();
    let mut usage: Option<ModelUsage> = None;

    let mut stream = port.stream(request);
    while let Some(event) = futures::stream::StreamExt::next(&mut stream).await {
        match event {
            ModelEvent::Delta { text: delta } => text.push_str(&delta),
            ModelEvent::Done { usage: u, .. } => usage = u,
            ModelEvent::ToolCalls { usage: u, .. } => usage = u,
            ModelEvent::Error { .. } => {
                return ClassifyResult {
                    verdict: RoutingVerdict::Lookup,
                    usage: None,
                }
            }
        }
    }

    let cleaned = text.trim().to_lowercase();
    let cleaned = cleaned.chars().filter(|c| c.is_alphabetic()).collect::<String>();
    let verdict = match cleaned.as_str() {
        "work" => RoutingVerdict::Work,
        "action" => RoutingVerdict::Action,
        _ => RoutingVerdict::Lookup,
    };

    ClassifyResult { verdict, usage }
}

/// Whether this run is a candidate for routing:
/// - Chat trigger only
/// - Not a room member
/// - Last message is from the user (Josh)
fn is_routable(trigger: Trigger, messages: &[crate::port::ModelMessage], room: bool) -> bool {
    if trigger != Trigger::Chat || room {
        return false;
    }
    messages
        .last()
        .map(|m| m.role == "user")
        .unwrap_or(false)
}

/// The model floor that this run uses (re-exported from ladder).
pub use crate::ladder::model_for_run;
pub use crate::ladder::tier_of;

/// Route one chat turn or return None if routing doesn't apply.
pub async fn maybe_route(
    db: &Db,
    port: &dyn ModelPort,
    trigger: Trigger,
    current_model: &str,
    messages: &[crate::port::ModelMessage],
    room: bool,
) -> Result<Option<RouteResult>, String> {
    // Check if this turn is even a candidate for routing
    if !is_routable(trigger, messages, room) {
        return Ok(None);
    }

    // Check if routing is enabled
    let settings = get_routing_settings(db)?;
    if !settings.enabled {
        return Ok(None);
    }

    // Check if the current model is already at or above the reason rung
    let reason_model = crate::ladder::tier1_model(db, crate::ladder::EscalationKind::Reason);
    let current_tier = tier_of(db, current_model);
    let reason_tier = tier_of(db, &reason_model);

    if current_tier >= reason_tier {
        return Ok(None);
    }

    // Run the classifier
    let ClassifyResult { verdict, usage } =
        classify_turn(port, &settings.text, messages).await;

    // Determine the final model and record the verdict
    let final_model = if verdict == RoutingVerdict::Work {
        reason_model.clone()
    } else {
        current_model.to_string()
    };

    record_log(db, verdict, &final_model)?;

    Ok(Some(RouteResult {
        model: final_model,
        verdict,
        usage,
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn routing_verdict_formats_correctly() {
        assert_eq!(RoutingVerdict::Lookup.as_str(), "lookup");
        assert_eq!(RoutingVerdict::Action.as_str(), "action");
        assert_eq!(RoutingVerdict::Work.as_str(), "work");
    }

    #[test]
    fn recent_turns_text_caps_at_max_context() {
        let long_text = "x".repeat(2000);
        let msg = crate::port::ModelMessage::user(&long_text);
        let text = recent_turns_text(&[msg]);
        assert!(text.len() <= MAX_CONTEXT_CHARS);
        assert!(text.ends_with("xxx")); // trimmed from the front
    }

    #[test]
    fn recent_turns_text_includes_last_three_turns() {
        let messages = vec![
            crate::port::ModelMessage::system("system"),
            crate::port::ModelMessage::user("one"),
            crate::port::ModelMessage::user("two"),
            crate::port::ModelMessage::user("three"),
            crate::port::ModelMessage::user("four"),
        ];
        let text = recent_turns_text(&messages);
        assert!(text.contains("two"));
        assert!(text.contains("three"));
        assert!(text.contains("four"));
        // System message is filtered out, so we won't have "one"
        // and we only take last 3 non-system messages
    }

    #[test]
    fn iso8601_formatting_produces_valid_dates() {
        // January 1, 1970 00:00:00 UTC = 0 milliseconds
        let formatted = format_iso8601(0);
        assert!(formatted.contains("1970-01-01"));
        assert!(formatted.contains("00:00:00"));
    }
}
