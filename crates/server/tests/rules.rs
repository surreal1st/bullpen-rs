//! S2-07 acceptance: auto-review rules. Port of the relevant cases in
//! `test/rules.test.ts` - a rule is only ever consulted when the grid
//! already said "ask"; it can turn that into an allow (skipping the
//! approval and running the tool), leave it asking, or deny it outright
//! with the same wording the grid's own deny uses; a rule may never lift a
//! tool an unattended run is not allowed to hold on "allow" at all; a
//! classifier that errors fails OPEN to "ask", never to "allow"; and the
//! approval card's "Always allow" / "Never" buttons (`remember`) write both
//! the grid override (existing H4 behaviour) AND a rule in the words of the
//! actual call.
//!
//! The first half drives `RunManager` directly, matching `tests/approvals.rs`'s
//! own posture. The second half (`remember`, and the `/api/auto-review/rules`
//! CRUD) goes through `build_app` over HTTP, since those are the routes this
//! ticket owns.

mod common;

use std::sync::{Arc, Mutex};
use std::time::Duration;

use axum::Router;
use axum::body::Body;
use axum::http::{Request, StatusCode};
use common::{as_port, seed_session};
use model::ladder::Trigger;
use model::{
    EventStream, MessageContent, ModelEvent, ModelMessage, ModelPort, ModelRequest, ToolCall,
};
use serde_json::{Value, json};
use server::rules::{self, RuleBehavior};
use server::runs::{RunEvent, RunManager, StartOptions};
use server::{AppState, build_app};
use store::Db;
use tower::ServiceExt;

// ---- shared fixtures ----

fn open_db() -> Arc<Mutex<Db>> {
    let db = Db::open(":memory:").expect("open :memory: db");
    model::routing::set_routing_settings(&db, Some(false), None)
        .expect("disable routing classifier for scripted-model tests");
    Arc::new(Mutex::new(db))
}

fn seed_bot(db: &Arc<Mutex<Db>>, id: &str, name: &str) {
    let db = db.lock().expect("db mutex poisoned");
    // OR IGNORE: several tests seed a rule against a bot id up front (so
    // `seed_rule`'s classifier reply can be scripted before the run
    // exists) and then call `start_shell_run`, which seeds the same bot
    // again - idempotent here rather than making every caller track which
    // helper already did it.
    db.conn()
        .execute(
            "INSERT OR IGNORE INTO bots (id, name, purpose, instructions, model, created_at) VALUES (?1, ?2, '', ?3, NULL, '2026-01-01T00:00:00Z')",
            rusqlite::params![id, name, format!("You are {name}.")],
        )
        .expect("seed bot");
}

fn own_conversation(db: &Arc<Mutex<Db>>, bot_id: &str) -> String {
    let db = db.lock().expect("db mutex poisoned");
    store::get_or_create_conversation(&db, bot_id).expect("get_or_create_conversation")
}

fn seed_user_message(db: &Arc<Mutex<Db>>, conversation_id: &str, text: &str) {
    let db = db.lock().expect("db mutex poisoned");
    store::append_message(
        &db,
        conversation_id,
        "user",
        text,
        store::NewMessage::default(),
    )
    .expect("append user message");
}

/// Seeds a rule through the real `add_rule` (so the table gets created the
/// same self-creating way production hits it) and hands back its generated
/// id - a `RulesPort`'s classify reply names ids, so tests build the rule
/// first and script the classifier's answer around the real id rather than
/// a chosen one.
fn seed_rule(db: &Arc<Mutex<Db>>, bot_id: &str, text: &str, behavior: RuleBehavior) -> String {
    let db = db.lock().expect("db mutex poisoned");
    rules::add_rule(&db, Some(bot_id.to_string()), text, behavior)
        .expect("seed rule")
        .id
}

fn rule_hits(db: &Arc<Mutex<Db>>, id: &str) -> i64 {
    let db = db.lock().expect("db mutex poisoned");
    rules::list_rules_for(&db, "arthur")
        .expect("list rules")
        .into_iter()
        .find(|r| r.id == id)
        .expect("rule still exists")
        .hits
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

fn pending_approval(db: &Arc<Mutex<Db>>, run_id: &str) -> Option<(String, String, String, String)> {
    let db = db.lock().expect("db mutex poisoned");
    let mut stmt = db
        .conn()
        .prepare("SELECT id, tool_name, tool_args, status FROM approvals WHERE run_id = ?1")
        .expect("prepare");
    let rows: Vec<(String, String, String, String)> = stmt
        .query_map(rusqlite::params![run_id], |row| {
            Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?))
        })
        .expect("query")
        .collect::<Result<_, _>>()
        .expect("collect");
    assert!(
        rows.len() <= 1,
        "expected at most one approval row, got {rows:?}"
    );
    rows.into_iter().next()
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

fn start_shell_run(manager: &Arc<RunManager>, db: &Arc<Mutex<Db>>, trigger: Trigger) -> String {
    seed_bot(db, "arthur", "Arthur");
    let conversation_id = own_conversation(db, "arthur");
    seed_user_message(db, &conversation_id, "clean up");
    manager.start(StartOptions {
        bot_id: "arthur".to_string(),
        conversation_id,
        model: "test/model".to_string(),
        messages: vec![ModelMessage::user("clean up")],
        trigger,
        room: false,
    })
}

// ---- RulesPort: plays both roles a real port does here - the bot's own
// conversation (`turn_script`), and the rules engine's classification call
// (`rules::classify`'s own request, always a system message containing
// "plain-language rules") - routing on that marker is what lets one fake
// stand in for the SAME port object both `run_turn`'s model calls and its
// rules consultation use. Ported from `test/rules.test.ts`'s own
// `withRules`. ----

enum ClassifyBehavior {
    Reply(Vec<String>),
    Error,
}

struct RulesPort {
    turn: Mutex<usize>,
    turn_script: Box<dyn Fn(usize) -> Vec<ModelEvent> + Send + Sync>,
    classify: ClassifyBehavior,
    classify_calls: Mutex<Vec<ModelRequest>>,
}

impl RulesPort {
    fn new(
        turn_script: impl Fn(usize) -> Vec<ModelEvent> + Send + Sync + 'static,
        classify: ClassifyBehavior,
    ) -> Self {
        Self {
            turn: Mutex::new(0),
            turn_script: Box::new(turn_script),
            classify,
            classify_calls: Mutex::new(Vec::new()),
        }
    }

    fn classify_call_count(&self) -> usize {
        self.classify_calls
            .lock()
            .expect("classify log poisoned")
            .len()
    }

    /// The most recent request sent to the classifier - what F6's test
    /// inspects to prove a huge tool argument reached it truncated and
    /// fenced, rather than verbatim.
    fn last_classify_request(&self) -> ModelRequest {
        self.classify_calls
            .lock()
            .expect("classify log poisoned")
            .last()
            .cloned()
            .expect("classify was never called")
    }
}

fn message_text(message: &ModelMessage) -> &str {
    match &message.content {
        MessageContent::Text(t) => t,
        other => panic!("expected text content, got {other:?}"),
    }
}

fn is_classify_request(request: &ModelRequest) -> bool {
    request
        .messages
        .first()
        .map(
            |m| matches!(&m.content, MessageContent::Text(t) if t.contains("plain-language rules")),
        )
        .unwrap_or(false)
}

impl ModelPort for RulesPort {
    fn stream(&self, request: ModelRequest) -> EventStream {
        if is_classify_request(&request) {
            self.classify_calls
                .lock()
                .expect("classify log poisoned")
                .push(request);
            let events = match &self.classify {
                ClassifyBehavior::Reply(ids) => vec![
                    ModelEvent::Delta {
                        text: serde_json::to_string(ids).expect("serialize ids"),
                    },
                    ModelEvent::Done {
                        model: "test/classifier".to_string(),
                        usage: None,
                        finish_reason: None,
                    },
                ],
                ClassifyBehavior::Error => vec![ModelEvent::Error {
                    message: "classifier exploded".to_string(),
                    status: None,
                }],
            };
            return Box::pin(futures::stream::iter(events));
        }

        let mut turn = self.turn.lock().expect("turn mutex poisoned");
        *turn += 1;
        let n = *turn;
        drop(turn);
        Box::pin(futures::stream::iter((self.turn_script)(n)))
    }
}

/// Asks for a shell call once, then answers - repeatable so a second send
/// re-asks. Matches `test/rules.test.ts`'s own `shellThenAnswer`.
fn shell_then_answer(
    command: &'static str,
    answer: &'static str,
) -> impl Fn(usize) -> Vec<ModelEvent> {
    move |turn| {
        if turn % 2 == 1 {
            vec![ModelEvent::ToolCalls {
                calls: vec![ToolCall {
                    id: format!("call-{turn}"),
                    name: "shell".to_string(),
                    arguments: json!({ "command": command }).to_string(),
                }],
                usage: None,
            }]
        } else {
            vec![
                ModelEvent::Delta {
                    text: answer.to_string(),
                },
                ModelEvent::Done {
                    model: "test/model".to_string(),
                    usage: None,
                    finish_reason: None,
                },
            ]
        }
    }
}

// ---- 1. allow: skips the approval and runs the tool, and records a hit ----

#[tokio::test]
async fn a_matching_allow_rule_skips_the_approval_runs_the_tool_and_records_a_hit() {
    let db = open_db();
    seed_bot(&db, "arthur", "Arthur");
    let rule_id = seed_rule(
        &db,
        "arthur",
        "clean up temp files for me",
        RuleBehavior::Allow,
    );

    let conversation_id = own_conversation(&db, "arthur");
    seed_user_message(&db, &conversation_id, "clean up");
    let port = Arc::new(RulesPort::new(
        shell_then_answer("find /work -name '*.tmp' -delete", "Done."),
        ClassifyBehavior::Reply(vec![rule_id.clone()]),
    ));
    let manager = Arc::new(RunManager::new(Arc::clone(&db), port));
    let run_id = manager.start(StartOptions {
        bot_id: "arthur".to_string(),
        conversation_id: conversation_id.clone(),
        model: "test/model".to_string(),
        messages: vec![ModelMessage::user("clean up")],
        trigger: Trigger::Chat,
        room: false,
    });

    drain_until_paused_or_done(manager.subscribe(&run_id)).await;
    assert_eq!(wait_for_status(&db, &run_id, "done").await, "done");

    assert!(
        pending_approval(&db, &run_id).is_none(),
        "an allow rule must skip the approval entirely"
    );
    let messages_json = run_messages_json(&db, &run_id);
    assert!(
        messages_json.contains("find /work -name"),
        "expected the tool to actually run, got {messages_json}"
    );
    assert_eq!(rule_hits(&db, &rule_id), 1);
}

// ---- 2. no rules: never calls the classifier, and still asks ----

#[tokio::test]
async fn no_rules_never_calls_the_classifier_and_still_asks() {
    let db = open_db();
    let port = Arc::new(RulesPort::new(
        shell_then_answer("find /work -name '*.tmp' -delete", "Done."),
        ClassifyBehavior::Reply(vec![]),
    ));
    let manager = Arc::new(RunManager::new(Arc::clone(&db), port.clone()));
    let run_id = start_shell_run(&manager, &db, Trigger::Chat);

    drain_until_paused_or_done(manager.subscribe(&run_id)).await;
    assert_eq!(wait_for_status(&db, &run_id, "waiting").await, "waiting");

    let (_, tool_name, _, status) =
        pending_approval(&db, &run_id).expect("still asks with no rules");
    assert_eq!(tool_name, "shell");
    assert_eq!(status, "pending");
    assert_eq!(port.classify_call_count(), 0);
}

// ---- 3. ask beats allow when both match ----

#[tokio::test]
async fn ask_beats_allow_when_both_match() {
    let db = open_db();
    let allow_id = seed_rule(&db, "arthur", "clean up temp files", RuleBehavior::Allow);
    let ask_id = seed_rule(
        &db,
        "arthur",
        "run any destructive command",
        RuleBehavior::Ask,
    );

    let port = Arc::new(RulesPort::new(
        shell_then_answer("find /work -name '*.tmp' -delete", "Done."),
        ClassifyBehavior::Reply(vec![allow_id, ask_id]),
    ));
    let manager = Arc::new(RunManager::new(Arc::clone(&db), port));
    let run_id = start_shell_run(&manager, &db, Trigger::Chat);

    drain_until_paused_or_done(manager.subscribe(&run_id)).await;
    assert_eq!(wait_for_status(&db, &run_id, "waiting").await, "waiting");

    let (_, tool_name, _, status) = pending_approval(&db, &run_id).expect("ask still parks it");
    assert_eq!(tool_name, "shell");
    assert_eq!(status, "pending");
}

// ---- 4. never beats both allow and ask, and denies in the same words the
// grid's own deny uses ----

#[tokio::test]
async fn never_beats_both_and_denies_in_the_grids_own_words() {
    let db = open_db();
    let allow_id = seed_rule(&db, "arthur", "clean up temp files", RuleBehavior::Allow);
    let ask_id = seed_rule(
        &db,
        "arthur",
        "run any destructive command",
        RuleBehavior::Ask,
    );
    let never_id = seed_rule(
        &db,
        "arthur",
        "delete files with find -delete",
        RuleBehavior::Never,
    );

    let port = Arc::new(RulesPort::new(
        shell_then_answer(
            "find /work -name '*.tmp' -delete",
            "I did not delete anything.",
        ),
        ClassifyBehavior::Reply(vec![allow_id, ask_id, never_id.clone()]),
    ));
    let manager = Arc::new(RunManager::new(Arc::clone(&db), port));
    let run_id = start_shell_run(&manager, &db, Trigger::Chat);

    drain_until_paused_or_done(manager.subscribe(&run_id)).await;
    assert_eq!(wait_for_status(&db, &run_id, "done").await, "done");

    assert!(
        pending_approval(&db, &run_id).is_none(),
        "denied, not parked"
    );
    let messages_json = run_messages_json(&db, &run_id);
    let messages: Vec<ModelMessage> =
        serde_json::from_str(&messages_json).expect("parse stored messages");
    let tool_result = messages
        .iter()
        .find(|m| m.role == "tool")
        .expect("a tool result message");
    let MessageContent::Text(text) = &tool_result.content else {
        panic!("expected text content");
    };
    // Same wording the grid's own deny uses - `run.ts`/`runs.rs`'s
    // `Decision::Deny` arm, not a rule-specific message.
    assert!(
        text.contains("Not allowed"),
        "expected the grid's own deny text, got {text:?}"
    );
    assert_eq!(rule_hits(&db, &never_id), 1);
}

// ---- 5. deny in the grid is a floor rules cannot lift ----

#[tokio::test]
async fn grid_deny_is_a_floor_rules_cannot_lift() {
    let db = open_db();
    seed_bot(&db, "arthur", "Arthur");
    seed_rule(&db, "arthur", "clean up temp files", RuleBehavior::Allow);
    {
        let guard = db.lock().expect("db mutex poisoned");
        let current = server::permissions::get_permissions(&guard, "arthur").expect("get perms");
        server::permissions::set_permissions(&guard, "arthur", &{
            let mut m = current;
            m.insert("shell".to_string(), server::permissions::Decision::Deny);
            m
        })
        .expect("set perms");
    }

    let port = Arc::new(RulesPort::new(
        shell_then_answer("find /work -name '*.tmp' -delete", "Done."),
        ClassifyBehavior::Reply(vec!["should-never-be-used".to_string()]),
    ));
    let manager = Arc::new(RunManager::new(Arc::clone(&db), port.clone()));
    let run_id = start_shell_run(&manager, &db, Trigger::Chat);

    drain_until_paused_or_done(manager.subscribe(&run_id)).await;
    assert_eq!(wait_for_status(&db, &run_id, "done").await, "done");

    assert!(pending_approval(&db, &run_id).is_none());
    assert_eq!(
        port.classify_call_count(),
        0,
        "a grid deny must never even ask the classifier"
    );
}

// ---- 6. a rule cannot lift a tool an unattended run is not allowed at all ----

#[tokio::test]
async fn a_rule_cannot_lift_shell_for_an_unattended_routine() {
    let db = open_db();
    let allow_id = seed_rule(&db, "arthur", "clean up temp files", RuleBehavior::Allow);

    let port = Arc::new(RulesPort::new(
        shell_then_answer("find /work -name '*.tmp' -delete", "Done."),
        ClassifyBehavior::Reply(vec![allow_id]),
    ));
    let manager = Arc::new(RunManager::new(Arc::clone(&db), port));
    let run_id = start_shell_run(&manager, &db, Trigger::Routine);

    drain_until_paused_or_done(manager.subscribe(&run_id)).await;
    assert_eq!(wait_for_status(&db, &run_id, "waiting").await, "waiting");

    let (_, tool_name, _, status) =
        pending_approval(&db, &run_id).expect("still parked - the rule's allow was not permitted");
    assert_eq!(tool_name, "shell");
    assert_eq!(status, "pending");
}

// ---- 7. classifier error fails OPEN to "ask", never to "allow" - THE BITE ----

#[tokio::test]
async fn a_classifier_error_still_parks_the_run() {
    let db = open_db();
    // A rule that WOULD have matched and allowed it, if the classifier had
    // ever answered - proving the error path, not just "no rules".
    seed_rule(
        &db,
        "arthur",
        "clean up temp files for me",
        RuleBehavior::Allow,
    );

    let port = Arc::new(RulesPort::new(
        shell_then_answer("find /work -name '*.tmp' -delete", "Done."),
        ClassifyBehavior::Error,
    ));
    let manager = Arc::new(RunManager::new(Arc::clone(&db), port));
    let run_id = start_shell_run(&manager, &db, Trigger::Chat);

    drain_until_paused_or_done(manager.subscribe(&run_id)).await;
    assert_eq!(wait_for_status(&db, &run_id, "waiting").await, "waiting");

    let (_, tool_name, _, status) = pending_approval(&db, &run_id)
        .expect("a classifier error must fail open to ask, not run the tool unsupervised");
    assert_eq!(tool_name, "shell");
    assert_eq!(status, "pending");
}

// ---- HTTP: remember, and /api/auto-review/rules CRUD ----

fn app_for(db: Db, port: Arc<dyn model::ModelPort>) -> Router {
    build_app(AppState::with_port(db, port))
}

fn open_db_plain() -> Db {
    let db = Db::open(":memory:").expect("open :memory: db");
    model::routing::set_routing_settings(&db, Some(false), None)
        .expect("disable routing classifier for scripted-model tests");
    db
}

fn seed_bot_plain(db: &Db, id: &str, name: &str) {
    db.conn()
        .execute(
            "INSERT INTO bots (id, name, purpose, instructions, model, created_at) VALUES (?1, ?2, '', ?3, NULL, '2026-01-01T00:00:00Z')",
            rusqlite::params![id, name, format!("You are {name}.")],
        )
        .expect("seed bot");
}

async fn get_json(app: &Router, uri: &str, cookie: &str) -> (StatusCode, Value) {
    let resp = app
        .clone()
        .oneshot(
            Request::get(uri)
                .header("cookie", cookie)
                .body(Body::empty())
                .expect("build request"),
        )
        .await
        .expect("oneshot");
    let status = resp.status();
    let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
        .await
        .expect("collect");
    let json = if bytes.is_empty() {
        Value::Null
    } else {
        serde_json::from_slice(&bytes).expect("response body is JSON")
    };
    (status, json)
}

async fn send_json(
    app: &Router,
    method: &str,
    uri: &str,
    body: Value,
    cookie: &str,
) -> (StatusCode, Value) {
    let builder = match method {
        "POST" => Request::post(uri),
        "PUT" => Request::put(uri),
        "DELETE" => Request::delete(uri),
        other => panic!("unsupported method {other}"),
    };
    let resp = app
        .clone()
        .oneshot(
            builder
                .header("content-type", "application/json")
                .header("cookie", cookie)
                .body(Body::from(body.to_string()))
                .expect("build request"),
        )
        .await
        .expect("oneshot");
    let status = resp.status();
    let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
        .await
        .expect("collect");
    let json = if bytes.is_empty() {
        Value::Null
    } else {
        serde_json::from_slice(&bytes).expect("response body is JSON")
    };
    (status, json)
}

async fn post_message_fire_and_forget(app: &Router, bot_id: &str, text: &str, cookie: &str) {
    let resp = app
        .clone()
        .oneshot(
            Request::post(format!("/api/bots/{bot_id}/messages"))
                .header("content-type", "application/json")
                .header("cookie", cookie)
                .body(Body::from(json!({"text": text}).to_string()))
                .expect("build request"),
        )
        .await
        .expect("oneshot");
    assert_eq!(resp.status(), StatusCode::OK);
}

async fn wait_for_one_pending(app: &Router, cookie: &str) -> Value {
    for _ in 0..300 {
        let (status, body) = get_json(app, "/api/approvals", cookie).await;
        assert_eq!(status, StatusCode::OK);
        let approvals = body["approvals"].as_array().cloned().unwrap_or_default();
        if approvals.len() == 1 {
            return approvals[0].clone();
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    panic!("no approval showed up in time");
}

/// A `shell` call once, then a final answer, as an HTTP-driven `ScriptedPort`.
fn shell_then_answer_http(answer: &str) -> common::ScriptedPort {
    common::ScriptedPort::new(vec![
        vec![ModelEvent::ToolCalls {
            calls: vec![ToolCall {
                id: "call-1".to_string(),
                name: "shell".to_string(),
                arguments: "{\"command\":\"rm -rf /work/x\"}".to_string(),
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

// ---- 8. 'Always allow' writes both the permission override and a rule ----

#[tokio::test]
async fn remember_allow_writes_the_permission_override_and_a_rule() {
    let db = open_db_plain();
    store::set_password(&db, "test-password").expect("set password");
    seed_bot_plain(&db, "arthur", "Arthur");
    let session = seed_session(&db);
    let app = app_for(db, as_port(shell_then_answer_http("Cleaned it up.")));

    post_message_fire_and_forget(&app, "arthur", "clean up", &session).await;
    let approval = wait_for_one_pending(&app, &session).await;
    let id = approval["id"].as_str().expect("approval id").to_string();

    let (status, body) = send_json(
        &app,
        "POST",
        &format!("/api/approvals/{id}"),
        json!({"approved": true, "remember": "allow"}),
        &session,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["ok"], true);
    assert_eq!(body["approved"], true);

    let (perm_status, perm_body) = get_json(&app, "/api/bots/arthur/permissions", &session).await;
    assert_eq!(perm_status, StatusCode::OK);
    assert_eq!(perm_body["permissions"]["shell"], "allow");

    let (rules_status, rules_body) =
        get_json(&app, "/api/auto-review/rules?botId=arthur", &session).await;
    assert_eq!(rules_status, StatusCode::OK);
    let rules = rules_body["rules"].as_array().expect("rules array");
    assert_eq!(rules.len(), 1);
    assert_eq!(rules[0]["behavior"], "allow");
    assert!(
        rules[0]["text"]
            .as_str()
            .unwrap_or_default()
            .contains("rm -rf /work/x"),
        "expected the rule text to name the actual command, got {:?}",
        rules[0]["text"]
    );
}

// ---- 9. 'Never' writes a never rule ----

#[tokio::test]
async fn remember_deny_writes_a_never_rule() {
    let db = open_db_plain();
    store::set_password(&db, "test-password").expect("set password");
    seed_bot_plain(&db, "arthur", "Arthur");
    let session = seed_session(&db);
    let app = app_for(db, as_port(shell_then_answer_http("Cleaned it up.")));

    post_message_fire_and_forget(&app, "arthur", "clean up", &session).await;
    let approval = wait_for_one_pending(&app, &session).await;
    let id = approval["id"].as_str().expect("approval id").to_string();

    let (status, _) = send_json(
        &app,
        "POST",
        &format!("/api/approvals/{id}"),
        json!({"approved": false, "remember": "deny"}),
        &session,
    )
    .await;
    assert_eq!(status, StatusCode::OK);

    let (perm_status, perm_body) = get_json(&app, "/api/bots/arthur/permissions", &session).await;
    assert_eq!(perm_status, StatusCode::OK);
    assert_eq!(perm_body["permissions"]["shell"], "deny");

    let (_, rules_body) = get_json(&app, "/api/auto-review/rules?botId=arthur", &session).await;
    let rules = rules_body["rules"].as_array().expect("rules array");
    assert_eq!(rules.len(), 1);
    assert_eq!(rules[0]["behavior"], "never");
}

// ---- 10. a question cannot be remembered ----

#[tokio::test]
async fn remembering_ask_josh_is_refused() {
    let db = open_db_plain();
    store::set_password(&db, "test-password").expect("set password");
    seed_bot_plain(&db, "arthur", "Arthur");
    let session = seed_session(&db);
    let port = common::ScriptedPort::new(vec![vec![ModelEvent::ToolCalls {
        calls: vec![ToolCall {
            id: "call-1".to_string(),
            name: "ask_josh".to_string(),
            arguments: json!({"question": "Which repo?", "wait": true}).to_string(),
        }],
        usage: None,
    }]]);
    let app = app_for(db, as_port(port));

    post_message_fire_and_forget(&app, "arthur", "which repo do you mean?", &session).await;
    let approval = wait_for_one_pending(&app, &session).await;
    let id = approval["id"].as_str().expect("approval id").to_string();
    assert_eq!(approval["toolName"], "ask_josh");

    let (status, body) = send_json(
        &app,
        "POST",
        &format!("/api/approvals/{id}"),
        json!({"approved": true, "remember": "allow"}),
        &session,
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert!(body["error"].as_str().is_some());
}

// ---- 11. CRUD round trip ----

#[tokio::test]
async fn auto_review_rules_crud_round_trips_over_http() {
    let db = open_db_plain();
    store::set_password(&db, "test-password").expect("set password");
    seed_bot_plain(&db, "arthur", "Arthur");
    let session = seed_session(&db);
    let app = app_for(db, as_port(shell_then_answer_http("x")));

    // Create, scoped to arthur.
    let (create_status, create_body) = send_json(
        &app,
        "POST",
        "/api/auto-review/rules",
        json!({"botId": "arthur", "text": "clean up temp files", "behavior": "allow"}),
        &session,
    )
    .await;
    assert_eq!(create_status, StatusCode::CREATED);
    let id = create_body["rule"]["id"]
        .as_str()
        .expect("created rule id")
        .to_string();
    assert_eq!(create_body["rule"]["behavior"], "allow");
    assert_eq!(create_body["rule"]["hits"], 0);

    // A global rule (no botId), for the "no bot in view" listing.
    let (global_status, _) = send_json(
        &app,
        "POST",
        "/api/auto-review/rules",
        json!({"text": "never touch prod", "behavior": "never"}),
        &session,
    )
    .await;
    assert_eq!(global_status, StatusCode::CREATED);

    // Listing arthur's rules includes both his own and the global one.
    let (list_status, list_body) =
        get_json(&app, "/api/auto-review/rules?botId=arthur", &session).await;
    assert_eq!(list_status, StatusCode::OK);
    assert_eq!(list_body["rules"].as_array().map(Vec::len), Some(2));

    // Listing with no botId gets only the global rule.
    let (global_list_status, global_list_body) =
        get_json(&app, "/api/auto-review/rules", &session).await;
    assert_eq!(global_list_status, StatusCode::OK);
    assert_eq!(global_list_body["rules"].as_array().map(Vec::len), Some(1));

    // Update.
    let (update_status, update_body) = send_json(
        &app,
        "PUT",
        &format!("/api/auto-review/rules/{id}"),
        json!({"behavior": "ask"}),
        &session,
    )
    .await;
    assert_eq!(update_status, StatusCode::OK);
    assert_eq!(update_body["rule"]["behavior"], "ask");
    assert_eq!(update_body["rule"]["text"], "clean up temp files");

    // Empty text is refused.
    let (bad_status, bad_body) = send_json(
        &app,
        "PUT",
        &format!("/api/auto-review/rules/{id}"),
        json!({"text": "   "}),
        &session,
    )
    .await;
    assert_eq!(bad_status, StatusCode::BAD_REQUEST);
    assert!(bad_body["error"].as_str().is_some());

    // Delete.
    let (delete_status, delete_body) = send_json(
        &app,
        "DELETE",
        &format!("/api/auto-review/rules/{id}"),
        Value::Null,
        &session,
    )
    .await;
    assert_eq!(delete_status, StatusCode::OK);
    assert_eq!(delete_body["ok"], true);

    // Gone.
    let (redelete_status, _) = send_json(
        &app,
        "DELETE",
        &format!("/api/auto-review/rules/{id}"),
        Value::Null,
        &session,
    )
    .await;
    assert_eq!(redelete_status, StatusCode::NOT_FOUND);

    // Unknown bot on create and on list.
    let (no_bot_create_status, _) = send_json(
        &app,
        "POST",
        "/api/auto-review/rules",
        json!({"botId": "ghost", "text": "x", "behavior": "allow"}),
        &session,
    )
    .await;
    assert_eq!(no_bot_create_status, StatusCode::NOT_FOUND);

    let (no_bot_list_status, _) =
        get_json(&app, "/api/auto-review/rules?botId=ghost", &session).await;
    assert_eq!(no_bot_list_status, StatusCode::NOT_FOUND);
}

// ---- 12. F6: a huge tool argument reaches the classifier capped and
// fenced, not verbatim - THE BITE ----

#[tokio::test]
async fn a_huge_command_argument_reaches_the_classifier_truncated_and_fenced() {
    let db = open_db();
    seed_bot(&db, "arthur", "Arthur");
    // A rule exists so `apply_rules`/`classify` is actually consulted -
    // "no rules means no model call" (test 2) covers the other branch.
    seed_rule(
        &db,
        "arthur",
        "clean up temp files for me",
        RuleBehavior::Allow,
    );
    let conversation_id = own_conversation(&db, "arthur");
    seed_user_message(&db, &conversation_id, "clean up");

    let big_command = "x".repeat(5000);
    let script_command = big_command.clone();
    let port = Arc::new(RulesPort::new(
        move |turn| {
            if turn % 2 == 1 {
                vec![ModelEvent::ToolCalls {
                    calls: vec![ToolCall {
                        id: format!("call-{turn}"),
                        name: "shell".to_string(),
                        arguments: json!({ "command": script_command }).to_string(),
                    }],
                    usage: None,
                }]
            } else {
                vec![
                    ModelEvent::Delta {
                        text: "Done.".to_string(),
                    },
                    ModelEvent::Done {
                        model: "test/model".to_string(),
                        usage: None,
                        finish_reason: None,
                    },
                ]
            }
        },
        // Doesn't match - irrelevant to this test, which only inspects
        // what was SENT to the classifier, not how it answered.
        ClassifyBehavior::Reply(vec![]),
    ));
    let manager = Arc::new(RunManager::new(Arc::clone(&db), port.clone()));
    let run_id = manager.start(StartOptions {
        bot_id: "arthur".to_string(),
        conversation_id: conversation_id.clone(),
        model: "test/model".to_string(),
        messages: vec![ModelMessage::user("clean up")],
        trigger: Trigger::Chat,
        room: false,
    });

    drain_until_paused_or_done(manager.subscribe(&run_id)).await;
    assert_eq!(wait_for_status(&db, &run_id, "waiting").await, "waiting");
    assert_eq!(port.classify_call_count(), 1);

    let request = port.last_classify_request();
    let user_text = message_text(&request.messages[1]);
    let system_text = message_text(&request.messages[0]);

    assert!(
        !user_text.contains(&big_command),
        "the full 5KB command must never reach the classifier, got {} bytes",
        user_text.len()
    );
    assert!(
        user_text.len() < 400,
        "expected the fenced, capped description to stay well under 400 bytes, got {} bytes",
        user_text.len()
    );
    assert!(
        user_text.starts_with("<<<PENDING_ACTION_DATA>>>")
            && user_text.contains("<<<END_PENDING_ACTION_DATA>>>"),
        "expected the pending action to be fenced, got {user_text:?}"
    );
    assert!(
        system_text.to_lowercase().contains("data")
            && system_text.contains("<<<PENDING_ACTION_DATA>>>"),
        "expected the system message to name the fence and call it data, got {system_text:?}"
    );
}

// ---- 13. F7: identical rule text for the same bot dedupes on insert,
// through both callers of the shared upsert - THE BITE ----

#[tokio::test]
async fn identical_rule_text_for_the_same_bot_is_deduped_on_insert() {
    let db = open_db();
    seed_bot(&db, "arthur", "Arthur");
    {
        let guard = db.lock().expect("db mutex poisoned");
        rules::upsert_rule(
            &guard,
            Some("arthur".to_string()),
            "clean up temp files",
            RuleBehavior::Allow,
        )
        .expect("first upsert");
        rules::upsert_rule(
            &guard,
            Some("arthur".to_string()),
            "clean up temp files",
            RuleBehavior::Never,
        )
        .expect("second upsert, same text, different behavior");
    }

    let rules = {
        let guard = db.lock().expect("db mutex poisoned");
        rules::list_rules_for(&guard, "arthur").expect("list rules")
    };
    assert_eq!(
        rules.len(),
        1,
        "two identical presses must write one rule, got {rules:?}"
    );
    assert_eq!(
        rules[0].behavior,
        RuleBehavior::Never,
        "the second press's behavior must win"
    );
}

#[tokio::test]
async fn two_identical_posts_to_the_rules_route_write_one_rule() {
    let db = open_db_plain();
    store::set_password(&db, "test-password").expect("set password");
    seed_bot_plain(&db, "arthur", "Arthur");
    let session = seed_session(&db);
    let app = app_for(db, as_port(shell_then_answer_http("x")));

    let body = json!({"botId": "arthur", "text": "clean up temp files", "behavior": "allow"});
    let (first_status, _) = send_json(
        &app,
        "POST",
        "/api/auto-review/rules",
        body.clone(),
        &session,
    )
    .await;
    assert_eq!(first_status, StatusCode::CREATED);
    let (second_status, _) =
        send_json(&app, "POST", "/api/auto-review/rules", body, &session).await;
    assert_eq!(second_status, StatusCode::CREATED);

    let (list_status, list_body) =
        get_json(&app, "/api/auto-review/rules?botId=arthur", &session).await;
    assert_eq!(list_status, StatusCode::OK);
    let rules = list_body["rules"].as_array().expect("rules array");
    assert_eq!(
        rules.len(),
        1,
        "two identical POSTs must write one rule, got {rules:?}"
    );
}

// ---- 14. F7: `list_rules_for` caps at the newest 40 rules - THE BITE ----

#[tokio::test]
async fn list_rules_for_caps_at_the_newest_forty() {
    let db = open_db();
    seed_bot(&db, "arthur", "Arthur");
    {
        let guard = db.lock().expect("db mutex poisoned");
        for i in 0..45 {
            rules::add_rule(
                &guard,
                Some("arthur".to_string()),
                &format!("rule number {i}"),
                RuleBehavior::Ask,
            )
            .unwrap_or_else(|_| panic!("seed rule {i}"));
            // `created_at` has millisecond resolution and these inserts can
            // land in the same millisecond - a strictly increasing rowid
            // (the tiebreak the cap orders by) is what actually keeps them
            // distinguishable, so no sleep is needed here.
        }
    }

    let rules = {
        let guard = db.lock().expect("db mutex poisoned");
        rules::list_rules_for(&guard, "arthur").expect("list rules")
    };
    assert_eq!(
        rules.len(),
        40,
        "expected the cap to hold at 40, got {}",
        rules.len()
    );
    assert_eq!(
        rules.last().expect("at least one rule").text,
        "rule number 44",
        "expected the newest rules to survive the cap"
    );
    assert_eq!(
        rules.first().expect("at least one rule").text,
        "rule number 5",
        "expected the oldest 5 to have been dropped by the cap"
    );
}
