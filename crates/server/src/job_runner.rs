//! Driving background shell jobs to completion.
//!
//! Port of `projects/bullpen-night/src/server/job-runner.ts` (shell path only).
//! The polling loop runs on the server, not inside a run.

use std::sync::{Arc, Mutex};

use regex::Regex;
use store::{
    Db, JobKind, JobStatus, NewMessage, append_job_output, create_job, finish_job, get_job,
    mark_job_notified,
};

use crate::sandbox::{ExecResult, ProbeResult, SpawnResult, container_for_job, job_log_path};
use crate::workers::JobEvents;

/// Sandbox cannot background — same message shape as TS when `spawn` is absent.
pub struct UnavailableJobSandbox;

#[async_trait::async_trait]
impl JobSandbox for UnavailableJobSandbox {
    fn supports_background(&self) -> bool {
        false
    }

    async fn exec_with_timeout(
        &self,
        _bot_id: &str,
        _command: &str,
        _timeout_ms: u64,
    ) -> ExecResult {
        unreachable!()
    }

    async fn spawn(&self, _bot_id: &str, _job_id: &str, _command: &str) -> SpawnResult {
        unreachable!()
    }

    async fn probe(&self, _handle: &str) -> ProbeResult {
        unreachable!()
    }

    async fn kill(&self, _handle: &str) -> bool {
        unreachable!()
    }
}

pub const NOTIFY_PATTERN_MAX_CHARS: usize = 200;

const POLL_MS: u64 = 3_000;
const MAX_JOB_MS: u64 = 2 * 60 * 60 * 1000;
const READ_LOG_TIMEOUT_MS: u64 = 15_000;

/// Sandbox surface the job runner needs — meridian `DockerSandbox` and test fakes.
#[async_trait::async_trait]
pub trait JobSandbox: Send + Sync {
    fn supports_background(&self) -> bool;
    async fn exec_with_timeout(&self, bot_id: &str, command: &str, timeout_ms: u64) -> ExecResult;
    async fn spawn(&self, bot_id: &str, job_id: &str, command: &str) -> SpawnResult;
    async fn probe(&self, handle: &str) -> ProbeResult;
    async fn kill(&self, handle: &str) -> bool;
}

#[async_trait::async_trait]
impl JobSandbox for crate::sandbox::DockerSandbox {
    fn supports_background(&self) -> bool {
        crate::sandbox::DockerSandbox::supports_background(self)
    }

    async fn exec_with_timeout(&self, bot_id: &str, command: &str, timeout_ms: u64) -> ExecResult {
        crate::sandbox::DockerSandbox::exec_with_timeout(self, bot_id, command, timeout_ms).await
    }

    async fn spawn(&self, bot_id: &str, job_id: &str, command: &str) -> SpawnResult {
        crate::sandbox::DockerSandbox::spawn(self, bot_id, job_id, command).await
    }

    async fn probe(&self, handle: &str) -> ProbeResult {
        crate::sandbox::DockerSandbox::probe(self, handle).await
    }

    async fn kill(&self, handle: &str) -> bool {
        crate::sandbox::DockerSandbox::kill(self, handle).await
    }
}

pub struct JobRunnerDeps {
    pub db: Arc<Mutex<Db>>,
    pub sandbox: Arc<dyn JobSandbox>,
    pub poll_ms: u64,
    pub max_ms: u64,
}

impl JobRunnerDeps {
    pub fn new(db: Arc<Mutex<Db>>, sandbox: Arc<dyn JobSandbox>) -> Self {
        Self {
            db,
            sandbox,
            poll_ms: POLL_MS,
            max_ms: MAX_JOB_MS,
        }
    }
}

/// `JobEvents` backed by the real jobs table (for `WorkerSandbox` and production).
pub struct StoreJobEvents {
    db: Arc<Mutex<Db>>,
}

impl StoreJobEvents {
    pub fn new(db: Arc<Mutex<Db>>) -> Self {
        Self { db }
    }
}

impl JobEvents for StoreJobEvents {
    fn append_output(&self, job_id: &str, text: &str) {
        if let Ok(db) = self.db.lock() {
            let _ = append_job_output(&db, job_id, text);
        }
    }

    fn finish_failed(&self, job_id: &str, message: &str) {
        if let Ok(db) = self.db.lock() {
            let _ = finish_job(&db, job_id, JobStatus::Failed, message, None, None);
        }
    }
}

pub async fn start_shell_job(
    deps: &JobRunnerDeps,
    bot_id: &str,
    label: &str,
    command: &str,
    notify_when: Option<&str>,
) -> String {
    if !deps.sandbox.supports_background() {
        return "Background commands are not available here: this server has no sandbox that can run one. Use `shell` for something short instead.".to_string();
    }

    if let Some(pattern) = notify_when {
        if pattern.len() > NOTIFY_PATTERN_MAX_CHARS {
            return format!(
                "That notify pattern is too long; keep it under {NOTIFY_PATTERN_MAX_CHARS} characters."
            );
        }
        if Regex::new(&format!("(?i){pattern}")).is_err() {
            return "That notify pattern is not a valid regular expression.".to_string();
        }
    }

    let label_used = if label.is_empty() {
        command.chars().take(60).collect::<String>()
    } else {
        label.to_string()
    };

    let job = {
        let db = deps.db.lock().expect("db lock");
        match create_job(
            &db,
            bot_id,
            JobKind::Shell,
            &label_used,
            command,
            notify_when,
        ) {
            Ok(job) => job,
            Err(msg) => return msg,
        }
    };

    let started = deps.sandbox.spawn(bot_id, &job.id, command).await;
    if !started.ok {
        let msg = format!("It did not start: {}", started.detail);
        if let Ok(db) = deps.db.lock() {
            let _ = finish_job(&db, &job.id, JobStatus::Failed, &msg, None, None);
        }
        return format!("That did not start: {}", started.detail);
    }

    let watch_deps = JobRunnerDeps {
        db: Arc::clone(&deps.db),
        sandbox: Arc::clone(&deps.sandbox),
        poll_ms: deps.poll_ms,
        max_ms: deps.max_ms,
    };
    let bot = bot_id.to_string();
    let job_id = job.id.clone();
    tokio::spawn(async move {
        watch_shell_job(&watch_deps, &bot, &job_id).await;
    });

    format!(
        "Started in the background. Job id {}. Do NOT wait for it here and do not poll it in a loop - finish what else you can do, and read it with `job_status` on a later turn or in your next run.",
        job.id
    )
}

async fn watch_shell_job(deps: &JobRunnerDeps, bot_id: &str, job_id: &str) {
    let handle = container_for_job(job_id);
    let deadline = std::time::Instant::now() + std::time::Duration::from_millis(deps.max_ms);

    loop {
        tokio::time::sleep(std::time::Duration::from_millis(deps.poll_ms)).await;

        let still_running = {
            let db = deps.db.lock().expect("db lock");
            get_job(&db, bot_id, job_id).is_some_and(|j| j.status == JobStatus::Running)
        };
        if !still_running {
            return;
        }

        if std::time::Instant::now() > deadline {
            deps.sandbox.kill(&handle).await;
            let output = read_log(
                deps,
                bot_id,
                job_id,
                "It ran past the two-hour limit and was stopped.",
            )
            .await;
            if let Ok(db) = deps.db.lock() {
                let _ = finish_job(&db, job_id, JobStatus::Failed, &output, Some(124), None);
            }
            return;
        }

        let probe = deps.sandbox.probe(&handle).await;
        if probe.running {
            maybe_notify(deps, bot_id, job_id).await;
            continue;
        }

        if probe.exit_code.is_none() {
            let output = read_log(
                deps,
                bot_id,
                job_id,
                &format!(
                    "Its container disappeared before the result could be read ({}). Treat the result as unknown, not as success.",
                    probe.detail
                ),
            )
            .await;
            if let Ok(db) = deps.db.lock() {
                let _ = finish_job(&db, job_id, JobStatus::Failed, &output, None, None);
            }
            deps.sandbox.kill(&handle).await;
            return;
        }

        let output = read_log(deps, bot_id, job_id, "").await;
        let status = if probe.exit_code == Some(0) {
            JobStatus::Done
        } else {
            JobStatus::Failed
        };
        if let Ok(db) = deps.db.lock() {
            let _ = finish_job(&db, job_id, status, &output, probe.exit_code, None);
        }
        deps.sandbox.kill(&handle).await;
        return;
    }
}

async fn maybe_notify(deps: &JobRunnerDeps, bot_id: &str, job_id: &str) {
    let (pattern, label, notified) = {
        let db = deps.db.lock().expect("db lock");
        let Some(job) = get_job(&db, bot_id, job_id) else {
            return;
        };
        (job.notify_when.clone(), job.label.clone(), job.notified)
    };
    let Some(pattern) = pattern else {
        return;
    };
    if notified != 0 {
        return;
    }
    let Ok(re) = Regex::new(&format!("(?i){pattern}")) else {
        return;
    };
    let current = read_log(deps, bot_id, job_id, "").await;
    for line in current.lines() {
        if re.is_match(line) {
            let matched = line.chars().take(200).collect::<String>();
            say_to_josh(
                deps,
                bot_id,
                &format!("Background job \"{label}\" matched /{pattern}/: {matched}"),
            );
            if let Ok(db) = deps.db.lock() {
                let _ = mark_job_notified(&db, job_id);
            }
            break;
        }
    }
}

fn say_to_josh(deps: &JobRunnerDeps, bot_id: &str, text: &str) {
    let Ok(db) = deps.db.lock() else {
        return;
    };
    let Ok(conversation_id) = store::get_or_create_conversation(&db, bot_id) else {
        return;
    };
    let _ = store::append_message(
        &db,
        &conversation_id,
        "assistant",
        text,
        NewMessage {
            bot_id: Some(bot_id.to_string()),
            ..Default::default()
        },
    );
}

pub async fn await_job(
    deps: &JobRunnerDeps,
    bot_id: &str,
    job_id: &str,
    seconds: Option<u32>,
) -> String {
    let max_seconds = seconds.unwrap_or(30).clamp(1, 60);
    let deadline =
        std::time::Instant::now() + std::time::Duration::from_secs(u64::from(max_seconds));
    let poll_ms = deps.poll_ms.min(500);

    loop {
        if std::time::Instant::now() > deadline {
            return format!(
                "Still running after {max_seconds} seconds; check again later with job_status."
            );
        }

        let current = {
            let db = deps.db.lock().expect("db lock");
            store::get_job(&db, bot_id, job_id)
        };
        let Some(current) = current else {
            return "No job of yours has that id.".to_string();
        };
        if current.status != JobStatus::Running {
            return format!(
                "[{}] {}\n{}",
                current.status.as_str(),
                current.label,
                if current.output.is_empty() {
                    "(no output)".to_string()
                } else {
                    current.output.clone()
                }
            );
        }
        tokio::time::sleep(std::time::Duration::from_millis(poll_ms)).await;
    }
}

pub async fn stop_job(deps: &JobRunnerDeps, bot_id: &str, job_id: &str) -> String {
    let job = {
        let db = deps.db.lock().expect("db lock");
        store::get_job(&db, bot_id, job_id)
    };
    let Some(job) = job else {
        return "No job of yours has that id.".to_string();
    };
    if job.status != JobStatus::Running {
        return format!("That job is already {}.", job.status.as_str());
    }

    if job.kind == JobKind::Shell {
        deps.sandbox.kill(&container_for_job(&job.id)).await;
    }

    let output = if job.kind == JobKind::Agent {
        "Stopped. The colleague may still finish its turn, but its answer will be discarded."
            .to_string()
    } else {
        "Stopped before it finished.".to_string()
    };

    if let Ok(db) = deps.db.lock() {
        let _ = finish_job(&db, &job.id, JobStatus::Stopped, &output, None, None);
    }

    format!("Stopped {}.", job.label)
}

pub fn job_runner_deps(db: Arc<Mutex<Db>>, sandbox: Arc<dyn JobSandbox>) -> JobRunnerDeps {
    JobRunnerDeps::new(db, sandbox)
}

async fn read_log(deps: &JobRunnerDeps, bot_id: &str, job_id: &str, prefix: &str) -> String {
    let path = job_log_path(job_id);
    let command = format!("cat {path} 2>/dev/null || true");
    let result = deps
        .sandbox
        .exec_with_timeout(bot_id, &command, READ_LOG_TIMEOUT_MS)
        .await;
    let body = format!("{}{}", result.stdout, result.stderr)
        .trim()
        .to_string();
    if prefix.is_empty() {
        body
    } else if body.is_empty() {
        prefix.to_string()
    } else {
        format!("{prefix}\n\n{body}")
    }
}
