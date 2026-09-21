//! Per-bot sandbox routing — meridian vs worker vs missing worker.
//!
//! Port of TS `botTools`'s one-time `sandbox` reassignment (`app.ts` ~4793).

use std::sync::Arc;

use store::Db;

use crate::job_runner::{JobSandbox, StoreJobEvents, UnavailableJobSandbox};
use crate::sandbox::{Sandbox, UnavailableSandbox};
use crate::workers::{
    DockerRun, SandboxSource, WORKER_WAKE_TIMEOUT_MS, Worker, WorkerSandbox, get_bot_worker_id,
    get_worker, resolve_sandbox_source,
};

const MAX_JOB_MS: u64 = 2 * 60 * 60 * 1000;

pub struct ResolvedSandboxes {
    pub exec: Arc<dyn Sandbox>,
    pub job: Arc<dyn JobSandbox>,
    pub job_max_ms: u64,
}

struct WorkerExecAdapter(Arc<WorkerSandbox>);

#[async_trait::async_trait]
impl Sandbox for WorkerExecAdapter {
    async fn exec(&self, bot_id: &str, command: &str) -> crate::sandbox::ExecResult {
        let r = self.0.exec(bot_id, command, None).await;
        crate::sandbox::ExecResult {
            stdout: r.stdout,
            stderr: r.stderr,
            exit_code: r.exit_code,
            timed_out: r.timed_out,
            truncated: r.truncated,
            unavailable: false,
        }
    }

    async fn read_file(&self, bot_id: &str, path: &str) -> Result<String, String> {
        crate::sandbox::read_file_via_exec(self, bot_id, path).await
    }
}

#[async_trait::async_trait]
impl JobSandbox for WorkerExecAdapter {
    fn supports_background(&self) -> bool {
        true
    }

    async fn exec_with_timeout(
        &self,
        bot_id: &str,
        command: &str,
        timeout_ms: u64,
    ) -> crate::sandbox::ExecResult {
        let r = self.0.exec(bot_id, command, Some(timeout_ms)).await;
        crate::sandbox::ExecResult {
            stdout: r.stdout,
            stderr: r.stderr,
            exit_code: r.exit_code,
            timed_out: r.timed_out,
            truncated: r.truncated,
            unavailable: false,
        }
    }

    async fn spawn(
        &self,
        bot_id: &str,
        job_id: &str,
        command: &str,
    ) -> crate::sandbox::SpawnResult {
        let r = self.0.spawn(bot_id, job_id, command).await;
        crate::sandbox::SpawnResult {
            ok: r.ok,
            handle: r.handle,
            detail: r.detail,
        }
    }

    async fn probe(&self, handle: &str) -> crate::sandbox::ProbeResult {
        let r = self.0.probe(handle).await;
        crate::sandbox::ProbeResult {
            running: r.running,
            exit_code: r.exit_code.map(|c| c as i32),
            detail: r.detail,
        }
    }

    async fn kill(&self, handle: &str) -> bool {
        self.0.kill(handle).await
    }
}

/// Until cert-backed `DockerRun` is wired, worker-routed bots get a sandbox
/// that fails loudly rather than silently hitting meridian.
struct PlaceholderWorkerDocker {
    label: String,
}

#[async_trait::async_trait]
impl DockerRun for PlaceholderWorkerDocker {
    async fn run(&self, _args: Vec<String>, _timeout_ms: u64) -> crate::workers::DockerResult {
        crate::workers::DockerResult {
            ok: false,
            stdout: String::new(),
            stderr: format!(
                "{}: worker Docker is not configured on this server yet (TLS/SSH wiring is pending).",
                self.label
            ),
        }
    }
}

fn docker_for_worker(worker: &Worker) -> Arc<dyn DockerRun> {
    Arc::new(PlaceholderWorkerDocker {
        label: worker.label.clone(),
    })
}

pub fn resolve_bot_sandboxes(
    db: &Db,
    bot_id: &str,
    meridian_exec: Arc<dyn Sandbox>,
    meridian_job: Arc<dyn JobSandbox>,
    job_events: Arc<dyn crate::workers::JobEvents>,
) -> ResolvedSandboxes {
    let worker_id = get_bot_worker_id(db, bot_id).ok().flatten();
    let full_worker = worker_id.as_deref().and_then(|id| get_worker(db, id));
    match resolve_sandbox_source(worker_id.as_deref(), full_worker.is_some()) {
        SandboxSource::Meridian => ResolvedSandboxes {
            exec: meridian_exec,
            job: meridian_job,
            job_max_ms: MAX_JOB_MS,
        },
        SandboxSource::Worker => {
            let worker = full_worker.expect("worker_exists when source is Worker");
            let ws = Arc::new(WorkerSandbox::new(
                worker.id.clone(),
                worker.label.clone(),
                docker_for_worker(&worker),
                job_events,
            ));
            let adapter: Arc<dyn JobSandbox> = Arc::new(WorkerExecAdapter(Arc::clone(&ws)));
            ResolvedSandboxes {
                exec: Arc::new(WorkerExecAdapter(ws)),
                job: adapter,
                job_max_ms: WORKER_WAKE_TIMEOUT_MS,
            }
        }
        SandboxSource::Missing { worker_id } => {
            let msg = format!(
                "This bot is assigned to worker \"{worker_id}\", but that worker no longer exists. \
Clear the worker assignment in Settings or pick a worker that is still registered."
            );
            ResolvedSandboxes {
                exec: Arc::new(UnavailableSandbox::new(msg.clone())),
                job: Arc::new(UnavailableJobSandbox),
                job_max_ms: MAX_JOB_MS,
            }
        }
    }
}

/// Default job events for production routing (store-backed).
pub fn store_job_events(db: Arc<std::sync::Mutex<Db>>) -> Arc<StoreJobEvents> {
    Arc::new(StoreJobEvents::new(db))
}
