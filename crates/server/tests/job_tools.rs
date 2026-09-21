//! S9-03: model-facing job tools dispatch through `RunManager::toolbox_for`.

mod common;

use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use common::{as_port, seed_bot};
use model::ladder::Trigger;
use model::{ModelEvent, ModelPort, ModelRequest};
use server::job_runner::{JobSandbox, UnavailableJobSandbox};
use server::runs::RunManager;
use server::sandbox::{ExecResult, ProbeResult, Sandbox, SpawnResult, UnavailableSandbox};
use store::Db;
use store::bots::{BotDraft, create_bot};
use store::jobs::get_job;

struct SilentPort;

impl ModelPort for SilentPort {
    fn stream(&self, _request: ModelRequest) -> model::EventStream {
        Box::pin(futures::stream::iter(vec![ModelEvent::Done {
            model: "test/model".to_string(),
            usage: None,
            finish_reason: None,
        }]))
    }
}

struct FakeJobSandbox {
    probes: AtomicU32,
    ticks_to_done: u32,
    exit_code: Option<i32>,
    log: String,
    killed: Mutex<Vec<String>>,
}

impl FakeJobSandbox {
    fn new(ticks_to_done: u32, exit_code: Option<i32>, log: impl Into<String>) -> Self {
        Self {
            probes: AtomicU32::new(0),
            ticks_to_done,
            exit_code,
            log: log.into(),
            killed: Mutex::new(Vec::new()),
        }
    }
}

#[async_trait]
impl JobSandbox for FakeJobSandbox {
    fn supports_background(&self) -> bool {
        true
    }

    async fn exec_with_timeout(
        &self,
        _bot_id: &str,
        command: &str,
        _timeout_ms: u64,
    ) -> ExecResult {
        let body = if command.contains(".jobs") {
            self.log.clone()
        } else {
            String::new()
        };
        ExecResult {
            stdout: body,
            stderr: String::new(),
            exit_code: 0,
            timed_out: false,
            truncated: false,
            unavailable: false,
        }
    }

    async fn spawn(&self, _bot_id: &str, job_id: &str, _command: &str) -> SpawnResult {
        SpawnResult {
            ok: true,
            handle: server::sandbox::container_for_job(job_id),
            detail: "started".to_string(),
        }
    }

    async fn probe(&self, _handle: &str) -> ProbeResult {
        let n = self.probes.fetch_add(1, Ordering::SeqCst) + 1;
        if n < self.ticks_to_done {
            return ProbeResult {
                running: true,
                exit_code: None,
                detail: "true 0".to_string(),
            };
        }
        ProbeResult {
            running: false,
            exit_code: self.exit_code,
            detail: format!("false {}", self.exit_code.unwrap_or(0)),
        }
    }

    async fn kill(&self, handle: &str) -> bool {
        self.killed.lock().unwrap().push(handle.to_string());
        true
    }
}

fn open_db() -> Arc<Mutex<Db>> {
    let db = Db::open(":memory:").expect("open db");
    model::routing::set_routing_settings(&db, Some(false), None).unwrap();
    server::judge::set_judge_enabled(&db, false).unwrap();
    Arc::new(Mutex::new(db))
}

fn job_id_from(reply: &str) -> String {
    reply
        .split("Job id ")
        .nth(1)
        .and_then(|s| s.split('.').next())
        .expect("job id in reply")
        .trim()
        .to_string()
}

fn manager_with_jobs(db: Arc<Mutex<Db>>, job_sandbox: Arc<dyn JobSandbox>) -> Arc<RunManager> {
    let exec = Arc::new(UnavailableSandbox::new("unused")) as Arc<dyn Sandbox>;
    Arc::new(RunManager::with_sandbox_and_job_sandbox(
        db,
        as_port(SilentPort),
        exec,
        job_sandbox,
    ))
}

const JOB_TOOLS: &[&str] = &["run_in_background", "job_status", "await_job", "stop_job"];

#[tokio::test]
async fn run_in_background_tool_starts_job_and_returns_id() {
    let db = open_db();
    seed_bot(&db, "arthur", "Arthur");
    let fake = Arc::new(FakeJobSandbox::new(99, Some(0), ""));
    let manager = manager_with_jobs(Arc::clone(&db), fake);
    let toolbox = manager.toolbox_for(
        "arthur",
        Trigger::Chat,
        false,
        "test/model",
        Some(JOB_TOOLS.iter().map(|s| s.to_string()).collect()),
    );
    let outcome = toolbox
        .run(
            "run_in_background",
            r#"{"command":"make all","label":"build"}"#,
        )
        .await;
    let text = outcome.text;
    assert!(text.contains("Started in the background"));
    let id = job_id_from(&text);
    let guard = db.lock().unwrap();
    assert!(get_job(&guard, "arthur", &id).is_some());
}

#[tokio::test]
async fn job_status_lists_and_describes() {
    let db = open_db();
    seed_bot(&db, "arthur", "Arthur");
    let fake = Arc::new(FakeJobSandbox::new(99, Some(0), ""));
    let manager = manager_with_jobs(Arc::clone(&db), fake);
    let toolbox = manager.toolbox_for(
        "arthur",
        Trigger::Chat,
        false,
        "test/model",
        Some(JOB_TOOLS.iter().map(|s| s.to_string()).collect()),
    );
    let start = toolbox
        .run("run_in_background", r#"{"command":"sleep 9"}"#)
        .await
        .text;
    let id = job_id_from(&start);
    let list = toolbox.run("job_status", "{}").await.text;
    assert!(list.contains(&id));
    let one = toolbox
        .run("job_status", &format!(r#"{{"id":"{id}"}}"#))
        .await
        .text;
    assert!(one.contains("running") || one.contains("Running"));
}

#[tokio::test]
async fn await_job_returns_when_finished() {
    let db = open_db();
    seed_bot(&db, "arthur", "Arthur");
    let fake = Arc::new(FakeJobSandbox::new(1, Some(0), "done output"));
    let manager = manager_with_jobs(Arc::clone(&db), fake);
    let toolbox = manager.toolbox_for(
        "arthur",
        Trigger::Chat,
        false,
        "test/model",
        Some(JOB_TOOLS.iter().map(|s| s.to_string()).collect()),
    );
    let start = toolbox
        .run("run_in_background", r#"{"command":"make"}"#)
        .await
        .text;
    let id = job_id_from(&start);
    let waited = toolbox
        .run("await_job", &format!(r#"{{"id":"{id}","seconds":5}}"#))
        .await
        .text;
    assert!(waited.contains("done output") || waited.contains("Done"));
}

#[tokio::test]
async fn stop_job_kills_running_job() {
    let db = open_db();
    seed_bot(&db, "arthur", "Arthur");
    let fake = Arc::new(FakeJobSandbox::new(99, Some(0), ""));
    let job_sandbox: Arc<dyn JobSandbox> = Arc::clone(&fake) as Arc<dyn JobSandbox>;
    let manager = manager_with_jobs(Arc::clone(&db), job_sandbox);
    let toolbox = manager.toolbox_for(
        "arthur",
        Trigger::Chat,
        false,
        "test/model",
        Some(JOB_TOOLS.iter().map(|s| s.to_string()).collect()),
    );
    let start = toolbox
        .run("run_in_background", r#"{"command":"sleep 999"}"#)
        .await
        .text;
    let id = job_id_from(&start);
    let stopped = toolbox
        .run("stop_job", &format!(r#"{{"id":"{id}"}}"#))
        .await
        .text;
    assert!(stopped.contains("Stopped") || stopped.contains("stopped"));
    assert_eq!(fake.killed.lock().unwrap().len(), 1);
}

#[tokio::test]
async fn unavailable_job_sandbox_is_plain_through_toolbox() {
    let db = open_db();
    let bot = {
        let guard = db.lock().unwrap();
        create_bot(
            &guard,
            BotDraft {
                name: "A".into(),
                purpose: "p".into(),
                instructions: "i".into(),
                model: None,
            },
        )
        .unwrap()
        .id
    };
    let manager = manager_with_jobs(
        Arc::clone(&db),
        Arc::new(UnavailableJobSandbox) as Arc<dyn JobSandbox>,
    );
    let toolbox = manager.toolbox_for(
        &bot,
        Trigger::Chat,
        false,
        "test/model",
        Some(vec!["run_in_background".into()]),
    );
    let text = toolbox
        .run("run_in_background", r#"{"command":"make"}"#)
        .await
        .text;
    assert!(text.contains("not available here"));
}
