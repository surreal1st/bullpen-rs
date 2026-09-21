//! S2-05: month-to-date spend and the ceiling gate. Port of
//! `projects/bullpen-night/src/server/spend.ts`.

use std::time::{Duration, Instant};

use rusqlite::OptionalExtension;
use serde::{Deserialize, Serialize};
use store::{Db, ListScope, get_user};

use crate::scope::Scope;

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
/// Member-specific monthly ceiling, or none when the platform ceiling applies.
pub fn user_ceiling(db: &Db, scope: &Scope) -> Option<f64> {
    if scope.is_owner {
        return None;
    }
    let user = get_user(db, &scope.user_id).ok()??;
    user.ceiling_usd
}

/// One person's provider-reported spend for a calendar month (`YYYY-MM`).
pub fn spent_by_user(db: &Db, month: &str, filter: &ListScope) -> rusqlite::Result<f64> {
    let scope_sql = filter.and_sql("b");
    let sql = format!(
        "SELECT COALESCE(SUM(m.cost_usd), 0)
           FROM messages m
           JOIN conversations c ON c.id = m.conversation_id
           JOIN bots b ON b.id = c.bot_id
          WHERE m.role = 'assistant'
            AND m.cost_usd IS NOT NULL
            AND substr(m.created_at, 1, 7) = ?1{scope_sql}",
    );
    let (owner, user) = filter.bind_values();
    db.conn()
        .query_row(&sql, rusqlite::params![month, owner, user], |row| {
            row.get(0)
        })
}

/// Whether a member's own ceiling blocks unattended work (no OpenRouter read).
pub fn over_user_ceiling(
    db: &Db,
    scope: &Scope,
    now: chrono::DateTime<chrono::Utc>,
) -> Option<String> {
    let ceiling = user_ceiling(db, scope)?;
    let spent = spent_by_user(db, &current_month(now), &scope.list_filter()).ok()?;
    if spent < ceiling {
        return None;
    }
    Some(format!(
        "Paused: ${spent:.2} of a ${ceiling:.2} monthly ceiling"
    ))
}

pub fn gate_run(
    db: &Db,
    scope: Option<&Scope>,
    ceiling: f64,
    account_usage: Option<f64>,
) -> GateResult {
    if let Some(scope) = scope
        && let Some(member_ceiling) = user_ceiling(db, scope)
    {
        match spent_by_user(db, &current_month(chrono::Utc::now()), &scope.list_filter()) {
            Ok(spent) if spent >= member_ceiling => {
                return GateResult::Denied {
                    reason: format!(
                        "Spend ceiling reached. You have used ${spent:.2} of your ${member_ceiling:.2} this month. Ask Josh to raise it.",
                    ),
                };
            }
            Err(err) => {
                tracing::error!("member spend sum failed: {err}");
            }
            _ => {}
        }
    }

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
    /// COST-01: how many of this bot's assistant messages this month came
    /// back with a usage frame that carried no cost at all. `cost_usd` above
    /// is the SUM of only what was priced - it does not, and must not,
    /// silently absorb these as $0.00. A non-zero count here is the signal
    /// that this row's dollar figure understates what the bot actually
    /// cost.
    pub unpriced_count: i64,
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
                COALESCE(SUM(m.cached_tokens), 0),
                COALESCE(SUM(m.cost_unknown), 0)
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
            unpriced_count: row.get(7)?,
        })
    })?;

    let mut result = Vec::new();
    for row in rows {
        result.push(row?);
    }
    Ok(result)
}

// ---------------------------------------------------------------------
// S2-F-04: the account's real balance. F3/D6: `gate_run` was only ever
// called with `account_usage = None`, which its own `None` arm always
// allows - so the 402 branch above was dead and this ceiling could never
// actually fire. `CreditsPort` is what closes that: `AppState` holds a
// live `OpenRouterCredits` (`crates/server/src/lib.rs`), and
// `routes/messages.rs` reads it before every `gate_run` call now. Port of
// `createOpenRouterCredits` (`projects/bullpen-night/src/server/spend.ts:37-64`).
// ---------------------------------------------------------------------

/// Reads the account's all-time OpenRouter usage - what the ceiling gate
/// and `GET /api/spend`'s balance panel both need. `FakeCredits` below is
/// the test double; `OpenRouterCredits` is what production runs with.
#[async_trait::async_trait]
pub trait CreditsPort: Send + Sync {
    /// Dollars spent, all time, across everything using this key.
    async fn total_usage(&self) -> Result<f64, String>;
}

const CREDITS_URL: &str = "https://openrouter.ai/api/v1/credits";
/// Same TTL as `spend.ts`'s `CACHE_MS`.
const CREDITS_CACHE_TTL: Duration = Duration::from_secs(30);

struct CreditsCache {
    at: Instant,
    value: f64,
}

/// The live OpenRouter credits reader. The key is resolved fresh through
/// `KeySource` on every fetch (never cached itself, same posture
/// `model::OpenRouterCatalog` takes), and every error that could carry it
/// goes through [`model::redact`] before it leaves this type.
pub struct OpenRouterCredits {
    client: reqwest::Client,
    key_source: model::KeySource,
    cache: tokio::sync::Mutex<Option<CreditsCache>>,
}

impl OpenRouterCredits {
    pub fn new(key_source: model::KeySource) -> Self {
        Self {
            client: reqwest::Client::builder()
                .connect_timeout(Duration::from_secs(10))
                .timeout(Duration::from_secs(30))
                .build()
                .expect("client builder failed"),
            key_source,
            cache: tokio::sync::Mutex::new(None),
        }
    }

    async fn fetch(&self) -> Result<f64, String> {
        let key = self
            .key_source
            .resolve()
            .ok_or_else(|| "No OpenRouter key configured.".to_string())?;
        let res = self
            .client
            .get(CREDITS_URL)
            .bearer_auth(&key)
            .send()
            .await
            .map_err(|e| model::redact(&e.to_string(), Some(&key)))?;
        if !res.status().is_success() {
            let status = res.status().as_u16();
            return Err(model::redact(
                &format!("OpenRouter credits returned {status}"),
                Some(&key),
            ));
        }
        let body: RawCreditsResponse = res
            .json()
            .await
            .map_err(|e| model::redact(&e.to_string(), Some(&key)))?;
        Ok(body.data.and_then(|d| d.total_usage).unwrap_or(0.0))
    }
}

#[async_trait::async_trait]
impl CreditsPort for OpenRouterCredits {
    async fn total_usage(&self) -> Result<f64, String> {
        {
            let cache = self.cache.lock().await;
            if let Some(c) = cache.as_ref()
                && c.at.elapsed() < CREDITS_CACHE_TTL
            {
                return Ok(c.value);
            }
        }
        let value = self.fetch().await?;
        let mut cache = self.cache.lock().await;
        *cache = Some(CreditsCache {
            at: Instant::now(),
            value,
        });
        Ok(value)
    }
}

#[derive(Debug, Deserialize, Default)]
struct RawCreditsResponse {
    data: Option<RawCreditsData>,
}

#[derive(Debug, Deserialize, Default)]
struct RawCreditsData {
    total_usage: Option<f64>,
}

/// A `CreditsPort` that answers a fixed value (or error) every call. For
/// tests: no key, no network, no cache to reason about.
pub struct FakeCredits {
    result: std::sync::Mutex<Result<f64, String>>,
}

impl FakeCredits {
    /// Answers `total_usage` with this dollar figure every call.
    pub fn usage(value: f64) -> Self {
        Self {
            result: std::sync::Mutex::new(Ok(value)),
        }
    }

    /// Answers every call with this error - the credits-read-failure case
    /// `gate_run`/`get_spend` both have to handle (a read failure ALLOWS
    /// the run - see `gate_run`'s doc - but still says so).
    pub fn failing(message: impl Into<String>) -> Self {
        Self {
            result: std::sync::Mutex::new(Err(message.into())),
        }
    }
}

#[async_trait::async_trait]
impl CreditsPort for FakeCredits {
    async fn total_usage(&self) -> Result<f64, String> {
        self.result
            .lock()
            .expect("fake credits mutex poisoned")
            .clone()
    }
}
