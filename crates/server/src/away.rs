//! W6: "while you were away." Port of `projects/bullpen-night/src/server/away.ts`.

use chrono::{DateTime, Utc};
use futures::StreamExt;
use model::{
    CHEAP_DEFAULT_MODEL, ModelEvent, ModelPort, ModelRequest, ModelUsage, utility_messages,
};
use rusqlite::OptionalExtension;
use serde::Serialize;
use std::sync::{Arc, Mutex};
use store::{Db, auth, first_line, questions};

pub const AWAY_GAP_HOURS: f64 = 4.0;

#[derive(Debug, Clone, Serialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct AwayBotRow {
    pub bot_id: String,
    pub bot_name: String,
    pub unread: i64,
    pub last_unread_line: String,
    pub questions: i64,
    pub approvals: i64,
    pub stopped_routines: Vec<String>,
}

#[derive(Debug, Clone, Serialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct AwayPayload {
    pub show: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub gap_hours: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub summary: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub bots: Option<Vec<AwayBotRow>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub generated_at: Option<String>,
}

struct AwayStateRow {
    epoch: Option<String>,
    summary: Option<String>,
    generated_at: Option<String>,
    dismissed_at: Option<String>,
    gap_hours: Option<f64>,
}

fn current_epoch(db: &Db) -> rusqlite::Result<String> {
    Ok(auth::last_login_at(db)?.unwrap_or_else(|| "never".to_string()))
}

fn last_active_at(db: &Db) -> rusqlite::Result<Option<DateTime<Utc>>> {
    let row: Option<String> = db
        .conn()
        .query_row(
            "SELECT MAX(last_seen_at) FROM bots
              WHERE archived_at IS NULL AND hidden_at IS NULL AND last_seen_at IS NOT NULL",
            [],
            |row| row.get(0),
        )
        .optional()?
        .flatten();
    if let Some(at) = row {
        return Ok(DateTime::parse_from_rfc3339(&at)
            .ok()
            .map(|d| d.with_timezone(&Utc)));
    }
    Ok(auth::last_login_at(db)?
        .and_then(|s| DateTime::parse_from_rfc3339(&s).ok())
        .map(|d| d.with_timezone(&Utc)))
}

fn read_state(db: &Db) -> rusqlite::Result<Option<AwayStateRow>> {
    db.conn()
        .query_row(
            "SELECT epoch, summary, generated_at, dismissed_at, gap_hours FROM away_state WHERE id = 1",
            [],
            |row| {
                Ok(AwayStateRow {
                    epoch: row.get(0)?,
                    summary: row.get(1)?,
                    generated_at: row.get(2)?,
                    dismissed_at: row.get(3)?,
                    gap_hours: row.get(4)?,
                })
            },
        )
        .optional()
}

fn write_state_row(db: &Db, row: AwayStateRow) -> rusqlite::Result<()> {
    db.conn().execute(
        "INSERT INTO away_state (id, epoch, summary, generated_at, dismissed_at, gap_hours)
         VALUES (1, ?1, ?2, ?3, ?4, ?5)
         ON CONFLICT(id) DO UPDATE SET
           epoch = excluded.epoch, summary = excluded.summary,
           generated_at = excluded.generated_at, dismissed_at = excluded.dismissed_at,
           gap_hours = excluded.gap_hours",
        rusqlite::params![
            row.epoch,
            row.summary,
            row.generated_at,
            row.dismissed_at,
            row.gap_hours,
        ],
    )?;
    Ok(())
}

/// Everything worth telling Josh about, per bot, straight off the tables.
pub fn gather_away_rows(db: &Db) -> rusqlite::Result<Vec<AwayBotRow>> {
    let mut stmt = db
        .conn()
        .prepare("SELECT id, name FROM bots WHERE archived_at IS NULL AND hidden_at IS NULL")?;
    let bots: Vec<(String, String)> = stmt
        .query_map([], |row| Ok((row.get(0)?, row.get(1)?)))?
        .collect::<Result<Vec<_>, _>>()?;

    let mut rows = Vec::new();
    for (bot_id, bot_name) in bots {
        let unread: i64 = db.conn().query_row(
            "SELECT COUNT(*) FROM messages m
               JOIN conversations c ON c.id = m.conversation_id
               JOIN bots b ON b.id = c.bot_id
              WHERE c.bot_id = ?1 AND c.kind != 'room' AND m.role = 'assistant'
                AND m.created_at > COALESCE(b.last_seen_at, '')",
            rusqlite::params![&bot_id],
            |row| row.get(0),
        )?;

        let last_unread_line: String = db
            .conn()
            .query_row(
                "SELECT m.content FROM messages m
                   JOIN conversations c ON c.id = m.conversation_id
                   JOIN bots b ON b.id = c.bot_id
                  WHERE c.bot_id = ?1 AND c.kind != 'room' AND m.role = 'assistant'
                    AND m.created_at > COALESCE(b.last_seen_at, '')
                  ORDER BY m.created_at DESC, m.seq DESC LIMIT 1",
                rusqlite::params![&bot_id],
                |row| row.get::<_, String>(0),
            )
            .optional()?
            .map(|c| first_line(&c))
            .unwrap_or_default();

        let questions = questions::list_open(db, &bot_id)?.len() as i64;

        let approvals: i64 = db.conn().query_row(
            "SELECT COUNT(*) FROM approvals WHERE bot_id = ?1 AND status = 'pending'",
            rusqlite::params![&bot_id],
            |row| row.get(0),
        )?;

        let mut stopped_stmt = db
            .conn()
            .prepare("SELECT name FROM routines WHERE bot_id = ?1 AND paused_reason IS NOT NULL")?;
        let stopped_routines: Vec<String> = stopped_stmt
            .query_map(rusqlite::params![&bot_id], |row| row.get(0))?
            .collect::<Result<Vec<_>, _>>()?;

        if unread == 0 && questions == 0 && approvals == 0 && stopped_routines.is_empty() {
            continue;
        }

        rows.push(AwayBotRow {
            bot_id,
            bot_name,
            unread,
            last_unread_line,
            questions,
            approvals,
            stopped_routines,
        });
    }
    Ok(rows)
}

fn record_utility_spend(db: &Db, kind: &str, cost_usd: f64) -> rusqlite::Result<()> {
    db.conn().execute(
        "INSERT INTO utility_calls (id, kind, cost_usd, created_at) VALUES (?1, ?2, ?3, ?4)",
        rusqlite::params![
            uuid::Uuid::new_v4().to_string(),
            kind,
            cost_usd,
            Utc::now().to_rfc3339(),
        ],
    )?;
    Ok(())
}

fn describe_row(row: &AwayBotRow) -> String {
    let mut parts = Vec::new();
    if row.unread > 0 {
        parts.push(format!(
            "{} unread repl{}, last: \"{}\"",
            row.unread,
            if row.unread == 1 { "y" } else { "ies" },
            row.last_unread_line
        ));
    }
    if row.questions > 0 {
        parts.push(format!(
            "{} unanswered question{}",
            row.questions,
            if row.questions == 1 { "" } else { "s" }
        ));
    }
    if row.approvals > 0 {
        parts.push(format!(
            "{} pending approval{}",
            row.approvals,
            if row.approvals == 1 { "" } else { "s" }
        ));
    }
    if !row.stopped_routines.is_empty() {
        parts.push(format!(
            "stopped routine(s): {}",
            row.stopped_routines.join(", ")
        ));
    }
    format!("{}: {}", row.bot_name, parts.join("; "))
}

fn fallback_summary(rows: &[AwayBotRow]) -> String {
    if rows.is_empty() {
        return "Nothing needs attention.".to_string();
    }
    let total_unread: i64 = rows.iter().map(|r| r.unread).sum();
    format!(
        "{} bot{} have something waiting, {} unread repl{} among them.",
        rows.len(),
        if rows.len() == 1 { "" } else { "s" },
        total_unread,
        if total_unread == 1 { "y" } else { "ies" }
    )
}

async fn summarize(
    db: Arc<Mutex<Db>>,
    port: &dyn ModelPort,
    model: &str,
    rows: &[AwayBotRow],
    gap_hours: f64,
) -> String {
    let instruction = format!(
        "Josh was away for about {gap_hours:.1} hours.\n\
         Write ONE short paragraph (2 to 4 sentences) summarising what happened, using ONLY the facts listed below.\n\
         Never invent anything that is not listed. No greeting, no sign-off, plain text only."
    );
    let facts = rows.iter().map(describe_row).collect::<Vec<_>>().join("\n");
    let request = ModelRequest {
        model: model.to_string(),
        messages: utility_messages(&instruction, &facts),
        ..Default::default()
    };

    let mut text = String::new();
    let mut usage: Option<ModelUsage> = None;
    let mut stream = port.stream(request);
    while let Some(event) = stream.next().await {
        match event {
            ModelEvent::Delta { text: delta } => text.push_str(&delta),
            ModelEvent::Done { usage: u, .. } | ModelEvent::ToolCalls { usage: u, .. } => {
                if u.is_some() {
                    usage = u;
                }
            }
            ModelEvent::Error { .. } => break,
        }
    }

    let cost = usage.map(|u| u.cost_usd).unwrap_or(0.0);
    if let Err(err) = record_utility_spend(&db.lock().expect("db mutex"), "away_summary", cost) {
        tracing::error!("away: failed to record utility spend: {err}");
    }

    let trimmed = text.trim();
    if trimmed.is_empty() {
        fallback_summary(rows)
    } else {
        trimmed.to_string()
    }
}

pub async fn compute_away(
    db: Arc<Mutex<Db>>,
    port: Arc<dyn ModelPort>,
    model: Option<&str>,
    now: DateTime<Utc>,
) -> rusqlite::Result<AwayPayload> {
    let model = model.unwrap_or(CHEAP_DEFAULT_MODEL);
    let epoch = {
        let guard = db.lock().expect("db mutex");
        current_epoch(&guard)?
    };
    let state = {
        let guard = db.lock().expect("db mutex");
        read_state(&guard)?
    };

    if let Some(state) = &state
        && state.epoch.as_deref() == Some(epoch.as_str())
        && state.dismissed_at.is_some()
    {
        return Ok(AwayPayload {
            show: false,
            gap_hours: None,
            summary: None,
            bots: None,
            generated_at: None,
        });
    }

    if let Some(state) = &state
        && state.epoch.as_deref() == Some(epoch.as_str())
        && state.summary.is_some()
    {
        let bots = {
            let guard = db.lock().expect("db mutex");
            gather_away_rows(&guard)?
        };
        return Ok(AwayPayload {
            show: true,
            gap_hours: Some(state.gap_hours.unwrap_or(0.0)),
            summary: state.summary.clone(),
            bots: Some(bots),
            generated_at: state.generated_at.clone(),
        });
    }

    let active = {
        let guard = db.lock().expect("db mutex");
        last_active_at(&guard)?
    };
    let Some(active) = active else {
        return Ok(AwayPayload {
            show: false,
            gap_hours: None,
            summary: None,
            bots: None,
            generated_at: None,
        });
    };

    let gap_hours = (now - active).num_milliseconds() as f64 / 3_600_000.0;
    if gap_hours < AWAY_GAP_HOURS {
        return Ok(AwayPayload {
            show: false,
            gap_hours: None,
            summary: None,
            bots: None,
            generated_at: None,
        });
    }

    let rows = {
        let guard = db.lock().expect("db mutex");
        gather_away_rows(&guard)?
    };
    let summary = summarize(Arc::clone(&db), port.as_ref(), model, &rows, gap_hours).await;
    let generated_at = now.to_rfc3339();
    let generated_at_clone = generated_at.clone();

    {
        let guard = db.lock().expect("db mutex");
        write_state_row(
            &guard,
            AwayStateRow {
                epoch: Some(epoch),
                summary: Some(summary.clone()),
                generated_at: Some(generated_at),
                dismissed_at: None,
                gap_hours: Some(gap_hours),
            },
        )?;
    }

    Ok(AwayPayload {
        show: true,
        gap_hours: Some(gap_hours),
        summary: Some(summary),
        bots: Some(rows),
        generated_at: Some(generated_at_clone),
    })
}

pub fn dismiss_away(db: &Db) -> rusqlite::Result<()> {
    let epoch = current_epoch(db)?;
    let prev = read_state(db)?;
    write_state_row(
        db,
        AwayStateRow {
            epoch: Some(epoch),
            summary: prev.as_ref().and_then(|p| p.summary.clone()),
            generated_at: prev.as_ref().and_then(|p| p.generated_at.clone()),
            dismissed_at: Some(Utc::now().to_rfc3339()),
            gap_hours: prev.and_then(|p| p.gap_hours),
        },
    )
}
