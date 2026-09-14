//! S4-04 acceptance: `RunManager::run_turn`'s tool loop consults the S4-01
//! auto-review judge on every grid-Allow RISKY call, on top of the S2-07
//! rules path. Port of the cases in `S4-tickets.md`'s own "S4-04" section.
//!
//! Drives `RunManager` directly (never `build_app`), matching
//! `tests/approvals.rs`'s and `tests/rules.rs`'s own posture for run
//! behaviour - a raw SQL read of `approvals`/`auto_review_log` and a drained
//! `RunEvent` stream are both things an external HTTP client cannot see
//! (`AppState`'s own `db`/`runs` fields are private to the `server` crate),
//! and this ticket's own cases need both: the judge's verdict/reason on the
//! approval row, the `Notice` events it emits, and the log row every
//! judgement (including a fail-open) writes.
//!
//! Most cases use `shell` under `Trigger::Chat`, where the grid holds
//! whatever Josh set it to. (d) needs an UNATTENDED trigger to prove the
//! Deny wording, and `shell` is one of `permissions::tighten_set`'s own
//! entries - `permissions_for_run` pulls a stored "allow" back to "ask" for
//! anything but `Trigger::Chat`, so it would never reach the judge at all
//! there. `message_bot` is in `judge::RISKY_TOOLS` but NOT in `tighten_set`
//! (delegating to a colleague stays "allow" unattended - see
//! `permissions.rs`'s own doc on why), so it is the one risky tool that can
//! actually reach a grid Allow under `Trigger::Routine` - exactly the case
//! S4's Design section means by "an unattended dangerous call is refused
//! outright".

mod common;

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use common::{drain, own_conversation, seed_bot, seed_user_message, text_script};
use model::ladder::Trigger;
use model::{MessageContent, ModelEvent, ModelMessage, ModelPort, ToolCall};
use server::judge;
use server::permissions::{self, Decision};
use server::rules::{self, RuleBehavior};
use server::runs::{RunEvent, RunManager, StartOptions};
use store::Db;

// ---- fixtures shared with `tests/approvals.rs` / `tests/rules.rs` ----

fn open_db() -> Arc<Mutex<Db>> {
    // S2-04: routing defaults to enabled; disabled here for the same reason
    // every other scripted-model test file disables it - a scripted turn's
    // first reply must not be consumed by the routing classifier instead of
    // the turn it was scripted for.
    let db = Db::open(":memory:").expect("open :memory: db");
    model::routing::set_routing_settings(&db, Some(false), None)
        .expect("disable routing classifier for scripted-model tests");
    Arc::new(Mutex::new(db))
}

fn allow_tool(db: &Arc<Mutex<Db>>, bot_id: &str, tool: &str) {
    let db = db.lock().expect("db mutex poisoned");
    let mut perms = HashMap::new();
    perms.insert(tool.to_string(), Decision::Allow);
    permissions::set_permissions(&db, bot_id, &perms).expect("set permissions");
}

fn start_run(
    manager: &Arc<RunManager>,
    db: &Arc<Mutex<Db>>,
    bot_id: &str,
    trigger: Trigger,
) -> String {
    let conversation_id = own_conversation(db, bot_id);
    seed_user_message(db, &conversation_id, "go");
    manager.start(StartOptions {
        bot_id: bot_id.to_string(),
        conversation_id,
        model: "test/model".to_string(),
        messages: vec![ModelMessage::user("go")],
        trigger,
        room: false,
    })
}

/// The one pending approval row for a run, if there is one, plus the two
/// judge columns S4-01's migration 20 added. Panics on more than one, same
/// posture as `tests/approvals.rs`'s own `pending_approval` - a run parks on
/// at most a single call.
struct PendingRow {
    tool_name: String,
    tool_args: String,
    judge_verdict: Option<String>,
    judge_reason: Option<String>,
}

fn pending_approval(db: &Arc<Mutex<Db>>, run_id: &str) -> Option<PendingRow> {
    let db = db.lock().expect("db mutex poisoned");
    let mut stmt = db
        .conn()
        .prepare(
            "SELECT tool_name, tool_args, judge_verdict, judge_reason FROM approvals WHERE run_id = ?1",
        )
        .expect("prepare");
    let rows: Vec<PendingRow> = stmt
        .query_map(rusqlite::params![run_id], |row| {
            Ok(PendingRow {
                tool_name: row.get(0)?,
                tool_args: row.get(1)?,
                judge_verdict: row.get(2)?,
                judge_reason: row.get(3)?,
            })
        })
        .expect("query")
        .collect::<Result<_, _>>()
        .expect("collect");
    assert!(
        rows.len() <= 1,
        "expected at most one approval row, got {}",
        rows.len()
    );
    rows.into_iter().next()
}

async fn wait_for_pending(db: &Arc<Mutex<Db>>, run_id: &str) -> PendingRow {
    for _ in 0..300 {
        if let Some(row) = pending_approval(db, run_id) {
            return row;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    panic!("no pending approval showed up in time");
}

/// One `auto_review_log` row - every column S4-01's `store::auto_review`
/// gives back, in insertion order (`created_at` ties within the same
/// millisecond, so `rowid` breaks them the same way SQLite already orders
/// an unindexed scan).
struct LogRow {
    tool_name: String,
    verdict: String,
    reason: String,
    decision: String,
}

fn log_rows(db: &Arc<Mutex<Db>>) -> Vec<LogRow> {
    let db = db.lock().expect("db mutex poisoned");
    let mut stmt = db
        .conn()
        .prepare("SELECT tool_name, verdict, reason, decision FROM auto_review_log ORDER BY rowid")
        .expect("prepare");
    stmt.query_map([], |row| {
        Ok(LogRow {
            tool_name: row.get(0)?,
            verdict: row.get(1)?,
            reason: row.get(2)?,
            decision: row.get(3)?,
        })
    })
    .expect("query")
    .collect::<Result<_, _>>()
    .expect("collect")
}

fn as_port_arc(port: &Arc<common::ScriptedPort>) -> Arc<dyn ModelPort> {
    Arc::clone(port) as Arc<dyn ModelPort>
}

/// A single tool-call turn, the shape every test here scripts for its first
/// model call.
fn tool_call_script(call_id: &str, tool_name: &str, args: &str) -> Vec<ModelEvent> {
    vec![ModelEvent::ToolCalls {
        calls: vec![ToolCall {
            id: call_id.to_string(),
            name: tool_name.to_string(),
            arguments: args.to_string(),
        }],
        usage: None,
    }]
}

/// The judge's own reply shape - a bare JSON object, same as `judge.rs`'s
/// own doc asks the model for.
fn judge_reply(verdict: &str, reason: &str) -> Vec<ModelEvent> {
    text_script(&format!(r#"{{"verdict":"{verdict}","reason":"{reason}"}}"#))
}

/// The rules classifier's own reply shape - a bare JSON array of matched
/// rule ids (`rules::classify`'s own doc).
fn classify_reply(ids: &[&str]) -> Vec<ModelEvent> {
    text_script(&serde_json::to_string(ids).expect("serialize rule ids"))
}

/// How many of `port`'s requests were the judge's own utility call - its
/// system message is the only one carrying "verdict" (the classifier's own
/// asks for a JSON array of rule ids instead, and a plain turn's first
/// message is never a system prompt built by either).
fn judge_request_count(port: &common::ScriptedPort) -> usize {
    port.requests()
        .into_iter()
        .filter(|r| {
            r.messages.first().is_some_and(
                |m| matches!(&m.content, MessageContent::Text(t) if t.contains("verdict")),
            )
        })
        .count()
}

// ---- (a) risky -> a pending approval whose row carries verdict + reason ----

#[tokio::test]
async fn risky_verdict_parks_with_verdict_and_reason_on_the_row() {
    let db = open_db();
    seed_bot(&db, "arthur", "Arthur");
    allow_tool(&db, "arthur", "shell");

    let port = Arc::new(common::ScriptedPort::new(vec![
        tool_call_script("call-1", "shell", r#"{"command":"rm -rf /work/x"}"#),
        judge_reply("risky", "deletes files outside the sandbox"),
    ]));
    let manager = Arc::new(RunManager::new(Arc::clone(&db), as_port_arc(&port)));
    let run_id = start_run(&manager, &db, "arthur", Trigger::Chat);

    let row = wait_for_pending(&db, &run_id).await;
    assert_eq!(row.tool_name, "shell");
    assert!(row.tool_args.contains("rm -rf"));
    assert_eq!(row.judge_verdict.as_deref(), Some("risky"));
    assert_eq!(
        row.judge_reason.as_deref(),
        Some("deletes files outside the sandbox")
    );

    let log = log_rows(&db);
    assert_eq!(log.len(), 1);
    assert_eq!(log[0].verdict, "risky");
    assert_eq!(log[0].decision, "ask");
}

// ---- (b) safe -> no approval, tool ran, exactly one judge request ----

#[tokio::test]
async fn safe_verdict_runs_the_tool_with_no_approval() {
    let db = open_db();
    seed_bot(&db, "arthur", "Arthur");
    allow_tool(&db, "arthur", "shell");

    let port = Arc::new(common::ScriptedPort::new(vec![
        tool_call_script("call-1", "shell", r#"{"command":"ls"}"#),
        judge_reply("safe", "just lists files"),
        text_script("Listed them."),
    ]));
    let manager = Arc::new(RunManager::new(Arc::clone(&db), as_port_arc(&port)));
    let run_id = start_run(&manager, &db, "arthur", Trigger::Chat);

    let events = drain(manager.subscribe(&run_id)).await;
    assert!(
        matches!(events.last(), Some(RunEvent::Done { .. })),
        "expected the run to finish, got {events:?}"
    );
    assert!(pending_approval(&db, &run_id).is_none());
    assert_eq!(judge_request_count(&port), 1);

    let log = log_rows(&db);
    assert_eq!(log.len(), 1);
    assert_eq!(log[0].verdict, "safe");
    assert_eq!(log[0].decision, "allow");
}

// ---- (c) dangerous + chat -> pending approval ----

#[tokio::test]
async fn dangerous_verdict_at_a_chat_trigger_parks_instead_of_denying() {
    let db = open_db();
    seed_bot(&db, "arthur", "Arthur");
    allow_tool(&db, "arthur", "shell");

    let port = Arc::new(common::ScriptedPort::new(vec![
        tool_call_script("call-1", "shell", r#"{"command":"rm -rf /"}"#),
        judge_reply("dangerous", "wipes the whole disk"),
    ]));
    let manager = Arc::new(RunManager::new(Arc::clone(&db), as_port_arc(&port)));
    let run_id = start_run(&manager, &db, "arthur", Trigger::Chat);

    let row = wait_for_pending(&db, &run_id).await;
    assert_eq!(row.judge_verdict.as_deref(), Some("dangerous"));
    assert_eq!(row.judge_reason.as_deref(), Some("wipes the whole disk"));

    // Ask-first wins: a dangerous verdict never consults the rules
    // classifier, whatever the bot's rules say - only two requests total
    // (the turn's own tool call, and the judge).
    assert_eq!(port.requests().len(), 2);
    assert_eq!(judge_request_count(&port), 1);
}

// ---- (d) dangerous + Trigger::Routine -> deny wording, no approval row ----

#[tokio::test]
async fn dangerous_verdict_at_an_unattended_trigger_denies_outright() {
    let db = open_db();
    seed_bot(&db, "arthur", "Arthur");
    // message_bot defaults to "allow" and is NOT in `tighten_set` - the one
    // RISKY_TOOLS entry a Routine trigger can still hold on Allow, which is
    // what actually lets the judge see it (shell/ssh/read_file/desk_shell
    // are all pulled back to "ask" unattended before the judge ever runs).

    let port = Arc::new(common::ScriptedPort::new(vec![
        tool_call_script(
            "call-1",
            "message_bot",
            r#"{"bot_id":"colleague","text":"wire the funds today"}"#,
        ),
        judge_reply("dangerous", "asks a colleague to move money unsupervised"),
        text_script("Never mind, I won't."),
    ]));
    let manager = Arc::new(RunManager::new(Arc::clone(&db), as_port_arc(&port)));
    let run_id = start_run(&manager, &db, "arthur", Trigger::Routine);

    let events = drain(manager.subscribe(&run_id)).await;
    assert!(
        matches!(events.last(), Some(RunEvent::Done { .. })),
        "a denied call must not park the run, got {events:?}"
    );

    let deny_result = events.iter().find_map(|event| match event {
        RunEvent::ToolResult { name, result } if name == "message_bot" => Some(result.clone()),
        _ => None,
    });
    assert_eq!(
        deny_result,
        Some(
            "Not allowed: auto review judged message_bot dangerous (asks a colleague to move \
money unsupervised). Carry on without it."
                .to_string()
        )
    );

    assert!(
        pending_approval(&db, &run_id).is_none(),
        "nobody is awake to approve an unattended dangerous call - it must deny, not park"
    );

    let log = log_rows(&db);
    assert_eq!(log.len(), 1);
    assert_eq!(log[0].tool_name, "message_bot");
    assert_eq!(log[0].verdict, "dangerous");
    assert_eq!(log[0].decision, "deny");
}

// ---- (e) risky + a matching allow rule lifts it -> tool ran ----

#[tokio::test]
async fn risky_verdict_lifted_by_a_matching_allow_rule_runs_the_tool() {
    let db = open_db();
    seed_bot(&db, "arthur", "Arthur");
    allow_tool(&db, "arthur", "shell");
    let rule_id = {
        let locked = db.lock().expect("db mutex poisoned");
        rules::add_rule(
            &locked,
            Some("arthur".to_string()),
            "Arthur may run shell commands Josh has already reviewed once",
            RuleBehavior::Allow,
        )
        .expect("seed rule")
        .id
    };

    let port = Arc::new(common::ScriptedPort::new(vec![
        tool_call_script("call-1", "shell", r#"{"command":"ls"}"#),
        judge_reply("risky", "runs an arbitrary shell command"),
        classify_reply(&[rule_id.as_str()]),
        text_script("Listed them."),
    ]));
    let manager = Arc::new(RunManager::new(Arc::clone(&db), as_port_arc(&port)));
    let run_id = start_run(&manager, &db, "arthur", Trigger::Chat);

    let events = drain(manager.subscribe(&run_id)).await;
    assert!(
        matches!(events.last(), Some(RunEvent::Done { .. })),
        "expected the run to finish, got {events:?}"
    );
    assert!(pending_approval(&db, &run_id).is_none());
    assert_eq!(judge_request_count(&port), 1);
    assert_eq!(
        port.requests().len(),
        4,
        "tool call, judge, classifier, final answer"
    );

    let log = log_rows(&db);
    assert_eq!(log.len(), 1);
    assert_eq!(log[0].verdict, "risky");
    // The log records the JUDGE's own decision (ask), not what the rule
    // lifted it to afterward - the rule is a second, separate mechanism.
    assert_eq!(log[0].decision, "ask");
}

// ---- (f) dangerous + the same allow rule -> still pending ----

#[tokio::test]
async fn dangerous_verdict_is_not_liftable_by_a_matching_allow_rule() {
    let db = open_db();
    seed_bot(&db, "arthur", "Arthur");
    allow_tool(&db, "arthur", "shell");
    {
        let locked = db.lock().expect("db mutex poisoned");
        rules::add_rule(
            &locked,
            Some("arthur".to_string()),
            "Arthur may run shell commands Josh has already reviewed once",
            RuleBehavior::Allow,
        )
        .expect("seed rule");
    }

    let port = Arc::new(common::ScriptedPort::new(vec![
        tool_call_script("call-1", "shell", r#"{"command":"rm -rf /"}"#),
        judge_reply("dangerous", "wipes the whole disk"),
    ]));
    let manager = Arc::new(RunManager::new(Arc::clone(&db), as_port_arc(&port)));
    let run_id = start_run(&manager, &db, "arthur", Trigger::Chat);

    let row = wait_for_pending(&db, &run_id).await;
    assert_eq!(row.judge_verdict.as_deref(), Some("dangerous"));
    assert_eq!(
        port.requests().len(),
        2,
        "a dangerous verdict skips the rules classifier even with a matching rule on file"
    );
}

// ---- (g) judge model error -> tool ran, notice says so, log records it ----

#[tokio::test]
async fn judge_model_error_fails_open_and_says_so() {
    let db = open_db();
    seed_bot(&db, "arthur", "Arthur");
    allow_tool(&db, "arthur", "shell");

    let port = Arc::new(common::ScriptedPort::new(vec![
        tool_call_script("call-1", "shell", r#"{"command":"ls"}"#),
        vec![ModelEvent::Error {
            message: "upstream exploded".to_string(),
            status: None,
        }],
        text_script("Listed them."),
    ]));
    let manager = Arc::new(RunManager::new(Arc::clone(&db), as_port_arc(&port)));
    let run_id = start_run(&manager, &db, "arthur", Trigger::Chat);

    let events = drain(manager.subscribe(&run_id)).await;
    assert!(
        matches!(events.last(), Some(RunEvent::Done { .. })),
        "a judge outage must not widen into a stuck run, got {events:?}"
    );
    assert!(pending_approval(&db, &run_id).is_none());

    let notice = events.iter().find_map(|event| match event {
        RunEvent::Notice { message } if message.contains("Auto review unavailable") => {
            Some(message.clone())
        }
        _ => None,
    });
    assert!(
        notice.is_some(),
        "expected a Notice saying auto review was unavailable, got {events:?}"
    );

    let log = log_rows(&db);
    assert_eq!(log.len(), 1);
    assert_eq!(log[0].decision, "allow");
    assert!(log[0].reason.contains("Auto review unavailable"));
}

// ---- (h) toggle off -> zero judge requests ----

#[tokio::test]
async fn toggle_off_skips_the_judge_entirely() {
    let db = open_db();
    seed_bot(&db, "arthur", "Arthur");
    allow_tool(&db, "arthur", "shell");
    {
        let locked = db.lock().expect("db mutex poisoned");
        judge::set_judge_enabled(&locked, false).expect("disable judge");
    }

    let port = Arc::new(common::ScriptedPort::new(vec![
        tool_call_script("call-1", "shell", r#"{"command":"ls"}"#),
        text_script("Listed them."),
    ]));
    let manager = Arc::new(RunManager::new(Arc::clone(&db), as_port_arc(&port)));
    let run_id = start_run(&manager, &db, "arthur", Trigger::Chat);

    let events = drain(manager.subscribe(&run_id)).await;
    assert!(
        matches!(events.last(), Some(RunEvent::Done { .. })),
        "expected the run to finish, got {events:?}"
    );
    assert!(pending_approval(&db, &run_id).is_none());
    assert_eq!(judge_request_count(&port), 0);
    assert_eq!(port.requests().len(), 2, "tool call, then the final answer");
    assert!(log_rows(&db).is_empty());
}

// ---- (i) grid Ask -> zero judge requests (rules path only, as before) ----

#[tokio::test]
async fn a_grid_ask_never_reaches_the_judge() {
    let db = open_db();
    seed_bot(&db, "arthur", "Arthur");
    // No `allow_tool` call: `shell` stays at its default "ask", so the
    // grid itself parks the run - the judge only ever runs on a grid
    // ALLOW (S4's Design section), and this is the pre-S4 path unchanged.

    let port = Arc::new(common::ScriptedPort::new(vec![tool_call_script(
        "call-1",
        "shell",
        r#"{"command":"ls"}"#,
    )]));
    let manager = Arc::new(RunManager::new(Arc::clone(&db), as_port_arc(&port)));
    let run_id = start_run(&manager, &db, "arthur", Trigger::Chat);

    let row = wait_for_pending(&db, &run_id).await;
    assert_eq!(row.judge_verdict, None);
    assert_eq!(row.judge_reason, None);
    assert_eq!(judge_request_count(&port), 0);
    assert_eq!(port.requests().len(), 1);
    assert!(log_rows(&db).is_empty());
}
