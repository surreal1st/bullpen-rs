//! S6L-02 acceptance: `shell` drives a real `Sandbox` once Josh approves it
//! (rather than S2's hardcoded stub), and the new `sandbox_read` tool reads
//! a bot's own `/work` through the same seam. Drives `RunManager` directly
//! via `RunManager::with_sandbox`, injecting a `DockerSandbox` backed by
//! `FakeRunner` (or `UnavailableSandbox`) - never touches a real `docker`
//! daemon. Matches `tests/approvals.rs`'s own posture and helpers.

mod common;

use std::sync::Arc;
use std::time::Duration;

use common::{ScriptedPort, as_port, own_conversation, seed_bot, seed_user_message};
use model::ladder::Trigger;
use model::{MessageContent, ModelEvent, ModelMessage, ToolCall};
use server::runs::{RunEvent, RunManager, StartOptions};
use server::sandbox::{
    CommandRunner, DockerSandbox, FakeRunner, Sandbox, SandboxConfig, UnavailableSandbox,
};
use std::sync::Mutex;
use store::Db;

fn open_db() -> Arc<Mutex<Db>> {
    // S2-04: routing defaults to enabled; disable it so a scripted test's
    // first reply isn't consumed by the classifier instead of the turn it
    // scripted it for - same reasoning as `tests/approvals.rs::open_db`.
    let db = Db::open(":memory:").expect("open :memory: db");
    model::routing::set_routing_settings(&db, Some(false), None)
        .expect("disable routing classifier for scripted-model tests");
    Arc::new(Mutex::new(db))
}

fn run_status(db: &Arc<Mutex<Db>>, run_id: &str) -> String {
    let db = db.lock().expect("db mutex poisoned");
    db.conn()
        .query_row(
            "SELECT status FROM runs WHERE id = ?1",
            rusqlite::params![run_id],
            |row| row.get(0),
        )
        .expect("read run row")
}

fn run_messages_json(db: &Arc<Mutex<Db>>, run_id: &str) -> String {
    let db = db.lock().expect("db mutex poisoned");
    db.conn()
        .query_row(
            "SELECT messages FROM runs WHERE id = ?1",
            rusqlite::params![run_id],
            |row| row.get(0),
        )
        .expect("read run messages")
}

fn tool_result_text(db: &Arc<Mutex<Db>>, run_id: &str) -> String {
    let messages_json = run_messages_json(db, run_id);
    let messages: Vec<ModelMessage> =
        serde_json::from_str(&messages_json).expect("parse stored messages");
    let tool_result = messages
        .iter()
        .find(|m| m.role == "tool")
        .expect("a tool result message");
    let MessageContent::Text(text) = &tool_result.content else {
        panic!("expected text content");
    };
    text.clone()
}

fn pending_approval(db: &Arc<Mutex<Db>>, run_id: &str) -> Option<String> {
    let db = db.lock().expect("db mutex poisoned");
    db.conn()
        .query_row(
            "SELECT id FROM approvals WHERE run_id = ?1",
            rusqlite::params![run_id],
            |row| row.get(0),
        )
        .ok()
}

async fn drain_until_paused_or_done(
    mut rx: tokio::sync::mpsc::UnboundedReceiver<RunEvent>,
) -> Vec<RunEvent> {
    let mut seen = Vec::new();
    while let Some(event) = rx.recv().await {
        let stop = matches!(
            event,
            RunEvent::Done { .. } | RunEvent::Error { .. } | RunEvent::ApprovalNeeded { .. }
        );
        seen.push(event);
        if stop {
            break;
        }
    }
    seen
}

async fn wait_for_status(db: &Arc<Mutex<Db>>, run_id: &str, target: &str) -> String {
    let mut status = run_status(db, run_id);
    for _ in 0..300 {
        if status == target {
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
        status = run_status(db, run_id);
    }
    status
}

fn shell_then_answer(command: &str, answer: &str) -> ScriptedPort {
    ScriptedPort::new(vec![
        vec![ModelEvent::ToolCalls {
            calls: vec![ToolCall {
                id: "call-1".to_string(),
                name: "shell".to_string(),
                arguments: format!("{{\"command\":{command:?}}}"),
            }],
            usage: None,
        }],
        vec![
            ModelEvent::Delta {
                text: answer.to_string(),
            },
            ModelEvent::Done {
                model: "test/model".to_string(),
                usage: None,
                finish_reason: None,
            },
        ],
    ])
}

fn sandbox_read_then_answer(path: &str, answer: &str) -> ScriptedPort {
    ScriptedPort::new(vec![
        vec![ModelEvent::ToolCalls {
            calls: vec![ToolCall {
                id: "call-1".to_string(),
                name: "sandbox_read".to_string(),
                arguments: format!("{{\"path\":{path:?}}}"),
            }],
            usage: None,
        }],
        vec![
            ModelEvent::Delta {
                text: answer.to_string(),
            },
            ModelEvent::Done {
                model: "test/model".to_string(),
                usage: None,
                finish_reason: None,
            },
        ],
    ])
}

fn docker_sandbox_with(runner: &Arc<FakeRunner>) -> Arc<dyn Sandbox> {
    let runner_dyn: Arc<dyn CommandRunner> = runner.clone();
    Arc::new(DockerSandbox::new(SandboxConfig::default(), runner_dyn))
}

// 1. Approve: `shell` drives the real sandbox. The fake recorded an argv
//    that actually shells the command out (`sh -c <command>`), and the
//    scripted stdout - not S2's stub - lands in the transcript, fenced as
//    untrusted data.
#[tokio::test]
async fn approving_shell_runs_the_real_sandbox_and_fences_the_output() {
    let db = open_db();
    seed_bot(&db, "arthur", "Arthur");
    let conversation_id = own_conversation(&db, "arthur");
    seed_user_message(&db, &conversation_id, "list work");

    let runner = Arc::new(FakeRunner::new());
    runner.push_response("file1.txt\nfile2.txt\n", "", 0);
    let sandbox = docker_sandbox_with(&runner);

    let manager = Arc::new(RunManager::with_sandbox(
        Arc::clone(&db),
        as_port(shell_then_answer("ls /work", "Listed it.")),
        sandbox,
    ));
    let run_id = manager.start(StartOptions {
        bot_id: "arthur".to_string(),
        conversation_id: conversation_id.clone(),
        model: "test/model".to_string(),
        messages: vec![ModelMessage::user("list work")],
        trigger: Trigger::Chat,
        room: false,
    });

    drain_until_paused_or_done(manager.subscribe(&run_id)).await;
    let approval_id = pending_approval(&db, &run_id).expect("pending approval");
    assert!(manager.decide_approval(&approval_id, true).await);
    assert_eq!(wait_for_status(&db, &run_id, "done").await, "done");

    let commands = runner.commands();
    assert_eq!(commands.len(), 1, "expected exactly one real exec");
    let argv = &commands[0];
    assert!(
        argv.iter().any(|a| a == "sh"),
        "expected `sh` in the argv, got {argv:?}"
    );
    assert!(
        argv.iter().any(|a| a == "ls /work"),
        "expected the bot's command in the argv, got {argv:?}"
    );

    let text = tool_result_text(&db, &run_id);
    assert!(
        text.contains("<<<TOOL_OUTPUT_DATA>>>") && text.contains("<<<END_TOOL_OUTPUT_DATA>>>"),
        "expected the untrusted-data fence, got {text:?}"
    );
    assert!(
        text.contains("file1.txt"),
        "expected the fake's scripted stdout in the transcript, got {text:?}"
    );
}

// 2. Reject: nothing runs. The fake recorded no exec at all.
#[tokio::test]
async fn rejecting_shell_never_touches_the_sandbox() {
    let db = open_db();
    seed_bot(&db, "arthur", "Arthur");
    let conversation_id = own_conversation(&db, "arthur");
    seed_user_message(&db, &conversation_id, "delete work");

    let runner = Arc::new(FakeRunner::new());
    let sandbox = docker_sandbox_with(&runner);

    let manager = Arc::new(RunManager::with_sandbox(
        Arc::clone(&db),
        as_port(shell_then_answer("rm -rf /work/x", "I did not.")),
        sandbox,
    ));
    let run_id = manager.start(StartOptions {
        bot_id: "arthur".to_string(),
        conversation_id: conversation_id.clone(),
        model: "test/model".to_string(),
        messages: vec![ModelMessage::user("delete work")],
        trigger: Trigger::Chat,
        room: false,
    });

    drain_until_paused_or_done(manager.subscribe(&run_id)).await;
    let approval_id = pending_approval(&db, &run_id).expect("pending approval");
    assert!(manager.decide_approval(&approval_id, false).await);
    assert_eq!(wait_for_status(&db, &run_id, "done").await, "done");

    assert!(
        runner.commands().is_empty(),
        "a rejected call must never reach the sandbox"
    );
}

// 3. `sandbox_read` defaults to `allow` (no approval prompt) and returns
//    the fake's content, fenced as untrusted data.
#[tokio::test]
async fn sandbox_read_returns_the_fakes_content_with_no_approval() {
    let db = open_db();
    seed_bot(&db, "arthur", "Arthur");
    let conversation_id = own_conversation(&db, "arthur");
    seed_user_message(&db, &conversation_id, "what did you write");

    let runner = Arc::new(FakeRunner::new());
    runner.push_response("hello from work\n", "", 0);
    let sandbox = docker_sandbox_with(&runner);

    let manager = Arc::new(RunManager::with_sandbox(
        Arc::clone(&db),
        as_port(sandbox_read_then_answer("notes.txt", "Read it.")),
        sandbox,
    ));
    let run_id = manager.start(StartOptions {
        bot_id: "arthur".to_string(),
        conversation_id: conversation_id.clone(),
        model: "test/model".to_string(),
        messages: vec![ModelMessage::user("what did you write")],
        trigger: Trigger::Chat,
        room: false,
    });

    common::drain(manager.subscribe(&run_id)).await;
    assert_eq!(wait_for_status(&db, &run_id, "done").await, "done");
    assert!(
        pending_approval(&db, &run_id).is_none(),
        "sandbox_read defaults to allow - it must never park the run"
    );

    let text = tool_result_text(&db, &run_id);
    assert!(
        text.contains("<<<TOOL_OUTPUT_DATA>>>"),
        "expected the untrusted-data fence, got {text:?}"
    );
    assert!(
        text.contains("hello from work"),
        "expected the fake's file content, got {text:?}"
    );
}

// 4. A path outside `/work` is refused before anything shells out - the
//    fake records no exec, and the model sees the refusal text.
#[tokio::test]
async fn sandbox_read_refuses_a_path_outside_work() {
    let db = open_db();
    seed_bot(&db, "arthur", "Arthur");
    let conversation_id = own_conversation(&db, "arthur");
    seed_user_message(&db, &conversation_id, "read the etc passwd");

    let runner = Arc::new(FakeRunner::new());
    let sandbox = docker_sandbox_with(&runner);

    let manager = Arc::new(RunManager::with_sandbox(
        Arc::clone(&db),
        as_port(sandbox_read_then_answer("../etc/passwd", "Could not.")),
        sandbox,
    ));
    let run_id = manager.start(StartOptions {
        bot_id: "arthur".to_string(),
        conversation_id: conversation_id.clone(),
        model: "test/model".to_string(),
        messages: vec![ModelMessage::user("read the etc passwd")],
        trigger: Trigger::Chat,
        room: false,
    });

    common::drain(manager.subscribe(&run_id)).await;
    assert_eq!(wait_for_status(&db, &run_id, "done").await, "done");

    let text = tool_result_text(&db, &run_id);
    assert!(
        text.contains("cannot escape /work"),
        "expected the path-refusal text, got {text:?}"
    );
    assert!(
        runner.commands().is_empty(),
        "a refused path must never reach the sandbox"
    );
}

// 5. Unavailable: `shell` still answers exactly S2's stub text once
//    approved, unfenced - not the generic exec formatting.
#[tokio::test]
async fn shell_stays_on_the_s2_text_when_the_sandbox_is_unavailable() {
    let db = open_db();
    seed_bot(&db, "arthur", "Arthur");
    let conversation_id = own_conversation(&db, "arthur");
    seed_user_message(&db, &conversation_id, "clean up");

    let sandbox: Arc<dyn Sandbox> = Arc::new(UnavailableSandbox::new(
        "Sandboxing is off here. Set BULLPEN_SANDBOX=on where it is wanted.",
    ));

    let manager = Arc::new(RunManager::with_sandbox(
        Arc::clone(&db),
        as_port(shell_then_answer("rm -rf /work/x", "Cleaned it up.")),
        sandbox,
    ));
    let run_id = manager.start(StartOptions {
        bot_id: "arthur".to_string(),
        conversation_id: conversation_id.clone(),
        model: "test/model".to_string(),
        messages: vec![ModelMessage::user("clean up")],
        trigger: Trigger::Chat,
        room: false,
    });

    drain_until_paused_or_done(manager.subscribe(&run_id)).await;
    let approval_id = pending_approval(&db, &run_id).expect("pending approval");
    assert!(manager.decide_approval(&approval_id, true).await);
    assert_eq!(wait_for_status(&db, &run_id, "done").await, "done");

    let text = tool_result_text(&db, &run_id);
    assert_eq!(
        text,
        "No sandbox is available, so nothing was run. Sandboxing is off here. Set \
BULLPEN_SANDBOX=on where it is wanted."
    );
}

// 6. Unavailable: `sandbox_read` answers the matching S2-shaped read text,
//    unfenced, same as `shell`.
#[tokio::test]
async fn sandbox_read_stays_on_the_s2_text_when_the_sandbox_is_unavailable() {
    let db = open_db();
    seed_bot(&db, "arthur", "Arthur");
    let conversation_id = own_conversation(&db, "arthur");
    seed_user_message(&db, &conversation_id, "what did you write");

    let sandbox: Arc<dyn Sandbox> = Arc::new(UnavailableSandbox::new(
        "Sandboxing is off here. Set BULLPEN_SANDBOX=on where it is wanted.",
    ));

    let manager = Arc::new(RunManager::with_sandbox(
        Arc::clone(&db),
        as_port(sandbox_read_then_answer("notes.txt", "Could not read it.")),
        sandbox,
    ));
    let run_id = manager.start(StartOptions {
        bot_id: "arthur".to_string(),
        conversation_id: conversation_id.clone(),
        model: "test/model".to_string(),
        messages: vec![ModelMessage::user("what did you write")],
        trigger: Trigger::Chat,
        room: false,
    });

    common::drain(manager.subscribe(&run_id)).await;
    assert_eq!(wait_for_status(&db, &run_id, "done").await, "done");

    let text = tool_result_text(&db, &run_id);
    assert_eq!(
        text,
        "No sandbox is available, so nothing was read. Sandboxing is off here. Set \
BULLPEN_SANDBOX=on where it is wanted."
    );
}
