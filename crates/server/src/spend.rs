//! S2-05: month-to-date spend and the ceiling gate. Port of
//! `projects/bullpen-night/src/server/spend.ts`.

use rusqlite::OptionalExtension;
use serde::{Deserialize, Serialize};
use store::Db;

const CEILING_KEY: &str = "spend.ceiling_usd";
const DEFAULT_CEILING: f64 = 10.0;

/// Platform-wide ceiling, dollars.
pub fn get_ceiling(db: &Db) -> f64 {
    let row: Option<String> = db
        .conn()
        .query_row(
            "SELECT value FROM settings WHERE key = ?",
            rusqlite::params![CEILING_KEY],
            |row| row.get(0),
        )
        .optional()
        .unwrap_or(None);

    row.and_then(|value| {
        value
            .parse::<f64>()
            .ok()
            .filter(|v| v.is_finite() && *v >= 0.0)
    })
    .unwrap_or(DEFAULT_CEILING)
}

/// Set the platform-wide ceiling, dollars. Returns the cleaned value (max 0).
pub fn set_ceiling(db: &Db, usd: f64) -> rusqlite::Result<f64> {
    let clean = f64::max(0.0, usd);
    db.conn().execute(
        "INSERT INTO settings (key, value) VALUES (?, ?) ON CONFLICT(key) DO UPDATE SET value = excluded.value",
        rusqlite::params![CEILING_KEY, clean.to_string()],
    )?;
    Ok(clean)
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum GateResult {
    /// The run may start. `warning` is non-empty when approaching the ceiling
    /// (≤15% headroom).
    Allowed { warning: Option<String> },
    /// The run must not start. `reason` explains why.
    Denied { reason: String },
}

/// Decides whether a new run may start, reading account spend from OpenRouter
/// and the platform ceiling from settings. In-flight runs are never touched:
/// stopping mid-answer wastes what was already spent and loses the reply.
/// The gate only refuses to START.
///
/// On a read failure (network, etc.), allows the run: a hard stop on a network
/// blip is worse than the overspend it prevents, and the account has its own
/// hard limit underneath.
pub fn gate_run(_db: &Db, ceiling: f64, account_usage: Option<f64>) -> GateResult {
    match account_usage {
        Some(used) if used >= ceiling => GateResult::Denied {
            reason: format!(
                "Spend ceiling reached. OpenRouter reports ${:.2} used against your ${:.2} ceiling. Runs already in flight will finish. Raise the ceiling in the spend panel to continue.",
                used, ceiling
            ),
        },
        Some(used) => {
            let headroom = ceiling - used;
            if headroom <= ceiling * 0.15 {
                GateResult::Allowed {
                    warning: Some(format!(
                        "${:.2} left before your ${:.2} ceiling stops new runs.",
                        headroom, ceiling
                    )),
                }
            } else {
                GateResult::Allowed { warning: None }
            }
        }
        None => {
            // Network read failed - allow and warn
            GateResult::Allowed {
                warning: Some("Could not read your OpenRouter balance.".to_string()),
            }
        }
    }
}

/// Current calendar month as `YYYY-MM`.
pub fn current_month(now: chrono::DateTime<chrono::Utc>) -> String {
    now.format("%Y-%m").to_string()
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct BotSpend {
    pub bot_id: String,
    pub bot_name: String,
    pub cost_usd: f64,
    pub runs: i64,
    pub input_tokens: i64,
    pub output_tokens: i64,
    pub cached_tokens: i64,
}

/// Per-bot spend for a calendar month, summed from provider-reported costs.
pub fn spend_by_bot(db: &Db, month: &str) -> rusqlite::Result<Vec<BotSpend>> {
    let mut stmt = db.conn().prepare(
        "SELECT b.id,
                b.name,
                COALESCE(SUM(m.cost_usd), 0),
                COUNT(m.id),
                COALESCE(SUM(m.input_tokens), 0),
                COALESCE(SUM(m.output_tokens), 0),
                COALESCE(SUM(m.cached_tokens), 0)
           FROM messages m
           JOIN conversations c ON c.id = m.conversation_id
           JOIN bots b ON b.id = c.bot_id
          WHERE m.role = 'assistant'
            AND m.cost_usd IS NOT NULL
            AND substr(m.created_at, 1, 7) = ?
          GROUP BY b.id, b.name
          ORDER BY COALESCE(SUM(m.cost_usd), 0) DESC",
    )?;

    let rows = stmt.query_map(rusqlite::params![month], |row| {
        Ok(BotSpend {
            bot_id: row.get(0)?,
            bot_name: row.get(1)?,
            cost_usd: row.get(2)?,
            runs: row.get(3)?,
            input_tokens: row.get(4)?,
            output_tokens: row.get(5)?,
            cached_tokens: row.get(6)?,
        })
    })?;

    let mut result = Vec::new();
    for row in rows {
        result.push(row?);
    }
    Ok(result)
}
