//! S9-02: shell job runner — port of TS `test/jobs.test.ts` background command cases.

use async_trait::async_trait;
use server::delegate::AskResult;
use server::job_runner::{JobRunnerDeps, JobSandbox, start_agent_job, start_shell_job, stop_job};
use server::sandbox::{ExecResult, ProbeResult, SpawnResult, container_for_job, job_log_path};
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, Mutex};
use store::Db;
use store::bots::{BotDraft, create_bot};
use store::create_job;
use store::jobs::get_job;
use store::{JobKind, JobStatus};

struct FakeJobSandbox {
    probes: AtomicU32,
    ticks_to_done: u32,
    exit_code: Option<i32>,
    vanish: bool,
    log: String,
    killed: Mutex<Vec<String>>,
    spawn_ok: bool,
}

impl FakeJobSandbox {
    fn new(ticks_to_done: u32, exit_code: Option<i32>, log: impl Into<String>) -> Self {
        Self {
            probes: AtomicU32::new(0),
            ticks_to_done,
            exit_code,
            vanish: false,
            log: log.into(),
            killed: Mutex::new(Vec::new()),
            spawn_ok: true,
        }
    }

    fn vanish(ticks_to_done: u32) -> Self {
        Self {
            probes: AtomicU32::new(0),
            ticks_to_done,
            exit_code: None,
            vanish: true,
            log: String::new(),
            killed: Mutex::new(Vec::new()),
            spawn_ok: true,
        }
    }

    fn killed_handles(&self) -> Vec<String> {
        self.killed.lock().unwrap().clone()
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
        if self.spawn_ok {
            SpawnResult {
                ok: true,
                handle: container_for_job(job_id),
                detail: "started".to_string(),
            }
        } else {
            SpawnResult {
                ok: false,
                handle: container_for_job(job_id),
                detail: "failed".to_string(),
            }
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
        if self.vanish {
            return ProbeResult {
                running: false,
                exit_code: None,
                detail: "No such container".to_string(),
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

struct NoBackgroundSandbox;

#[async_trait]
impl JobSandbox for NoBackgroundSandbox {
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

fn seed() -> (Db, String) {
    let db = Db::open(":memory:").expect("open");
    let id = create_bot(
        &db,
        BotDraft {
            name: "A".into(),
            purpose: "p".into(),
            instructions: "i".into(),
            model: None,
        },
    )
    .expect("bot")
    .id;
    (db, id)
}

fn seed_two() -> (Db, String, String) {
    let db = Db::open(":memory:").expect("open");
    let a = create_bot(
        &db,
        BotDraft {
            name: "A".into(),
            purpose: "p".into(),
            instructions: "i".into(),
            model: None,
        },
    )
    .expect("bot")
    .id;
    let b = create_bot(
        &db,
        BotDraft {
            name: "B".into(),
            purpose: "p".into(),
            instructions: "i".into(),
            model: None,
        },
    )
    .expect("bot")
    .id;
    (db, a, b)
}

fn id_from(reply: &str) -> String {
    reply
        .split("Job id ")
        .nth(1)
        .and_then(|s| s.split('.').next())
        .expect("job id in reply")
        .trim()
        .to_string()
}

async fn settle(db: &Arc<Mutex<Db>>, bot_id: &str, job_id: &str) -> Option<store::Job> {
    for _ in 0..100 {
        let job = {
            let guard = db.lock().unwrap();
            get_job(&guard, bot_id, job_id)
        };
        if job.as_ref().is_some_and(|j| j.status != JobStatus::Running) {
            return job;
        }
        tokio::time::sleep(std::time::Duration::from_millis(5)).await;
    }
    let guard = db.lock().unwrap();
    get_job(&guard, bot_id, job_id)
}

#[tokio::test]
async fn returns_job_id_and_tells_bot_not_to_wait() {
    let (db, bot) = seed();
    let db = Arc::new(Mutex::new(db));
    let sandbox = Arc::new(FakeJobSandbox::new(99, Some(0), ""));
    let deps = JobRunnerDeps {
        db: Arc::clone(&db),
        sandbox,
        poll_ms: 1,
        max_ms: MAX_TEST_MS,
    };
    let reply = start_shell_job(&deps, &bot, "long build", "sleep 60", None).await;
    assert!(reply.contains("Started in the background"));
    assert!(reply.contains("do not poll it in a loop"));
    let id = id_from(&reply);
    let guard = db.lock().unwrap();
    assert_eq!(
        get_job(&guard, &bot, &id).unwrap().status,
        JobStatus::Running
    );
}

const MAX_TEST_MS: u64 = 60_000;

#[tokio::test]
async fn records_output_and_exit_code_when_finished() {
    let (db, bot) = seed();
    let db = Arc::new(Mutex::new(db));
    let sandbox = Arc::new(FakeJobSandbox::new(2, Some(0), "built ok"));
    let deps = JobRunnerDeps {
        db: Arc::clone(&db),
        sandbox,
        poll_ms: 1,
        max_ms: MAX_TEST_MS,
    };
    let reply = start_shell_job(&deps, &bot, "build", "make", None).await;
    let id = id_from(&reply);
    let job = settle(&db, &bot, &id).await.unwrap();
    assert_eq!(job.status, JobStatus::Done);
    assert_eq!(job.exit_code, Some(0));
    assert!(job.output.contains("built ok"));
}

#[tokio::test]
async fn non_zero_exit_is_failed() {
    let (db, bot) = seed();
    let db = Arc::new(Mutex::new(db));
    let sandbox = Arc::new(FakeJobSandbox::new(1, Some(2), "error: nope"));
    let deps = JobRunnerDeps {
        db: Arc::clone(&db),
        sandbox,
        poll_ms: 1,
        max_ms: MAX_TEST_MS,
    };
    let reply = start_shell_job(&deps, &bot, "build", "make", None).await;
    let job = settle(&db, &bot, &id_from(&reply)).await.unwrap();
    assert_eq!(job.status, JobStatus::Failed);
    assert_eq!(job.exit_code, Some(2));
}

#[tokio::test]
async fn vanished_container_is_not_success() {
    let (db, bot) = seed();
    let db = Arc::new(Mutex::new(db));
    let sandbox = Arc::new(FakeJobSandbox::vanish(1));
    let deps = JobRunnerDeps {
        db: Arc::clone(&db),
        sandbox,
        poll_ms: 1,
        max_ms: MAX_TEST_MS,
    };
    let reply = start_shell_job(&deps, &bot, "build", "make", None).await;
    let job = settle(&db, &bot, &id_from(&reply)).await.unwrap();
    assert_eq!(job.status, JobStatus::Failed);
    assert!(job.exit_code.is_none());
    assert!(job.output.contains("not as success"));
}

#[tokio::test]
async fn cleans_up_container_when_done() {
    let (db, bot) = seed();
    let db = Arc::new(Mutex::new(db));
    let fake = Arc::new(FakeJobSandbox::new(1, Some(0), ""));
    let sandbox: Arc<dyn JobSandbox> = Arc::clone(&fake) as Arc<dyn JobSandbox>;
    let deps = JobRunnerDeps {
        db: Arc::clone(&db),
        sandbox,
        poll_ms: 1,
        max_ms: MAX_TEST_MS,
    };
    let reply = start_shell_job(&deps, &bot, "build", "make", None).await;
    settle(&db, &bot, &id_from(&reply)).await;
    assert_eq!(fake.killed_handles().len(), 1);
}

#[tokio::test]
async fn unavailable_sandbox_is_plain() {
    let (db, bot) = seed();
    let db = Arc::new(Mutex::new(db));
    let deps = JobRunnerDeps::new(db, Arc::new(NoBackgroundSandbox));
    let reply = start_shell_job(&deps, &bot, "x", "make", None).await;
    assert!(reply.contains("not available here"));
}

#[tokio::test]
async fn kills_past_time_limit() {
    let (db, bot) = seed();
    let db = Arc::new(Mutex::new(db));
    let fake = Arc::new(FakeJobSandbox::new(99, Some(0), ""));
    let sandbox: Arc<dyn JobSandbox> = Arc::clone(&fake) as Arc<dyn JobSandbox>;
    let deps = JobRunnerDeps {
        db: Arc::clone(&db),
        sandbox,
        poll_ms: 1,
        max_ms: 0,
    };
    let reply = start_shell_job(&deps, &bot, "forever", "sleep 99999", None).await;
    let job = settle(&db, &bot, &id_from(&reply)).await.unwrap();
    assert_eq!(job.status, JobStatus::Failed);
    assert!(job.output.contains("two-hour limit"));
    assert_eq!(fake.killed_handles().len(), 1);
}

#[tokio::test]
async fn agent_job_returns_immediately_and_fills_answer_later() {
    let (db, a, b) = seed_two();
    let db = Arc::new(Mutex::new(db));
    let (tx, rx) = tokio::sync::oneshot::channel::<AskResult>();
    let reply = start_agent_job(
        Arc::clone(&db),
        &a,
        &b,
        "B",
        "what do you think?",
        move |_, _| async move {
            rx.await.unwrap_or(AskResult {
                reply: String::new(),
                usage: None,
                error: Some("channel closed".into()),
            })
        },
    );
    let id = id_from(&reply);
    {
        let guard = db.lock().unwrap();
        assert_eq!(get_job(&guard, &a, &id).unwrap().status, JobStatus::Running);
    }
    let _ = tx.send(AskResult {
        reply: "I think yes".into(),
        usage: None,
        error: None,
    });
    let job = settle(&db, &a, &id).await.unwrap();
    assert_eq!(job.status, JobStatus::Done);
    assert!(job.output.contains("I think yes"));
}

#[tokio::test]
async fn agent_job_records_colleague_failure() {
    let (db, a, b) = seed_two();
    let db = Arc::new(Mutex::new(db));
    let reply = start_agent_job(Arc::clone(&db), &a, &b, "B", "q", |_, _| async move {
        AskResult {
            reply: String::new(),
            usage: None,
            error: Some("429 rate limited".into()),
        }
    });
    let job = settle(&db, &a, &id_from(&reply)).await.unwrap();
    assert_eq!(job.status, JobStatus::Failed);
    assert!(job.output.contains("429"));
}

#[tokio::test]
async fn agent_job_records_cost_usd_on_finish() {
    let (db, a, b) = seed_two();
    let db = Arc::new(Mutex::new(db));
    let reply = start_agent_job(Arc::clone(&db), &a, &b, "B", "q", |_, _| async move {
        AskResult {
            reply: "ok".into(),
            usage: Some(model::ModelUsage {
                cost_usd: 0.0025,
                input_tokens: 0,
                output_tokens: 0,
                cached_tokens: 0,
                cost_known: true,
            }),
            error: None,
        }
    });
    let job = settle(&db, &a, &id_from(&reply)).await.unwrap();
    assert_eq!(job.cost_usd, Some(0.0025));
}

#[tokio::test]
async fn stopping_agent_job_says_answer_is_discarded() {
    let (db, a) = seed();
    let db = Arc::new(Mutex::new(db));
    let sandbox = Arc::new(FakeJobSandbox::new(99, Some(0), ""));
    let job = {
        let guard = db.lock().unwrap();
        create_job(&guard, &a, JobKind::Agent, "ask B", "q", None).expect("job")
    };
    let deps = JobRunnerDeps::new(Arc::clone(&db), sandbox);
    stop_job(&deps, &a, &job.id).await;
    let guard = db.lock().unwrap();
    assert!(
        get_job(&guard, &a, &job.id)
            .unwrap()
            .output
            .contains("discarded")
    );
}

#[test]
fn job_log_paths_match_ts() {
    assert_eq!(job_log_path("abc"), "/work/.jobs/abc.log");
    assert_eq!(container_for_job("a/b c"), "bullpen-job-a_b_c");
}
