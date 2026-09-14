//! S4-01: auto_review_log table and operations.

use crate::Db;
use serde::Serialize;

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct LogEntry {
    pub id: String,
    pub bot_id: String,
    pub run_id: String,
    pub tool_name: String,
    pub description: String,
    pub verdict: String,
    pub reason: String,
    pub decision: String,
    pub created_at: String,
}

/// Insert a judgement into the log, capped at 200 newest rows. Older rows
/// are deleted on every insert to maintain the cap.
pub fn insert(db: &Db, entry: LogEntry) -> rusqlite::Result<()> {
    let conn = db.conn();

    // Insert the new entry
    conn.execute(
        "INSERT INTO auto_review_log (id, bot_id, run_id, tool_name, description, verdict, reason, decision, created_at)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
        rusqlite::params![
            entry.id,
            entry.bot_id,
            entry.run_id,
            entry.tool_name,
            entry.description,
            entry.verdict,
            entry.reason,
            entry.decision,
            entry.created_at
        ],
    )?;

    // Delete rows beyond the 200-row cap, keeping only the newest 200
    conn.execute(
        "DELETE FROM auto_review_log WHERE id NOT IN (
            SELECT id FROM auto_review_log ORDER BY created_at DESC LIMIT 200
         )",
        [],
    )?;

    Ok(())
}

/// List the most recent judgements (up to limit), newest first.
pub fn list(db: &Db, limit: u32) -> rusqlite::Result<Vec<LogEntry>> {
    let mut stmt = db.conn().prepare(
        "SELECT id, bot_id, run_id, tool_name, description, verdict, reason, decision, created_at
         FROM auto_review_log
         ORDER BY created_at DESC
         LIMIT ?1",
    )?;

    let entries = stmt.query_map(rusqlite::params![limit as i32], |row| {
        Ok(LogEntry {
            id: row.get(0)?,
            bot_id: row.get(1)?,
            run_id: row.get(2)?,
            tool_name: row.get(3)?,
            description: row.get(4)?,
            verdict: row.get(5)?,
            reason: row.get(6)?,
            decision: row.get(7)?,
            created_at: row.get(8)?,
        })
    })?;

    let mut result = Vec::new();
    for entry in entries {
        result.push(entry?);
    }
    Ok(result)
}
