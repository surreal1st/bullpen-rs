//! S2-04 acceptance: routing a chat turn to the ladder's reason rung before
//! the bot's own model sees it, `escalate` climbing one rung mid-turn (and
//! refusing off a scheduled trigger), and `interject` draining into a user
//! turn between model calls. Drives `RunManager` directly, same posture as
//! `tests/runs.rs`/`tests/tools.rs` - these are run-manager behaviours, not
//! routes.
//!
//! Kept out of `tests/runs.rs` (which predates S2-04) rather than grown
//! into it, per the ticket.

mod common;

use std::sync::{Arc, Mutex};
use std::time::Duration;

use common::{ScriptedPort, as_port, drain, own_conversation, seed_bot, seed_user_message};
use model::ladder::Trigger;
use model::{
    EventStream, MessageContent, ModelEvent, ModelMessage, ModelPort, ModelRequest, ModelUsage,
    ToolCall,
};
use server::runs::{RunEvent, RunManager, StartOptions};
use store::Db;

fn open_db() -> Arc<Mutex<Db>> {
    Arc::new(Mutex::new(Db::open(":memory:").expect("open :memory: db")))
}

/// Disables the routing classifier on `db` - every test but the routing one
/// itself needs this, or a scripted turn's first reply is consumed by the
/// classifier's own call instead of the turn it was scripted for.
fn disable_routing(db: &Arc<Mutex<Db>>) {
    let db = db.lock().expect("db mutex poisoned");
    model::routing::set_routing_settings(&db, Some(false), None)
        .expect("disable routing classifier");
}

fn run_row(db: &Arc<Mutex<Db>>, run_id: &str) -> (String, String, f64) {
    let db = db.lock().expect("db mutex poisoned");
    db.conn()
        .query_row(
            "SELECT status, model, cost_usd FROM runs WHERE id = ?1",
            rusqlite::params![run_id],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
        )
        .expect("read run row")
}

fn tool_result<'a>(events: &'a [RunEvent], tool: &str) -> Option<&'a str> {
    events.iter().find_map(|e| match e {
        RunEvent::ToolResult { name, result } if name == tool => Some(result.as_str()),
        _ => None,
    })
}

fn notice_message(events: &[RunEvent]) -> Option<&str> {
    events.iter().find_map(|e| match e {
        RunEvent::Notice { message } => Some(message.as_str()),
        _ => None,
    })
}

fn tool_call_script(id: &str, name: &str, arguments: String) -> Vec<ModelEvent> {
    vec![ModelEvent::ToolCalls {
        calls: vec![ToolCall {
            id: id.to_string(),
            name: name.to_string(),
            arguments,
        }],
        usage: None,
    }]
}

fn text_script_with_usage(text: &str, model: &str, usage: ModelUsage) -> Vec<ModelEvent> {
    vec![
        ModelEvent::Delta {
            text: text.to_string(),
        },
        ModelEvent::Done {
            model: model.to_string(),
            usage: Some(usage),
            finish_reason: None,
        },
    ]
}

// 1. A chat turn that asks for real work gets routed to the ladder's reason
//    rung before the bot's own (weaker) model ever sees it: the subscriber
//    gets a "Routed to X: real work" notice, the run row's model reflects
//    the route, and the classifier's own cost is folded into the run's
//    total alongside the routed reply's.
#[tokio::test]
async fn routed_chat_turn_carries_the_notice_and_the_classifiers_cost() {
    let db = open_db(); // routing left at its default: enabled (S2-01).
    seed_bot(&db, "arthur", "Arthur");
    let conversation_id = own_conversation(&db, "arthur");
    seed_user_message(&db, &conversation_id, "write me a migration plan");

    let reason_model = model::ladder::DEFAULT_TIER1.reason;
    let classifier_usage = ModelUsage {
        cost_usd: 0.0007,
        input_tokens: 120,
        output_tokens: 1,
        cached_tokens: 0,
    };
    let reply_usage = ModelUsage {
        cost_usd: 0.0421,
        input_tokens: 900,
        output_tokens: 220,
        cached_tokens: 0,
    };

    let port = ScriptedPort::new(vec![
        text_script_with_usage("work", model::CHEAP_DEFAULT_MODEL, classifier_usage.clone()),
        text_script_with_usage("Here is the plan.", reason_model, reply_usage.clone()),
    ]);

    let manager = Arc::new(RunManager::new(Arc::clone(&db), as_port(port)));
    let run_id = manager.start(StartOptions {
        bot_id: "arthur".to_string(),
        conversation_id: conversation_id.clone(),
        model: "cheap/unconfigured-model".to_string(),
        messages: vec![ModelMessage::user("write me a migration plan")],
        trigger: Trigger::Chat,
        room: false,
    });

    let events = drain(manager.subscribe(&run_id)).await;

    assert_eq!(
        notice_message(&events),
        Some(format!("Routed to {reason_model}: real work").as_str()),
        "expected a routing notice naming {reason_model}, got {events:?}"
    );

    let (status, model_after, cost) = run_row(&db, &run_id);
    assert_eq!(status, "done");
    assert_eq!(
        model_after, reason_model,
        "expected the run row's model to carry the route"
    );
    let expected_cost = classifier_usage.cost_usd + reply_usage.cost_usd;
    assert!(
        (cost - expected_cost).abs() < 1e-9,
        "expected the classifier's {} folded into the routed reply's {} = {expected_cost}, got {cost}",
        classifier_usage.cost_usd,
        reply_usage.cost_usd
    );
}

// F3: a routed chat turn leaves a `routing_log` row behind it, not just the
//    notice and the run row's model - the settings card's "Last 20 routings"
//    list reads this table directly and was permanently empty before this
//    fix even though the routing feature itself worked (drive's hand-rolled
//    routing block called `classify_turn` directly and never wrote the log).
#[tokio::test]
async fn a_routed_chat_turn_leaves_a_routing_log_row() {
    let db = open_db(); // routing left at its default: enabled (S2-01).
    seed_bot(&db, "arthur", "Arthur");
    let conversation_id = own_conversation(&db, "arthur");
    seed_user_message(&db, &conversation_id, "write me a migration plan");

    let reason_model = model::ladder::DEFAULT_TIER1.reason;
    let port = ScriptedPort::new(vec![
        text_script_with_usage(
            "work",
            model::CHEAP_DEFAULT_MODEL,
            ModelUsage {
                cost_usd: 0.0007,
                input_tokens: 120,
                output_tokens: 1,
                cached_tokens: 0,
            },
        ),
        text_script_with_usage(
            "Here is the plan.",
            reason_model,
            ModelUsage {
                cost_usd: 0.0421,
                input_tokens: 900,
                output_tokens: 220,
                cached_tokens: 0,
            },
        ),
    ]);

    let manager = Arc::new(RunManager::new(Arc::clone(&db), as_port(port)));
    let run_id = manager.start(StartOptions {
        bot_id: "arthur".to_string(),
        conversation_id: conversation_id.clone(),
        model: "cheap/unconfigured-model".to_string(),
        messages: vec![ModelMessage::user("write me a migration plan")],
        trigger: Trigger::Chat,
        room: false,
    });

    drain(manager.subscribe(&run_id)).await;

    let log = {
        let db = db.lock().expect("db mutex poisoned");
        model::routing::list_routing_log(&db, 20).expect("read routing log")
    };
    assert_eq!(
        log.len(),
        1,
        "expected the routed turn to leave exactly one routing_log row, got {log:?}"
    );
    assert_eq!(log[0].verdict, "work");
    assert_eq!(log[0].model, reason_model);
}

// 2. `escalate({kind:"code"})` from an unconfigured (tier-0) model climbs to
//    that kind's tier-1 model: a notice names it, the run row's model
//    carries the climb, and - proving the SAME run keeps going on it rather
//    than merely reporting it - the very next model call is made against
//    the climbed model.
#[tokio::test]
async fn escalate_climbs_from_cheap_to_the_kinds_tier1_model() {
    let db = open_db();
    disable_routing(&db);
    seed_bot(&db, "arthur", "Arthur");
    let conversation_id = own_conversation(&db, "arthur");
    seed_user_message(&db, &conversation_id, "fix this build error");

    let code_model = model::ladder::DEFAULT_TIER1.code;
    let args = serde_json::json!({"reason": "can't work out this linker error", "kind": "code"})
        .to_string();
    let port = ScriptedPort::new(vec![
        tool_call_script("c1", "escalate", args),
        text_script_with_usage(
            "Found it - undefined symbol.",
            code_model,
            ModelUsage {
                cost_usd: 0.01,
                input_tokens: 500,
                output_tokens: 60,
                cached_tokens: 0,
            },
        ),
    ]);

    let manager = Arc::new(RunManager::new(Arc::clone(&db), as_port(port)));
    let run_id = manager.start(StartOptions {
        bot_id: "arthur".to_string(),
        conversation_id: conversation_id.clone(),
        model: "test/unconfigured-model".to_string(),
        messages: vec![ModelMessage::user("fix this build error")],
        trigger: Trigger::Chat,
        room: false,
    });

    let events = drain(manager.subscribe(&run_id)).await;

    let notice = notice_message(&events).expect("expected an escalation notice");
    assert!(
        notice.starts_with("Escalating:") && notice.contains(code_model),
        "expected a notice naming the climb to {code_model}, got {notice:?}"
    );

    let escalate_result = tool_result(&events, "escalate").expect("expected an escalate result");
    assert!(
        escalate_result.starts_with("Escalating."),
        "expected the escalate tool's own reply, got {escalate_result:?}"
    );

    let (status, model_after, _) = run_row(&db, &run_id);
    assert_eq!(status, "done");
    assert_eq!(
        model_after, code_model,
        "expected the run row's model to carry the climb"
    );
}

// 3. `escalate` on a routine (never a chat) refuses instead of climbing: the
//    tool result is the refusal text, no notice is emitted, and the run's
//    model is untouched. Bite: drop the `may_escalate` check in
//    `tools/escalate.rs::run` and this goes red - the tool climbs instead of
//    refusing.
#[tokio::test]
async fn escalate_refuses_on_a_routine_trigger() {
    let db = open_db();
    disable_routing(&db);
    seed_bot(&db, "arthur", "Arthur");
    let conversation_id = own_conversation(&db, "arthur");

    let args = serde_json::json!({"reason": "stuck", "kind": "code"}).to_string();
    let port = ScriptedPort::new(vec![
        tool_call_script("c1", "escalate", args),
        text_script_with_usage(
            "Understood, stopping here.",
            "test/model",
            ModelUsage {
                cost_usd: 0.001,
                input_tokens: 40,
                output_tokens: 8,
                cached_tokens: 0,
            },
        ),
    ]);

    let manager = Arc::new(RunManager::new(Arc::clone(&db), as_port(port)));
    let run_id = manager.start(StartOptions {
        bot_id: "arthur".to_string(),
        conversation_id,
        model: "test/model".to_string(),
        messages: vec![ModelMessage::user("run the nightly checklist")],
        trigger: Trigger::Routine,
        room: false,
    });

    let events = drain(manager.subscribe(&run_id)).await;

    let escalate_result = tool_result(&events, "escalate").expect("expected an escalate result");
    assert!(
        escalate_result.contains("not available on a scheduled run"),
        "expected the escalation refusal, got {escalate_result:?}"
    );
    assert_eq!(
        notice_message(&events),
        None,
        "a refused escalation must not emit a climb notice"
    );

    let (status, model_after, _) = run_row(&db, &run_id);
    assert_eq!(status, "done");
    assert_eq!(
        model_after, "test/model",
        "a refused escalation must not change the run's model"
    );
}

/// Gates the FIRST model call behind `gate` (so a test can act while the run
/// is provably suspended mid-turn, between `take_interjections` for step 1
/// and the one for step 2) and logs every request sent, in order - what
/// `ScriptedPort` does not expose but this test needs, to prove an
/// interjection lands in step 2's request and NOT step 1's.
struct GatedLoggingPort {
    turn: Mutex<usize>,
    gate: Mutex<Option<tokio::sync::oneshot::Receiver<()>>>,
    requests: Mutex<Vec<ModelRequest>>,
}

impl ModelPort for GatedLoggingPort {
    fn stream(&self, request: ModelRequest) -> EventStream {
        self.requests
            .lock()
            .expect("requests mutex poisoned")
            .push(request);
        let mut turn = self.turn.lock().expect("turn mutex poisoned");
        *turn += 1;
        let n = *turn;
        drop(turn);

        if n == 1 {
            let rx = self.gate.lock().expect("gate mutex poisoned").take();
            Box::pin(async_stream::stream! {
                if let Some(rx) = rx { let _ = rx.await; }
                yield ModelEvent::ToolCalls {
                    calls: vec![ToolCall {
                        id: "c1".to_string(),
                        name: "say".to_string(),
                        arguments: "{\"text\":\"Checking.\"}".to_string(),
                    }],
                    usage: None,
                };
            })
        } else {
            Box::pin(futures::stream::iter(vec![
                ModelEvent::Delta {
                    text: "All good.".to_string(),
                },
                ModelEvent::Done {
                    model: "test/model".to_string(),
                    usage: None,
                    finish_reason: None,
                },
            ]))
        }
    }
}

// 4. Text sent to a still-running turn is drained into a user turn between
//    model calls: absent from the request already built for step 1, present
//    (with the interject wrapper, as a `user` turn) in step 2's.
#[tokio::test]
async fn interject_lands_as_a_user_turn_before_the_next_model_call() {
    let db = open_db();
    disable_routing(&db);
    seed_bot(&db, "arthur", "Arthur");
    let conversation_id = own_conversation(&db, "arthur");
    seed_user_message(&db, &conversation_id, "check on the deploy");

    let (gate_tx, gate_rx) = tokio::sync::oneshot::channel();
    let port = Arc::new(GatedLoggingPort {
        turn: Mutex::new(0),
        gate: Mutex::new(Some(gate_rx)),
        requests: Mutex::new(Vec::new()),
    });
    let dyn_port: Arc<dyn ModelPort> = port.clone();

    let manager = Arc::new(RunManager::new(Arc::clone(&db), dyn_port));
    let run_id = manager.start(StartOptions {
        bot_id: "arthur".to_string(),
        conversation_id,
        model: "test/model".to_string(),
        messages: vec![ModelMessage::user("check on the deploy")],
        trigger: Trigger::Chat,
        room: false,
    });

    // Give the spawned task a chance to build step 1's request and suspend
    // on the still-closed gate before interjecting - same technique
    // `tests/runs.rs`'s `stop_between_steps_...` uses, and for the same
    // reason: interjecting before the task is ever polled would land in
    // step 1's own request instead of proving the BETWEEN-calls drain.
    tokio::time::sleep(Duration::from_millis(10)).await;
    assert!(
        manager.interject(&run_id, "any word on the deploy?"),
        "expected interject to accept a still-running run"
    );
    let _ = gate_tx.send(());

    let events = drain(manager.subscribe(&run_id)).await;
    assert!(
        matches!(events.last(), Some(RunEvent::Done { .. })),
        "expected the run to finish, got {events:?}"
    );

    let requests = port
        .requests
        .lock()
        .expect("requests mutex poisoned")
        .clone();
    assert_eq!(
        requests.len(),
        2,
        "expected exactly two model calls, got {}",
        requests.len()
    );

    let has_interjection = |req: &ModelRequest| {
        req.messages.iter().any(|m| match &m.content {
            MessageContent::Text(t) => t.contains("any word on the deploy?"),
            MessageContent::Parts(_) => false,
        })
    };
    assert!(
        !has_interjection(&requests[0]),
        "interjection queued after step 1's request was built must not appear in it"
    );

    let interjected_turn = requests[1].messages.iter().find(|m| match &m.content {
        MessageContent::Text(t) => t.contains("any word on the deploy?"),
        MessageContent::Parts(_) => false,
    });
    let interjected_turn =
        interjected_turn.expect("expected the interjection to land in step 2's request");
    assert_eq!(
        interjected_turn.role, "user",
        "the interjection must land as a user turn"
    );
    if let MessageContent::Text(text) = &interjected_turn.content {
        assert!(
            text.starts_with("Josh, while you were working:"),
            "expected the interject wrapper text, got {text:?}"
        );
    }
}
