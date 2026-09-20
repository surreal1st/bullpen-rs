//! W5: bot-written tools — proposals and live roster rows.
//! Port of `bot-tools.ts` `ensureBotToolTables` and the SQL helpers that
//! do not need the TypeScript jail.

use chrono::{DateTime, Utc};
use rusqlite::{OptionalExtension, params};
use serde::{Deserialize, Serialize};

use crate::Db;

pub fn ensure_bot_tool_tables(db: &Db) -> rusqlite::Result<()> {
    db.conn().execute_batch(
        "
        CREATE TABLE IF NOT EXISTS bot_tool_proposals (
          id          TEXT PRIMARY KEY,
          bot_id      TEXT NOT NULL,
          name        TEXT NOT NULL,
          description TEXT NOT NULL DEFAULT '',
          parameters  TEXT NOT NULL DEFAULT '{}',
          examples    TEXT NOT NULL DEFAULT '[]',
          source      TEXT NOT NULL DEFAULT '',
          path        TEXT NOT NULL DEFAULT '',
          ok          INTEGER NOT NULL DEFAULT 0,
          refusal     TEXT,
          diagnostics TEXT NOT NULL DEFAULT '[]',
          results     TEXT NOT NULL DEFAULT '[]',
          status      TEXT NOT NULL DEFAULT 'checked',
          created_at  TEXT NOT NULL,
          decided_at  TEXT
        );
        CREATE TABLE IF NOT EXISTS bot_tools (
          name        TEXT PRIMARY KEY,
          bot_id      TEXT NOT NULL,
          proposal_id TEXT NOT NULL DEFAULT '',
          description TEXT NOT NULL DEFAULT '',
          parameters  TEXT NOT NULL DEFAULT '{}',
          examples    TEXT NOT NULL DEFAULT '[]',
          source      TEXT NOT NULL DEFAULT '',
          hash        TEXT NOT NULL DEFAULT '',
          path        TEXT NOT NULL DEFAULT '',
          calls       INTEGER NOT NULL DEFAULT 0,
          approved_at TEXT NOT NULL,
          revoked_at  TEXT
        );
        ",
    )
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ToolExampleResult {
    pub args: serde_json::Value,
    pub expect: String,
    pub got: String,
    pub pass: bool,
    #[serde(default)]
    pub logs: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ToolProposal {
    pub id: String,
    pub bot_id: String,
    pub name: String,
    pub description: String,
    pub parameters: serde_json::Value,
    pub examples: Vec<serde_json::Value>,
    pub source: String,
    pub path: String,
    pub ok: bool,
    pub refusal: Option<String>,
    pub diagnostics: Vec<String>,
    pub results: Vec<ToolExampleResult>,
    pub status: String,
    pub created_at: String,
    pub decided_at: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct BotMadeToolRow {
    pub name: String,
    pub bot_id: String,
    pub bot_name: String,
    pub description: String,
    pub approved_at: String,
    pub calls: i64,
}

fn parse_json_array(text: &str) -> Vec<String> {
    serde_json::from_str(text).unwrap_or_default()
}

fn parse_json_value(text: &str) -> serde_json::Value {
    serde_json::from_str(text).unwrap_or(serde_json::json!({}))
}

fn row_to_proposal(row: &rusqlite::Row) -> rusqlite::Result<ToolProposal> {
    Ok(ToolProposal {
        id: row.get(0)?,
        bot_id: row.get(1)?,
        name: row.get(2)?,
        description: row.get(3)?,
        parameters: parse_json_value(&row.get::<_, String>(4)?),
        examples: serde_json::from_str(&row.get::<_, String>(5)?).unwrap_or_default(),
        source: row.get(6)?,
        path: row.get(7)?,
        ok: row.get::<_, i64>(8)? != 0,
        refusal: row.get(9)?,
        diagnostics: parse_json_array(&row.get::<_, String>(10)?),
        results: serde_json::from_str(&row.get::<_, String>(11)?).unwrap_or_default(),
        status: row.get(12)?,
        created_at: row.get(13)?,
        decided_at: row.get(14)?,
    })
}

pub fn proposal_by_id(db: &Db, id: &str) -> rusqlite::Result<Option<ToolProposal>> {
    db.conn()
        .query_row(
            "SELECT id, bot_id, name, description, parameters, examples, source, path, ok, refusal, diagnostics, results, status, created_at, decided_at FROM bot_tool_proposals WHERE id = ?1",
            params![id],
            row_to_proposal,
        )
        .optional()
}

pub fn latest_proposal(
    db: &Db,
    bot_id: &str,
    name: &str,
) -> rusqlite::Result<Option<ToolProposal>> {
    db.conn()
        .query_row(
            "SELECT id, bot_id, name, description, parameters, examples, source, path, ok, refusal, diagnostics, results, status, created_at, decided_at FROM bot_tool_proposals WHERE bot_id = ?1 AND name = ?2 ORDER BY created_at DESC LIMIT 1",
            params![bot_id, name],
            row_to_proposal,
        )
        .optional()
}

pub fn reject_proposal(db: &Db, id: &str, now: DateTime<Utc>) -> rusqlite::Result<()> {
    db.conn().execute(
        "UPDATE bot_tool_proposals SET status = 'rejected', decided_at = ?1 WHERE id = ?2",
        params![now.to_rfc3339(), id],
    )?;
    Ok(())
}

pub fn is_live_bot_tool(db: &Db, name: &str) -> rusqlite::Result<bool> {
    let found: Option<String> = db
        .conn()
        .query_row(
            "SELECT name FROM bot_tools WHERE name = ?1 AND revoked_at IS NULL",
            params![name],
            |row| row.get(0),
        )
        .optional()?;
    Ok(found.is_some())
}

pub struct LiveToolRow {
    pub name: String,
    pub bot_id: String,
    pub description: String,
    pub parameters: String,
    pub source: String,
    pub path: String,
}

pub fn live_tool_row(db: &Db, name: &str) -> rusqlite::Result<Option<LiveToolRow>> {
    db.conn()
        .query_row(
            "SELECT name, bot_id, description, parameters, source, path FROM bot_tools WHERE name = ?1 AND revoked_at IS NULL",
            params![name],
            |row| {
                Ok(LiveToolRow {
                    name: row.get(0)?,
                    bot_id: row.get(1)?,
                    description: row.get(2)?,
                    parameters: row.get(3)?,
                    source: row.get(4)?,
                    path: row.get(5)?,
                })
            },
        )
        .optional()
}

pub fn list_live_tool_rows(db: &Db) -> rusqlite::Result<Vec<LiveToolRow>> {
    let mut stmt = db.conn().prepare(
        "SELECT name, bot_id, description, parameters, source, path FROM bot_tools WHERE revoked_at IS NULL ORDER BY name",
    )?;
    let rows = stmt
        .query_map([], |row| {
            Ok(LiveToolRow {
                name: row.get(0)?,
                bot_id: row.get(1)?,
                description: row.get(2)?,
                parameters: row.get(3)?,
                source: row.get(4)?,
                path: row.get(5)?,
            })
        })?
        .collect::<Result<Vec<_>, _>>()?;
    Ok(rows)
}

pub fn increment_tool_calls(db: &Db, name: &str) -> rusqlite::Result<()> {
    db.conn().execute(
        "UPDATE bot_tools SET calls = calls + 1 WHERE name = ?1",
        params![name],
    )?;
    Ok(())
}

pub fn list_bot_tools(db: &Db) -> rusqlite::Result<Vec<BotMadeToolRow>> {
    let mut stmt = db.conn().prepare(
        "SELECT t.name, t.bot_id, COALESCE(b.name, t.bot_id) AS bot_name, t.description, t.approved_at, t.calls
         FROM bot_tools t
         LEFT JOIN bots b ON b.id = t.bot_id
         WHERE t.revoked_at IS NULL
         ORDER BY t.name",
    )?;
    let rows = stmt
        .query_map([], |row| {
            Ok(BotMadeToolRow {
                name: row.get(0)?,
                bot_id: row.get(1)?,
                bot_name: row.get(2)?,
                description: row.get(3)?,
                approved_at: row.get(4)?,
                calls: row.get(5)?,
            })
        })?
        .collect::<Result<Vec<_>, _>>()?;
    Ok(rows)
}

pub fn revoke_bot_tool(db: &Db, name: &str, now: DateTime<Utc>) -> rusqlite::Result<bool> {
    let changed = db.conn().execute(
        "UPDATE bot_tools SET revoked_at = ?1 WHERE name = ?2 AND revoked_at IS NULL",
        params![now.to_rfc3339(), name],
    )?;
    Ok(changed > 0)
}

pub fn insert_live_tool(
    db: &Db,
    proposal: &ToolProposal,
    path: &str,
    hash: &str,
    approved_at: DateTime<Utc>,
) -> rusqlite::Result<()> {
    db.conn().execute(
        "INSERT OR REPLACE INTO bot_tools
           (name, bot_id, proposal_id, description, parameters, examples, source, hash, path, calls, approved_at, revoked_at)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, 0, ?10, NULL)",
        params![
            proposal.name,
            proposal.bot_id,
            proposal.id,
            proposal.description,
            serde_json::to_string(&proposal.parameters).unwrap_or_else(|_| "{}".to_string()),
            serde_json::to_string(&proposal.examples).unwrap_or_else(|_| "[]".to_string()),
            proposal.source,
            hash,
            path,
            approved_at.to_rfc3339(),
        ],
    )?;
    db.conn().execute(
        "UPDATE bot_tool_proposals SET status = 'approved', decided_at = ?1 WHERE id = ?2",
        params![approved_at.to_rfc3339(), proposal.id],
    )?;
    Ok(())
}

pub fn upsert_proposal_row(db: &Db, proposal: &ToolProposal) -> rusqlite::Result<()> {
    db.conn().execute(
        "INSERT OR REPLACE INTO bot_tool_proposals
           (id, bot_id, name, description, parameters, examples, source, path, ok, refusal,
            diagnostics, results, status, created_at, decided_at)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, NULL)",
        params![
            proposal.id,
            proposal.bot_id,
            proposal.name,
            proposal.description,
            serde_json::to_string(&proposal.parameters).unwrap_or_else(|_| "{}".to_string()),
            serde_json::to_string(&proposal.examples).unwrap_or_else(|_| "[]".to_string()),
            proposal.source,
            proposal.path,
            if proposal.ok { 1 } else { 0 },
            proposal.refusal,
            serde_json::to_string(&proposal.diagnostics).unwrap_or_else(|_| "[]".to_string()),
            serde_json::to_string(&proposal.results).unwrap_or_else(|_| "[]".to_string()),
            proposal.status,
            proposal.created_at,
        ],
    )?;
    Ok(())
}
