//! S9-05: deleted worker assignment must not fall back to meridian sandboxes.

mod common;

use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use model::ladder::Trigger;
use model::{ModelEvent, ModelPort, ModelRequest};
use server::runs::RunManager;
use server::sandbox::{ExecResult, Sandbox};
use server::workers::set_bot_worker_id;
use store::Db;
use store::bots::{BotDraft, create_bot};

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

struct MeridianSpy {
    hits: Mutex<u32>,
}

#[async_trait]
impl Sandbox for MeridianSpy {
    async fn exec(&self, _bot_id: &str, _command: &str) -> ExecResult {
        *self.hits.lock().unwrap() += 1;
        ExecResult {
            stdout: "MERIDIAN_EXEC".into(),
            stderr: String::new(),
            exit_code: 0,
            timed_out: false,
            truncated: false,
            unavailable: false,
        }
    }

    async fn read_file(&self, _bot_id: &str, _path: &str) -> Result<String, String> {
        *self.hits.lock().unwrap() += 1;
        Ok("MERIDIAN_READ".into())
    }
}

fn open_db() -> Arc<Mutex<Db>> {
    Arc::new(Mutex::new(Db::open(":memory:").expect("open db")))
}

#[tokio::test]
async fn missing_worker_never_hits_meridian_sandbox() {
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
    {
        let guard = db.lock().unwrap();
        server::workers::ensure_bot_worker_column(&guard).unwrap();
        set_bot_worker_id(&guard, &bot, Some("ghost-worker")).unwrap();
    }

    let spy = Arc::new(MeridianSpy {
        hits: Mutex::new(0),
    });
    let (_, meridian_job) = server::sandbox::default_sandbox_pair();
    let manager = Arc::new(RunManager::with_sandbox_and_job_sandbox(
        Arc::clone(&db),
        common::as_port(SilentPort),
        Arc::clone(&spy) as Arc<dyn Sandbox>,
        meridian_job,
    ));

    let toolbox = manager.toolbox_for(
        &bot,
        Trigger::Chat,
        false,
        "test/model",
        Some(vec!["shell".into(), "run_in_background".into()]),
    );

    let shell = toolbox.run("shell", r#"{"command":"echo hi"}"#).await.text;
    assert!(shell.contains("no longer exists"));
    assert_eq!(*spy.hits.lock().unwrap(), 0);

    let job = toolbox
        .run("run_in_background", r#"{"command":"sleep 1"}"#)
        .await
        .text;
    assert!(
        job.contains("not available here") || job.contains("Background commands are not available")
    );
    assert_eq!(*spy.hits.lock().unwrap(), 0);
}
