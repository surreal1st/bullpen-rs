//! S9-06: `spawn_helper` — narrowed tools, cheap model, depth gate.

mod common;

use std::sync::{Arc, Mutex};

use common::ScriptedPort;
use model::ladder::Trigger;
use model::routing::set_routing_settings;
use model::{ModelEvent, ModelMessage};
use server::helpers::helper_spec;
use server::runs::RunManager;
use server::sandbox;
use store::Db;
use store::bots::{BotDraft, create_bot};

fn open_db() -> Arc<Mutex<Db>> {
    let db = Db::open(":memory:").expect("open db");
    set_routing_settings(&db, Some(false), None).expect("routing off");
    Arc::new(Mutex::new(db))
}

fn seed_bot(db: &Db, name: &str) -> String {
    create_bot(
        db,
        BotDraft {
            name: name.into(),
            purpose: "p".into(),
            instructions: format!("You are {name}."),
            model: None,
        },
    )
    .expect("bot")
    .id
}

#[tokio::test]
async fn spawn_helper_read_kind_uses_cheap_model_and_narrow_tools() {
    let db = open_db();
    let bot = {
        let g = db.lock().unwrap();
        seed_bot(&g, "Arthur")
    };
    let read_system = helper_spec("read").unwrap().system;
    let scripted = Arc::new(ScriptedPort::new(vec![vec![
        ModelEvent::Delta {
            text: "Nothing in memory matches.".into(),
        },
        ModelEvent::Done {
            model: "test/model".into(),
            usage: None,
            finish_reason: None,
        },
    ]]));
    let port: Arc<dyn model::ModelPort> = scripted.clone();
    let (sb, js) = sandbox::default_sandbox_pair();
    let manager = Arc::new(RunManager::with_sandbox_and_job_sandbox(
        Arc::clone(&db),
        port,
        sb,
        js,
    ));
    let toolbox = manager.toolbox_for(
        &bot,
        Trigger::Chat,
        false,
        "anthropic/claude-fable-5.1",
        Some(vec!["spawn_helper".into()]),
    );
    let out = toolbox
        .run(
            "spawn_helper",
            r#"{"kind":"read","brief":"What did Josh say about deploy?"}"#,
        )
        .await;
    assert!(out.text.contains("Helper (read) says:"));
    assert!(out.text.contains("Nothing in memory"));
    let reqs = scripted.requests();
    assert_eq!(reqs.len(), 1);
    let helper_req = &reqs[0];
    let system = match &helper_req.messages[0] {
        ModelMessage {
            content: model::MessageContent::Text(s),
            ..
        } => s.as_str(),
        _ => panic!("expected text system"),
    };
    assert!(system.starts_with(read_system));
    assert!(!system.contains("## Where you are"));
    let offered: std::collections::HashSet<_> = helper_req
        .tools
        .as_ref()
        .expect("tools offered")
        .iter()
        .map(|t| t.name.as_str())
        .collect();
    assert_eq!(offered, std::collections::HashSet::from(["search_memory"]));
    let floor = {
        let g = db.lock().unwrap();
        model::ladder::default_model(&g)
    };
    assert_eq!(helper_req.model, floor);
}

#[tokio::test]
async fn spawn_helper_refuses_at_delegation_depth() {
    let db = open_db();
    let bot = {
        let g = db.lock().unwrap();
        seed_bot(&g, "Arthur")
    };
    let (sb, js) = sandbox::default_sandbox_pair();
    let manager = Arc::new(RunManager::with_sandbox_and_job_sandbox(
        Arc::clone(&db),
        Arc::new(ScriptedPort::new(vec![])),
        sb,
        js,
    ));
    let inner =
        manager.toolbox_for_helper(&bot, Trigger::Chat, false, 1, vec!["spawn_helper".into()]);
    let out = inner
        .run("spawn_helper", r#"{"kind":"read","brief":"go"}"#)
        .await;
    assert!(out.text.contains("cannot pass it on again"));
}
