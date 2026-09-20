//! SEC5-10: run loop sends `tool_choice: required` only for capable models.

mod common;

use std::sync::{Arc, Mutex};
use std::time::Duration;

use common::{ScriptedPort, as_port, drain, text_script};
use model::ladder::Trigger;
use model::{Catalog, ModelMessage, ModelPort, ToolChoice};
use server::runs::{RunManager, StartOptions};
use store::Db;
use store::vms::VmConfig;

fn vm_config() -> VmConfig {
    VmConfig {
        image: "test-image".into(),
        docker_host: "unix:///test.sock".into(),
        cdp_base: 9500,
        web_base: 6500,
        slots: 8,
        idle_ms: 1_800_000,
        init_dir: "/vm-init".into(),
        memory: "3g".into(),
        cpus: "1.5".into(),
        shm_size: "2g".into(),
        timezone: "UTC".into(),
        puid: "1000".into(),
        pgid: "1000".into(),
    }
}

struct SharedScriptedPort(Arc<ScriptedPort>);

impl ModelPort for SharedScriptedPort {
    fn stream(&self, request: model::ModelRequest) -> model::EventStream {
        self.0.stream(request)
    }
}

fn gemini_catalog() -> Arc<dyn Catalog> {
    Arc::new(
        model::FixtureCatalog::from_json(
            r#"[
            {
                "id": "google/gemini-3.8-flash",
                "name": "Gemini 3.8 Flash",
                "inPerM": 0.1,
                "outPerM": 0.4,
                "contextLength": 1000000,
                "supportsTools": true,
                "supportsImages": true,
                "supportsReasoning": false,
                "supportsToolChoiceRequired": true
            }
        ]"#,
        )
        .expect("catalog json"),
    )
}

fn qwen_catalog() -> Arc<dyn Catalog> {
    Arc::new(
        model::FixtureCatalog::from_json(
            r#"[
            {
                "id": "qwen/qwen3.8-flash",
                "name": "Qwen 3.8 Flash",
                "inPerM": 0.1,
                "outPerM": 0.4,
                "contextLength": 128000,
                "supportsTools": true,
                "supportsImages": true,
                "supportsReasoning": false,
                "supportsToolChoiceRequired": false
            }
        ]"#,
        )
        .expect("catalog json"),
    )
}

fn open_db() -> Arc<Mutex<Db>> {
    let db = Db::open(":memory:").expect("open db");
    model::routing::set_routing_settings(&db, Some(false), None).expect("routing off");
    server::judge::set_judge_enabled(&db, false).expect("judge off");
    Arc::new(Mutex::new(db))
}

fn manager_with_catalog(
    db: Arc<Mutex<Db>>,
    port: Arc<ScriptedPort>,
    catalog: Arc<dyn Catalog>,
) -> Arc<RunManager> {
    Arc::new(RunManager::with_sandbox_vm_and_catalog(
        db,
        as_port(SharedScriptedPort(Arc::clone(&port))),
        server::sandbox::default_sandbox(),
        Arc::new(server::vm::DisabledDockerRun),
        Arc::new(vm_config()),
        false,
        catalog,
    ))
}

async fn wait_for_run(manager: &RunManager, run_id: &str) {
    tokio::time::timeout(Duration::from_secs(2), drain(manager.subscribe(run_id)))
        .await
        .expect("run completed");
}

#[tokio::test]
async fn capable_model_gets_tool_choice_required_on_tool_turns() {
    let db = open_db();
    common::seed_bot(&db, "bot", "Bot");
    let conversation_id = common::own_conversation(&db, "bot");

    let port = Arc::new(ScriptedPort::new(vec![text_script("done")]));
    let manager = manager_with_catalog(Arc::clone(&db), Arc::clone(&port), gemini_catalog());

    let run_id = manager.start(StartOptions {
        bot_id: "bot".to_string(),
        conversation_id,
        model: "google/gemini-3.8-flash".to_string(),
        messages: vec![ModelMessage::user("hello")],
        trigger: Trigger::Chat,
        room: false,
    });

    wait_for_run(&manager, &run_id).await;
    let requests = port.requests();
    assert!(
        requests
            .iter()
            .any(|r| r.tool_choice == Some(ToolChoice::Required)),
        "expected tool_choice Required on a tool-offering turn: {requests:?}"
    );
}

#[tokio::test]
async fn qwen_catalog_does_not_send_tool_choice_required() {
    let db = open_db();
    common::seed_bot(&db, "bot", "Bot");
    let conversation_id = common::own_conversation(&db, "bot");

    let port = Arc::new(ScriptedPort::new(vec![text_script("done")]));
    let manager = manager_with_catalog(Arc::clone(&db), Arc::clone(&port), qwen_catalog());

    let run_id = manager.start(StartOptions {
        bot_id: "bot".to_string(),
        conversation_id,
        model: "qwen/qwen3.8-flash".to_string(),
        messages: vec![ModelMessage::user("hello")],
        trigger: Trigger::Chat,
        room: false,
    });

    wait_for_run(&manager, &run_id).await;
    let requests = port.requests();
    assert!(
        requests.iter().all(|r| r.tool_choice.is_none()),
        "qwen must not get tool_choice: {requests:?}"
    );
}
