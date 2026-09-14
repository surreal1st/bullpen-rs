//! S2-03: a run that must ask, waits. Port of the approval-row half of
//! `projects/bullpen-night/src/server/runs.ts` - the insert on `park`
//! (`:1660-1739`) and the lookup/decide on `decideApproval` (`:699-722`).
//! The tool loop that decides ask/allow/deny, and the resume that runs the
//! tool once Josh answers, both live in `crate::runs` - this module is only
//! the `approvals` table's own reads and writes, same split `permissions.rs`
//! draws between "what the map says" and "what a run does about it".

use rusqlite::OptionalExtension;
use serde::Serialize;
use shared::approval_groups::Groupable;
use store::Db;

/// One pending (or already-decided) approval row, joined with the bot's
/// name and the run's trigger - what `GET /api/approvals` shows Josh. LEFT
/// joined to `runs`: a run row is always there in practice (an approval is
/// only ever inserted alongside one), but a LEFT join costs nothing and
/// never turns a missing run into a missing approval.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Approval {
    pub id: String,
    pub run_id: String,
    pub bot_id: String,
    pub bot_name: String,
    pub tool_name: String,
    pub tool_args: String,
    pub created_at: String,
    pub trigger: Option<String>,
}

impl Groupable for Approval {
    fn id(&self) -> &str {
        &self.id
    }
    fn bot_id(&self) -> &str {
        &self.bot_id
    }
    fn tool_name(&self) -> &str {
        &self.tool_name
    }
    fn tool_args(&self) -> &str {
        &self.tool_args
    }
}

/// The columns `crate::runs`'s resume path needs off a pending row, once
/// it has decided who answered.
pub struct PendingCall {
    pub run_id: String,
    pub bot_id: String,
    pub tool_name: String,
    pub tool_args: String,
    pub call_id: String,
}

/// Inserts a pending approval for one gated tool call. Returns the new
/// row's id.
pub fn insert_pending(
    db: &Db,
    run_id: &str,
    bot_id: &str,
    tool_name: &str,
    tool_args: &str,
    call_id: &str,
) -> Result<String, rusqlite::Error> {
    let id = uuid::Uuid::new_v4().to_string();
    db.conn().execute(
        "INSERT INTO approvals (id, run_id, bot_id, tool_name, tool_args, call_id, status, created_at)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, 'pending', ?7)",
        rusqlite::params![id, run_id, bot_id, tool_name, tool_args, call_id, now_iso()],
    )?;
    Ok(id)
}

/// Every pending approval, newest first. Port of the TS `pending()`
/// (`runs.ts:829-843`), minus the multi-user `scope` filter - S2 has none.
pub fn list_pending(db: &Db) -> Result<Vec<Approval>, rusqlite::Error> {
    let mut stmt = db.conn().prepare(
        "SELECT a.id, a.run_id, a.bot_id, b.name, a.tool_name, a.tool_args, a.created_at, r.trigger
           FROM approvals a
           JOIN bots b ON b.id = a.bot_id
           LEFT JOIN runs r ON r.id = a.run_id
          WHERE a.status = 'pending'
          ORDER BY a.created_at DESC",
    )?;
    let rows = stmt.query_map([], |row| {
        Ok(Approval {
            id: row.get(0)?,
            run_id: row.get(1)?,
            bot_id: row.get(2)?,
            bot_name: row.get(3)?,
            tool_name: row.get(4)?,
            tool_args: row.get(5)?,
            created_at: row.get(6)?,
            trigger: row.get(7)?,
        })
    })?;
    rows.collect()
}

/// Run id -> the tool name it is parked on, for every pending approval.
/// What `RunManager::working` feeds `activity_line`'s `waiting_on` from -
/// port of the TS `working()`'s own `waitingOn` map (`runs.ts:916-919`).
pub fn waiting_on(db: &Db) -> Result<std::collections::HashMap<String, String>, rusqlite::Error> {
    let mut stmt = db
        .conn()
        .prepare("SELECT run_id, tool_name FROM approvals WHERE status = 'pending'")?;
    let rows = stmt.query_map([], |row| {
        Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
    })?;
    rows.collect()
}

/// Marks a pending approval decided (approved or rejected) and hands back
/// the row it decided, so the caller can resume the run it parked. `None`
/// when the id names no PENDING approval - already decided, or never
/// existed - which `routes/approvals.rs` turns into a 404, same as the TS
/// route answering nothing for a stale id.
///
/// The status flip happens here, unconditionally, before the caller does
/// anything else with the row - matching TS `decideApproval`'s own
/// ordering (`runs.ts:711-713`), so a decision is recorded even if resuming
/// the run it parked turns out to fail.
pub fn take_pending(
    db: &Db,
    approval_id: &str,
    approved: bool,
) -> Result<Option<PendingCall>, rusqlite::Error> {
    let row = db
        .conn()
        .query_row(
            "SELECT run_id, bot_id, tool_name, tool_args, call_id FROM approvals \
             WHERE id = ?1 AND status = 'pending'",
            rusqlite::params![approval_id],
            |row| {
                Ok(PendingCall {
                    run_id: row.get(0)?,
                    bot_id: row.get(1)?,
                    tool_name: row.get(2)?,
                    tool_args: row.get(3)?,
                    call_id: row.get(4)?,
                })
            },
        )
        .optional()?;
    let Some(row) = row else {
        return Ok(None);
    };

    db.conn().execute(
        "UPDATE approvals SET status = ?1, decided_at = ?2 WHERE id = ?3",
        rusqlite::params![
            if approved { "approved" } else { "rejected" },
            now_iso(),
            approval_id,
        ],
    )?;
    Ok(Some(row))
}

/// One pending approval old enough for the sweep to expire it, with the
/// columns `RunManager::sweep_approvals` (`crate::runs`) needs to fail the
/// run the same way `settle`'s failed branch does - F8.
pub struct ExpiredApproval {
    pub id: String,
    pub run_id: String,
    pub bot_id: String,
    /// `None` only if the run row itself is somehow already gone - the
    /// LEFT JOIN mirrors `list_pending`'s own, never turning a missing run
    /// into a missing approval.
    pub conversation_id: Option<String>,
}

/// Pending approvals created at or before `cutoff` (an ISO-8601 string,
/// same format `created_at` is stored in, so a lexical `<=` is a
/// chronological one) - what the 24h sweep expires. F8: nothing else ever
/// revisits a `waiting` run once `park` leaves it - a pending approval
/// Josh never answers held its row, and the run's `bus`/`backlog`/
/// `interjections` entries, forever.
pub fn list_pending_older_than(
    db: &Db,
    cutoff: &str,
) -> Result<Vec<ExpiredApproval>, rusqlite::Error> {
    let mut stmt = db.conn().prepare(
        "SELECT a.id, a.run_id, a.bot_id, r.conversation_id
           FROM approvals a
           LEFT JOIN runs r ON r.id = a.run_id
          WHERE a.status = 'pending' AND a.created_at <= ?1",
    )?;
    let rows = stmt.query_map(rusqlite::params![cutoff], |row| {
        Ok(ExpiredApproval {
            id: row.get(0)?,
            run_id: row.get(1)?,
            bot_id: row.get(2)?,
            conversation_id: row.get(3)?,
        })
    })?;
    rows.collect()
}

/// Marks one approval row `expired` - the sweep's terminal status for a
/// `pending` row nobody answered in time, distinct from `take_pending`'s
/// `approved`/`rejected` so Josh's history can tell "he said no" from "he
/// never got to it". `AND status = 'pending'` guards the same race
/// `take_pending` guards: a decision landing between the sweep's list and
/// this write means there is no longer anything here to expire, not an
/// error - the `bool` says which happened.
pub fn mark_expired(db: &Db, approval_id: &str) -> Result<bool, rusqlite::Error> {
    let changed = db.conn().execute(
        "UPDATE approvals SET status = 'expired', decided_at = ?1 WHERE id = ?2 AND status = 'pending'",
        rusqlite::params![now_iso(), approval_id],
    )?;
    Ok(changed > 0)
}

/// Deletes every approval whose `decided_at` is at or before `cutoff` -
/// F8's retention policy for `approved`/`rejected`/`expired` rows, which
/// otherwise sat in the table forever. A still-`pending` row has no
/// `decided_at` (`NULL <= x` is never true in SQL) and is never touched
/// here - only the sweep's own `mark_expired`, or Josh deciding it, can
/// make a row eligible.
pub fn prune_decided_older_than(db: &Db, cutoff: &str) -> Result<usize, rusqlite::Error> {
    db.conn().execute(
        "DELETE FROM approvals WHERE decided_at IS NOT NULL AND decided_at <= ?1",
        rusqlite::params![cutoff],
    )
}

/// Same format as JS `new Date().toISOString()` - matches `crate::runs`'s
/// own `now_iso`, kept as a second copy for the same reason that one is:
/// `store::conversations::now_iso` is `pub(crate)` to the store crate.
fn now_iso() -> String {
    chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Millis, true)
}
