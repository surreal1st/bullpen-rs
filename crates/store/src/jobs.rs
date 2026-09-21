//! Background jobs — work that outlives the run that started it.
//!
//! Port of `projects/bullpen-night/src/server/jobs.ts`. The table is
//! self-creating (same reason as `questions` and `goals`): live Bullpen may
//! already carry the row shape from TS `openDb`, and numbered migrations must
//! stay byte-compatible with migrations 1..16.

use crate::Db;
use chrono::Utc;
use rusqlite::{OptionalExtension, params};
use uuid::Uuid;

pub const MAX_RUNNING_JOBS: i64 = 4;

/// The most output a job keeps. Beyond this the newest is kept, not the oldest.
pub const MAX_JOB_OUTPUT: usize = 20_000;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum JobKind {
    Shell,
    Agent,
}

impl JobKind {
    fn as_str(self) -> &'static str {
        match self {
            JobKind::Shell => "shell",
            JobKind::Agent => "agent",
        }
    }

    fn from_db(raw: &str) -> Self {
        if raw == "agent" {
            JobKind::Agent
        } else {
            JobKind::Shell
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum JobStatus {
    Running,
    Done,
    Failed,
    Stopped,
}

impl JobStatus {
    pub fn as_str(self) -> &'static str {
        match self {
            JobStatus::Running => "running",
            JobStatus::Done => "done",
            JobStatus::Failed => "failed",
            JobStatus::Stopped => "stopped",
        }
    }

    fn from_db(raw: &str) -> Self {
        match raw {
            "done" => JobStatus::Done,
            "failed" => JobStatus::Failed,
            "stopped" => JobStatus::Stopped,
            _ => JobStatus::Failed,
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct Job {
    pub id: String,
    pub bot_id: String,
    pub kind: JobKind,
    pub label: String,
    pub spec: String,
    pub status: JobStatus,
    pub output: String,
    pub exit_code: Option<i32>,
    pub started_at: String,
    pub ended_at: Option<String>,
    pub cost_usd: Option<f64>,
    pub notify_when: Option<String>,
    pub notified: i32,
}

struct JobRow {
    id: String,
    bot_id: String,
    kind: String,
    label: String,
    spec: String,
    status: String,
    output: String,
    exit_code: Option<i32>,
    started_at: String,
    ended_at: Option<String>,
    cost_usd: Option<f64>,
    notify_when: Option<String>,
    notified: i32,
}

fn row_to_job(row: JobRow) -> Job {
    Job {
        id: row.id,
        bot_id: row.bot_id,
        kind: JobKind::from_db(&row.kind),
        label: row.label,
        spec: row.spec,
        status: if row.status == "running" {
            JobStatus::Running
        } else {
            JobStatus::from_db(&row.status)
        },
        output: row.output,
        exit_code: row.exit_code,
        started_at: row.started_at,
        ended_at: row.ended_at,
        cost_usd: row.cost_usd,
        notify_when: row.notify_when,
        notified: row.notified,
    }
}

fn column_exists(db: &Db, table: &str, column: &str) -> rusqlite::Result<bool> {
    let mut stmt = db.conn().prepare(&format!("PRAGMA table_info({table})"))?;
    let names = stmt.query_map([], |row| row.get::<_, String>(1))?;
    for name in names {
        if name? == column {
            return Ok(true);
        }
    }
    Ok(false)
}

/// Self-creating jobs table and columns added after first ship.
pub fn ensure_job_tables(db: &Db) -> rusqlite::Result<()> {
    db.conn().execute_batch(
        "CREATE TABLE IF NOT EXISTS jobs (
            id         TEXT PRIMARY KEY,
            bot_id     TEXT NOT NULL,
            kind       TEXT NOT NULL,
            label      TEXT NOT NULL DEFAULT '',
            spec       TEXT NOT NULL DEFAULT '',
            status     TEXT NOT NULL DEFAULT 'running',
            output     TEXT NOT NULL DEFAULT '',
            exit_code  INTEGER,
            started_at TEXT NOT NULL,
            ended_at   TEXT
        );
        CREATE INDEX IF NOT EXISTS idx_jobs_bot ON jobs(bot_id, started_at DESC);",
    )?;

    if !column_exists(db, "jobs", "cost_usd")? {
        db.conn()
            .execute_batch("ALTER TABLE jobs ADD COLUMN cost_usd REAL")?;
    }
    if !column_exists(db, "jobs", "notify_when")? {
        db.conn()
            .execute_batch("ALTER TABLE jobs ADD COLUMN notify_when TEXT")?;
    }
    if !column_exists(db, "jobs", "notified")? {
        db.conn()
            .execute_batch("ALTER TABLE jobs ADD COLUMN notified INTEGER NOT NULL DEFAULT 0")?;
    }

    Ok(())
}

pub fn count_running(db: &Db, bot_id: &str) -> rusqlite::Result<i64> {
    db.conn().query_row(
        "SELECT COUNT(*) FROM jobs WHERE bot_id = ?1 AND status = 'running'",
        params![bot_id],
        |row| row.get(0),
    )
}

pub fn create_job(
    db: &Db,
    bot_id: &str,
    kind: JobKind,
    label: &str,
    spec: &str,
    notify_when: Option<&str>,
) -> Result<Job, String> {
    let running = count_running(db, bot_id).map_err(|e| e.to_string())?;
    if running >= MAX_RUNNING_JOBS {
        return Err(format!(
            "You already have {MAX_RUNNING_JOBS} jobs running, which is the limit. Read or stop one before starting another."
        ));
    }

    let id = Uuid::new_v4().to_string();
    let label_kept = if label.len() > 200 {
        &label[..200]
    } else {
        label
    };
    let spec_kept = if spec.len() > 4000 {
        &spec[..4000]
    } else {
        spec
    };
    let started_at = Utc::now().to_rfc3339();

    db.conn()
        .execute(
            "INSERT INTO jobs (id, bot_id, kind, label, spec, status, started_at, notify_when)
             VALUES (?1, ?2, ?3, ?4, ?5, 'running', ?6, ?7)",
            params![
                id,
                bot_id,
                kind.as_str(),
                label_kept,
                spec_kept,
                started_at,
                notify_when
            ],
        )
        .map_err(|e| e.to_string())?;

    get_job(db, bot_id, &id).ok_or_else(|| "job insert succeeded but row missing".to_string())
}

pub fn get_job(db: &Db, bot_id: &str, job_id: &str) -> Option<Job> {
    db.conn()
        .query_row(
            "SELECT id, bot_id, kind, label, spec, status, output, exit_code,
                    started_at, ended_at, cost_usd, notify_when, notified
             FROM jobs WHERE id = ?1 AND bot_id = ?2",
            params![job_id, bot_id],
            |row| {
                Ok(row_to_job(JobRow {
                    id: row.get(0)?,
                    bot_id: row.get(1)?,
                    kind: row.get(2)?,
                    label: row.get(3)?,
                    spec: row.get(4)?,
                    status: row.get(5)?,
                    output: row.get(6)?,
                    exit_code: row.get(7)?,
                    started_at: row.get(8)?,
                    ended_at: row.get(9)?,
                    cost_usd: row.get(10)?,
                    notify_when: row.get(11)?,
                    notified: row.get(12)?,
                }))
            },
        )
        .optional()
        .ok()
        .flatten()
}

pub fn list_jobs(db: &Db, bot_id: &str) -> Vec<Job> {
    let mut stmt = match db.conn().prepare(
        "SELECT id, bot_id, kind, label, spec, status, output, exit_code,
                started_at, ended_at, cost_usd, notify_when, notified
         FROM jobs WHERE bot_id = ?1 ORDER BY started_at DESC LIMIT 25",
    ) {
        Ok(s) => s,
        Err(_) => return Vec::new(),
    };

    let rows = stmt.query_map(params![bot_id], |row| {
        Ok(row_to_job(JobRow {
            id: row.get(0)?,
            bot_id: row.get(1)?,
            kind: row.get(2)?,
            label: row.get(3)?,
            spec: row.get(4)?,
            status: row.get(5)?,
            output: row.get(6)?,
            exit_code: row.get(7)?,
            started_at: row.get(8)?,
            ended_at: row.get(9)?,
            cost_usd: row.get(10)?,
            notify_when: row.get(11)?,
            notified: row.get(12)?,
        }))
    });

    match rows {
        Ok(mapped) => mapped.filter_map(Result::ok).collect(),
        Err(_) => Vec::new(),
    }
}

fn keep_tail(output: &str) -> String {
    if output.len() > MAX_JOB_OUTPUT {
        output[output.len() - MAX_JOB_OUTPUT..].to_string()
    } else {
        output.to_string()
    }
}

pub fn finish_job(
    db: &Db,
    job_id: &str,
    status: JobStatus,
    output: &str,
    exit_code: Option<i32>,
    cost_usd: Option<f64>,
) -> rusqlite::Result<()> {
    let kept = keep_tail(output);
    let ended_at = Utc::now().to_rfc3339();
    db.conn().execute(
        "UPDATE jobs SET status = ?1, output = ?2, exit_code = ?3, cost_usd = ?4, ended_at = ?5
         WHERE id = ?6",
        params![status.as_str(), kept, exit_code, cost_usd, ended_at, job_id],
    )?;
    Ok(())
}

pub fn mark_job_notified(db: &Db, job_id: &str) -> rusqlite::Result<()> {
    db.conn().execute(
        "UPDATE jobs SET notified = 1 WHERE id = ?1",
        params![job_id],
    )?;
    Ok(())
}

pub fn append_job_output(db: &Db, job_id: &str, chunk: &str) -> rusqlite::Result<()> {
    let current: Option<String> = db
        .conn()
        .query_row(
            "SELECT output FROM jobs WHERE id = ?1",
            params![job_id],
            |row| row.get(0),
        )
        .optional()?;

    let Some(current) = current else {
        return Ok(());
    };

    let combined = format!("{current}{chunk}");
    let kept = keep_tail(&combined);
    db.conn().execute(
        "UPDATE jobs SET output = ?1 WHERE id = ?2",
        params![kept, job_id],
    )?;
    Ok(())
}

const REAP_MESSAGE: &str = "The server restarted while this was running, so its result was lost. Start it again if you still need it.";

/// Jobs left `running` by a restart. Returns rows updated.
pub fn reap_orphaned_jobs(db: &Db) -> rusqlite::Result<usize> {
    let ended_at = Utc::now().to_rfc3339();
    let changed = db.conn().execute(
        "UPDATE jobs
            SET status = 'failed',
                output = CASE
                    WHEN output = '' THEN ?1
                    ELSE output || char(10) || ?1
                END,
                ended_at = ?2
          WHERE status = 'running'",
        params![REAP_MESSAGE, ended_at],
    )?;
    Ok(changed)
}

/// What the model is told about one job.
pub fn describe_job(job: &Job) -> String {
    let head = format!("[{}] {} (id {})", job.status.as_str(), job.label, job.id);
    if job.status == JobStatus::Running {
        return format!(
            "{head}\nStill going. Ask again later - do not wait here and do not invent its result."
        );
    }
    let code = job
        .exit_code
        .map(|c| format!(" exit {c}"))
        .unwrap_or_default();
    let cost = job
        .cost_usd
        .map(|c| format!(" cost ${c:.4}"))
        .unwrap_or_default();
    let body = if job.output.is_empty() {
        "(no output)".to_string()
    } else {
        job.output.clone()
    };
    format!("{head}{code}{cost}\n{body}")
}
