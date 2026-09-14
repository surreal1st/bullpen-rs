//! S1-06 acceptance: `POST /api/rooms`, a message into a room driving the
//! round engine over HTTP, silence, `@everyone`, a narrowed `@mention`,
//! `GET /api/events`, and `POST /api/runs/:id/stop`. Drives `build_app`
//! through `tower::ServiceExt::oneshot`, same seam every server test uses -
//! never `RunManager`/`RoomEngine` internals directly. Ports of
//! `test/rooms.test.ts` and `test/routing.test.ts:180-230`'s room-cap case.
//!
//! S1-F-05: every route here now sits behind the session gate, so each test
//! seeds a password + session on its own `db` first (`common::seed_session`)
//! and carries the resulting cookie on every request - see that helper's doc.

mod common;

use std::sync::{Arc, Mutex};
use std::time::Duration;

use axum::Router;
use axum::body::{Body, Bytes};
use axum::http::{Request, StatusCode};
use common::{GatedPort, ScriptedPort, seed_session, text_script};
use futures::StreamExt;
use http_body_util::BodyExt;
use model::{EventStream, MessageContent, ModelEvent, ModelPort, ModelRequest, ToolCall};
use serde_json::{Value, json};
use server::{AppState, build_app};
use store::Db;
use tower::ServiceExt;

fn open_db() -> Db {
    Db::open(":memory:").expect("open :memory: db")
}

fn seed_bot(db: &Db, id: &str, name: &str) {
    db.conn()
        .execute(
            "INSERT INTO bots (id, name, purpose, instructions, model, created_at) VALUES (?1, ?2, '', ?3, NULL, '2026-01-01T00:00:00Z')",
            rusqlite::params![id, name, format!("You are {name}.")],
        )
        .expect("seed bot");
}

fn pin_model(db: &Db, bot_id: &str, model: &str) {
    db.conn()
        .execute(
            "UPDATE bots SET model = ?1 WHERE id = ?2",
            rusqlite::params![model, bot_id],
        )
        .expect("pin bot model");
}

fn app_for(db: Db, port: Arc<dyn ModelPort>) -> Router {
    build_app(AppState::with_port(db, port))
}

fn post_req(uri: &str, body: Value, cookie: &str) -> Request<Body> {
    Request::post(uri)
        .header("content-type", "application/json")
        .header("cookie", cookie)
        .body(Body::from(body.to_string()))
        .expect("build request")
}

fn get_req(uri: &str, cookie: &str) -> Request<Body> {
    Request::get(uri)
        .header("cookie", cookie)
        .body(Body::empty())
        .expect("build request")
}

async fn post_json(app: Router, uri: &str, body: Value, cookie: &str) -> (StatusCode, Value) {
    let resp = app
        .oneshot(post_req(uri, body, cookie))
        .await
        .expect("oneshot");
    let status = resp.status();
    let bytes = resp
        .into_body()
        .collect()
        .await
        .expect("collect")
        .to_bytes();
    let json = if bytes.is_empty() {
        Value::Null
    } else {
        serde_json::from_slice(&bytes).expect("response body is JSON")
    };
    (status, json)
}

async fn get_json(app: Router, uri: &str, cookie: &str) -> (StatusCode, Value) {
    let resp = app.oneshot(get_req(uri, cookie)).await.expect("oneshot");
    let status = resp.status();
    let bytes = resp
        .into_body()
        .collect()
        .await
        .expect("collect")
        .to_bytes();
    let json = if bytes.is_empty() {
        Value::Null
    } else {
        serde_json::from_slice(&bytes).expect("response body is JSON")
    };
    (status, json)
}

async fn create_room(app: &Router, title: &str, member_ids: &[&str], cookie: &str) -> String {
    let (status, body) = post_json(
        app.clone(),
        "/api/rooms",
        json!({"title": title, "memberIds": member_ids}),
        cookie,
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "create_room failed: {body:?}");
    body["room"]["id"].as_str().expect("room id").to_string()
}

/// Posts into the room and fully drains the OWNER's own SSE stream (that
/// first leg's `done`/`error`) - the rest of the round chains in the
/// background with no stream a caller holds, same as the TS original.
async fn send_message(app: &Router, bot_id: &str, thread_id: &str, text: &str, cookie: &str) {
    let resp = app
        .clone()
        .oneshot(post_req(
            &format!("/api/bots/{bot_id}/messages"),
            json!({"text": text, "threadId": thread_id}),
            cookie,
        ))
        .await
        .expect("oneshot");
    let _ = resp.into_body().collect().await.expect("collect");
}

async fn conversation_messages(
    app: &Router,
    owner_bot_id: &str,
    conversation_id: &str,
    cookie: &str,
) -> Vec<Value> {
    let (status, body) = get_json(
        app.clone(),
        &format!("/api/bots/{owner_bot_id}/conversation?thread={conversation_id}"),
        cookie,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    body["messages"].as_array().cloned().unwrap_or_default()
}

async fn wait_for_assistant_count(
    app: &Router,
    owner_bot_id: &str,
    conversation_id: &str,
    at_least: usize,
    cookie: &str,
) -> usize {
    let mut n = 0;
    for _ in 0..300 {
        let messages = conversation_messages(app, owner_bot_id, conversation_id, cookie).await;
        n = messages.iter().filter(|m| m["role"] == "assistant").count();
        if n >= at_least {
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    n
}

fn as_text(content: &MessageContent) -> String {
    match content {
        MessageContent::Text(t) => t.clone(),
        MessageContent::Parts(_) => String::new(),
    }
}

/// Reads one full SSE frame (`data: <json>\n\n`) off a response body stream,
/// buffering partial chunks. `None` once the stream ends.
async fn read_next_frame(
    stream: &mut (impl futures::Stream<Item = Result<Bytes, axum::Error>> + Unpin),
    buf: &mut String,
) -> Option<Value> {
    loop {
        if let Some(pos) = buf.find("\n\n") {
            let event_text: String = buf.drain(..pos + 2).collect();
            let data_line = event_text.lines().find(|l| l.starts_with("data:"))?;
            return serde_json::from_str(data_line["data:".len()..].trim()).ok();
        }
        let chunk = stream.next().await?.ok()?;
        buf.push_str(&String::from_utf8_lossy(&chunk));
    }
}

// 1. `POST /api/rooms` with 1 id -> 400 "at least two"; 7 -> 400 "at most
//    six"; 6 -> 201 with `memberIds`; thread with 6 members -> 400.
#[tokio::test]
async fn create_room_route_enforces_the_roster_size() {
    let db = open_db();
    let cookie = seed_session(&db);
    seed_bot(&db, "arthur", "Arthur");
    seed_bot(&db, "riley", "Riley");
    seed_bot(&db, "jason", "Jason");
    seed_bot(&db, "b0", "Bot0");
    seed_bot(&db, "b1", "Bot1");
    seed_bot(&db, "b2", "Bot2");
    seed_bot(&db, "b3", "Bot3");
    seed_bot(&db, "b4", "Bot4");
    seed_bot(&db, "b5", "Bot5");
    seed_bot(&db, "b6", "Bot6");
    let app = app_for(db, Arc::new(ScriptedPort::new(vec![])));

    let (status, body) = post_json(
        app.clone(),
        "/api/rooms",
        json!({"title": "Solo", "memberIds": ["arthur"]}),
        &cookie,
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert!(body["error"].as_str().unwrap().contains("at least two"));

    let seven = vec!["b0", "b1", "b2", "b3", "b4", "b5", "b6"];
    let (status, body) = post_json(
        app.clone(),
        "/api/rooms",
        json!({"title": "Crowd", "memberIds": seven}),
        &cookie,
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert!(body["error"].as_str().unwrap().contains("at most six"));

    let six = vec!["b0", "b1", "b2", "b3", "b4", "b5"];
    let (status, body) = post_json(
        app.clone(),
        "/api/rooms",
        json!({"title": "BigGroup", "memberIds": six}),
        &cookie,
    )
    .await;
    assert_eq!(status, StatusCode::CREATED);
    assert_eq!(
        body["room"]["memberIds"].as_array().unwrap().len(),
        6,
        "got {body:?}"
    );

    let (status, body) = post_json(
        app.clone(),
        "/api/rooms",
        json!({"title": "Growth", "memberIds": ["arthur", "riley", "jason"]}),
        &cookie,
    )
    .await;
    assert_eq!(status, StatusCode::CREATED);
    assert_eq!(
        body["room"]["memberIds"].as_array().unwrap().len(),
        3,
        "got {body:?}"
    );

    // Thread with 6 members (7 total including owner) -> 400
    let (status, body) = post_json(
        app.clone(),
        "/api/bots/arthur/threads",
        json!({"members": ["b0", "b1", "b2", "b3", "b4", "b5"]}),
        &cookie,
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert!(body["error"].as_str().unwrap().contains("at most six"));
}

// 2. A message into a 3-bot room: three model requests in owner-then-members
//    order; each request's LAST message is role `user` and contains "1 to 3
//    sentences"; every request's `model` is the cheap default even with a
//    member pinned to `anthropic/claude-sonnet-5`.
#[tokio::test]
async fn room_round_runs_every_member_capped_at_the_cheap_model() {
    let db = open_db();
    let cookie = seed_session(&db);
    seed_bot(&db, "arthur", "Arthur");
    seed_bot(&db, "riley", "Riley");
    seed_bot(&db, "jason", "Jason");
    // The exact pin that ran the bill up in production - the room cap has to
    // override it, since "sonnet" is deliberately not a premium-name marker.
    pin_model(&db, "riley", "anthropic/claude-sonnet-5");

    let port = Arc::new(ScriptedPort::new(vec![text_script("ok")]));
    let app = app_for(db, Arc::clone(&port) as Arc<dyn ModelPort>);

    let room_id = create_room(&app, "Growth", &["arthur", "riley", "jason"], &cookie).await;
    send_message(&app, "arthur", &room_id, "what do you all think?", &cookie).await;
    let n = wait_for_assistant_count(&app, "arthur", &room_id, 3, &cookie).await;
    assert_eq!(n, 3, "expected all three members to have answered");

    let requests = port.requests();
    assert_eq!(
        requests.len(),
        3,
        "expected owner-then-members, one call each"
    );
    for request in &requests {
        assert_eq!(request.model, model::CHEAP_DEFAULT_MODEL);
        let last = request.messages.last().expect("at least one message");
        assert_eq!(last.role, "user");
        let text = as_text(&last.content);
        assert!(
            text.contains("1 to 3 sentences"),
            "expected the trailing room instruction, got: {text}"
        );
    }
}

/// A port that answers for real as one named bot and declares silence for
/// everyone else - the TS `silentExcept` helper, ad hoc rather than
/// `ScriptedPort`, since every turn needs to look at WHO is asking.
struct SilentExcept {
    real_name: String,
    real_text: String,
}

impl ModelPort for SilentExcept {
    fn stream(&self, request: ModelRequest) -> EventStream {
        let system = request
            .messages
            .first()
            .map(|m| as_text(&m.content))
            .unwrap_or_default();
        let text = if system.contains(&format!("You are **{}**.", self.real_name)) {
            self.real_text.clone()
        } else {
            shared::nothing_new::NOTHING_NEW.to_string()
        };
        Box::pin(futures::stream::iter(vec![
            ModelEvent::Delta { text },
            ModelEvent::Done {
                model: "test/model".to_string(),
                usage: None,
                finish_reason: None,
            },
        ]))
    }
}

// 3. `silentExcept("Riley", ...)`: exactly one assistant message posted, by
//    Riley; nobody else's `NOTHING_NEW` reply remains.
#[tokio::test]
async fn silence_leaves_only_the_member_who_actually_spoke() {
    let db = open_db();
    let cookie = seed_session(&db);
    seed_bot(&db, "arthur", "Arthur");
    seed_bot(&db, "riley", "Riley");
    seed_bot(&db, "jason", "Jason");
    let port = SilentExcept {
        real_name: "Riley".to_string(),
        real_text: "The number holds.".to_string(),
    };
    let app = app_for(db, Arc::new(port));

    let room_id = create_room(&app, "Trio", &["arthur", "riley", "jason"], &cookie).await;
    send_message(&app, "arthur", &room_id, "anything new on this?", &cookie).await;

    // Give the whole round (all three) time to settle, not just the first
    // reply - the point is that arthur and jason stay quiet, not merely
    // that riley speaks. Every turn here resolves instantly (no real
    // network), so this margin is generous, not a race.
    tokio::time::sleep(Duration::from_millis(400)).await;

    let messages = conversation_messages(&app, "arthur", &room_id, &cookie).await;
    let assistants: Vec<&Value> = messages
        .iter()
        .filter(|m| m["role"] == "assistant")
        .collect();

    assert_eq!(assistants.len(), 1, "got {assistants:?}");
    assert_eq!(assistants[0]["content"], "The number holds.");
    assert_eq!(assistants[0]["botId"], "riley");
}

// 4. `@everyone`: three replies, none may be `NOTHING_NEW`, and each
//    prompt's trailing instruction contains "@everyone".
#[tokio::test]
async fn everyone_wakes_every_member_and_forbids_silence() {
    let db = open_db();
    let cookie = seed_session(&db);
    seed_bot(&db, "arthur", "Arthur");
    seed_bot(&db, "riley", "Riley");
    seed_bot(&db, "jason", "Jason");
    let port = Arc::new(ScriptedPort::new(vec![text_script("ok")]));
    let app = app_for(db, Arc::clone(&port) as Arc<dyn ModelPort>);

    let room_id = create_room(&app, "Growth", &["arthur", "riley", "jason"], &cookie).await;
    send_message(&app, "arthur", &room_id, "@everyone status check", &cookie).await;
    let n = wait_for_assistant_count(&app, "arthur", &room_id, 3, &cookie).await;
    assert_eq!(n, 3);

    let requests = port.requests();
    assert!(requests.len() >= 3, "got {}", requests.len());
    for request in requests.iter().take(3) {
        let last = request.messages.last().expect("at least one message");
        let text = as_text(&last.content);
        assert!(text.contains("@everyone"), "got: {text}");
        assert!(
            !text.contains(shared::nothing_new::NOTHING_NEW),
            "got: {text}"
        );
    }
}

// 5. `@Riley` inside the room: exactly one model request, for Riley.
#[tokio::test]
async fn mention_inside_a_room_routes_to_just_that_member() {
    let db = open_db();
    let cookie = seed_session(&db);
    seed_bot(&db, "arthur", "Arthur");
    seed_bot(&db, "riley", "Riley");
    seed_bot(&db, "jason", "Jason");
    let port = Arc::new(ScriptedPort::new(vec![text_script("ok")]));
    let app = app_for(db, Arc::clone(&port) as Arc<dyn ModelPort>);

    let room_id = create_room(&app, "Growth", &["arthur", "riley", "jason"], &cookie).await;
    send_message(
        &app,
        "arthur",
        &room_id,
        "@riley can you take this?",
        &cookie,
    )
    .await;

    // Give a would-be round a moment to (wrongly) continue, then confirm it
    // didn't.
    tokio::time::sleep(Duration::from_millis(150)).await;

    let requests = port.requests();
    assert_eq!(requests.len(), 1, "got {}", requests.len());
    let system = as_text(&requests[0].messages[0].content);
    assert!(system.contains("You are **Riley**."), "got: {system}");
}

/// Never answers - keeps a run in `running` forever, so its `start()` is the
/// ONLY change-bus touch it ever produces. A completing port (even a fake
/// one) settles almost instantly and touches "roster" a few milliseconds
/// later, which - since debounced kinds fire in whatever order their
/// 250 ms windows happen to elapse in, not insertion order - can race
/// "working" for second place on the stream.
struct NeverPort;

impl ModelPort for NeverPort {
    fn stream(&self, _request: ModelRequest) -> EventStream {
        Box::pin(futures::stream::pending())
    }
}

// 6. `GET /api/events`: first frame `hello`; a `touch("working")` produces a
//    `{type:"change",kind:"working"}` frame within 500 ms.
#[tokio::test]
async fn events_stream_opens_with_hello_then_reports_a_working_touch() {
    let db = open_db();
    let cookie = seed_session(&db);
    seed_bot(&db, "arthur", "Arthur");
    let app = app_for(db, Arc::new(NeverPort));

    let events_app = app.clone();
    let events_cookie = cookie.clone();
    let (frames_tx, mut frames_rx) = tokio::sync::mpsc::unbounded_channel::<Value>();
    tokio::spawn(async move {
        let resp = events_app
            .oneshot(get_req("/api/events", &events_cookie))
            .await
            .expect("oneshot");
        let mut stream = resp.into_body().into_data_stream();
        let mut buf = String::new();
        while let Some(frame) = read_next_frame(&mut stream, &mut buf).await {
            if frames_tx.send(frame).is_err() {
                break;
            }
        }
    });

    let hello = tokio::time::timeout(Duration::from_millis(500), frames_rx.recv())
        .await
        .expect("hello within 500ms")
        .expect("stream stayed open");
    assert_eq!(hello["type"], "hello");

    // Starting a run touches "working" - see `RunManager::start`. No
    // `threadId`: this lands in arthur's own (auto-created) conversation.
    // `.oneshot` resolves once the handler returns the SSE response, not
    // once its body stream ends - which, on `NeverPort`, is never; so this
    // is never drained.
    let _resp = app
        .clone()
        .oneshot(post_req(
            "/api/bots/arthur/messages",
            json!({"text": "hi"}),
            &cookie,
        ))
        .await
        .expect("oneshot");

    let change = tokio::time::timeout(Duration::from_millis(500), frames_rx.recv())
        .await
        .expect("change frame within 500ms")
        .expect("stream stayed open");
    assert_eq!(change["type"], "change");
    assert_eq!(change["kind"], "working");
}

// 7. `POST /api/runs/:id/stop` on a held run -> run failed "Stopped.".
#[tokio::test]
async fn stopping_a_held_run_over_http_fails_it_with_stopped() {
    let db = open_db();
    let cookie = seed_session(&db);
    seed_bot(&db, "arthur", "Arthur");

    let (gate_tx, gate_rx) = tokio::sync::oneshot::channel();
    let (_held_tx, held_rx) = tokio::sync::oneshot::channel();
    let port = GatedPort {
        turn: Mutex::new(0),
        gate: Mutex::new(Some(gate_rx)),
        held: Mutex::new(Some(held_rx)),
    };
    let app = app_for(db, Arc::new(port));

    let resp = app
        .clone()
        .oneshot(post_req(
            "/api/bots/arthur/messages",
            json!({"text": "hi"}),
            &cookie,
        ))
        .await
        .expect("oneshot");
    let mut stream = resp.into_body().into_data_stream();
    let mut buf = String::new();
    let first = read_next_frame(&mut stream, &mut buf)
        .await
        .expect("run frame");
    assert_eq!(first["type"], "run");
    let run_id = first["runId"].as_str().expect("runId").to_string();

    // Stop it while step 1 is still gated - `take_stop` is only checked
    // between steps, so this has to land before step 1 finishes (it asks
    // for a tool) and before step 2 (held behind its own gate) ever runs.
    let stop_resp = app
        .clone()
        .oneshot(
            Request::post(format!("/api/runs/{run_id}/stop"))
                .header("cookie", &cookie)
                .body(Body::empty())
                .expect("build request"),
        )
        .await
        .expect("oneshot");
    assert_eq!(stop_resp.status(), StatusCode::OK);

    let _ = gate_tx.send(());

    let mut error_message = None;
    while let Some(frame) = read_next_frame(&mut stream, &mut buf).await {
        if frame["type"] == "error" {
            error_message = frame["message"].as_str().map(str::to_string);
            break;
        }
    }
    assert_eq!(error_message.as_deref(), Some("Stopped."));
}

// 8. S1-F-06 (B6): a member woken by its own room's round calls
//    `message_bot` back into that SAME room, mid-round - the hook refuses to
//    start a second, overlapping round, so exactly 3 model requests happen
//    (owner's leg, the member's tool-call step, the member's final answer)
//    rather than a second full round piling 2 more on top.
#[tokio::test]
async fn message_bot_into_its_own_room_mid_round_does_not_start_a_second_round() {
    let db = open_db();
    let cookie = seed_session(&db);
    seed_bot(&db, "arthur", "Arthur");
    seed_bot(&db, "riley", "Riley");

    let message_bot_args = json!({"bot": "Loop", "question": "echo"}).to_string();
    let port = Arc::new(ScriptedPort::new(vec![
        text_script("ok"), // arthur's (owner's) leg
        vec![ModelEvent::ToolCalls {
            calls: vec![ToolCall {
                id: "c1".to_string(),
                name: "message_bot".to_string(),
                arguments: message_bot_args,
            }],
            usage: None,
        }], // riley's leg, step 1: calls message_bot back into "Loop"
        text_script("done"), // riley's leg, step 2: final answer
    ]));
    let app = app_for(db, Arc::clone(&port) as Arc<dyn ModelPort>);

    let room_id = create_room(&app, "Loop", &["arthur", "riley"], &cookie).await;
    send_message(&app, "arthur", &room_id, "go", &cookie).await;

    // 3 assistant-authored rows once the round (plus riley's message_bot
    // post) settles: arthur's "ok", riley's message_bot post ("echo"),
    // riley's own final "done".
    let n = wait_for_assistant_count(&app, "arthur", &room_id, 3, &cookie).await;
    assert_eq!(n, 3, "expected exactly the one round's worth of messages");

    // A second (illegitimate) round would add 2 more requests almost
    // immediately - give it a moment it would need, then confirm nothing
    // more ever arrived.
    tokio::time::sleep(Duration::from_millis(200)).await;
    assert_eq!(
        port.requests().len(),
        3,
        "message_bot into the room's own in-flight round must not wake a second one"
    );
}

/// Fails the FIRST call instantly (the owner's leg), then answers normally -
/// proves B9: the round still chains to member two even though the owner's
/// run settles about as fast as a run possibly can, leaving no window for
/// `on_run_done` to beat the round's own registration.
struct FailFirstThenAnswer {
    calls: Mutex<usize>,
}

impl ModelPort for FailFirstThenAnswer {
    fn stream(&self, _request: ModelRequest) -> EventStream {
        let mut calls = self.calls.lock().expect("calls mutex poisoned");
        *calls += 1;
        if *calls == 1 {
            Box::pin(futures::stream::iter(vec![ModelEvent::Error {
                message: "no key configured".to_string(),
                status: None,
            }]))
        } else {
            Box::pin(futures::stream::iter(text_script("ok")))
        }
    }
}

// 9. S1-F-06 (B9): the owner's leg fails instantly - the round still chains
//    to member two, proving the round is registered before that instant a
//    failure could possibly settle it.
#[tokio::test]
async fn an_instant_error_owner_run_still_chains_to_member_two() {
    let db = open_db();
    let cookie = seed_session(&db);
    seed_bot(&db, "arthur", "Arthur");
    seed_bot(&db, "riley", "Riley");
    seed_bot(&db, "jason", "Jason");
    let port = FailFirstThenAnswer {
        calls: Mutex::new(0),
    };
    let app = app_for(db, Arc::new(port));

    let room_id = create_room(&app, "Growth", &["arthur", "riley", "jason"], &cookie).await;
    send_message(&app, "arthur", &room_id, "go", &cookie).await;

    // Whether the owner's own (empty-text, instantly-failed) leg leaves a
    // row of its own is S1-F-10's call (F6), not this ticket's - what B9
    // guards is that riley AND jason both still answer, proving the round
    // chained past the owner's instant failure rather than losing it to the
    // race B9 named. Polled directly on bot id rather than a total count so
    // this does not couple to that other, concurrently-landing behaviour.
    let mut answered_bot_ids: Vec<Option<String>> = Vec::new();
    for _ in 0..300 {
        let messages = conversation_messages(&app, "arthur", &room_id, &cookie).await;
        answered_bot_ids = messages
            .iter()
            .filter(|m| m["role"] == "assistant")
            .map(|m| m["botId"].as_str().map(str::to_string))
            .collect();
        let riley_done = answered_bot_ids
            .iter()
            .any(|id| id.as_deref() == Some("riley"));
        let jason_done = answered_bot_ids
            .iter()
            .any(|id| id.as_deref() == Some("jason"));
        if riley_done && jason_done {
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    assert!(
        answered_bot_ids
            .iter()
            .any(|id| id.as_deref() == Some("riley")),
        "expected riley to have answered: {answered_bot_ids:?}"
    );
    assert!(
        answered_bot_ids
            .iter()
            .any(|id| id.as_deref() == Some("jason")),
        "expected jason to have answered: {answered_bot_ids:?}"
    );
}

// 10. F13: port of `test/rooms.test.ts:484` - a caller who is NOT a room
//     member reaches it by `message_bot`+title. The message posts under the
//     CALLER's own name (guest-attributed - arthur owns none of "Growth"'s
//     member slots, riley does), and wakes the round the same way a person
//     typing into it does.
#[tokio::test]
async fn message_bot_by_title_posts_under_the_callers_name_and_wakes_the_round() {
    let db = open_db();
    let cookie = seed_session(&db);
    seed_bot(&db, "arthur", "Arthur");
    seed_bot(&db, "riley", "Riley");
    seed_bot(&db, "jason", "Jason");

    let ask_growth = json!({"bot": "Growth", "question": "check the numbers"}).to_string();
    let port = Arc::new(ScriptedPort::new(vec![
        vec![ModelEvent::ToolCalls {
            calls: vec![ToolCall {
                id: "c1".to_string(),
                name: "message_bot".to_string(),
                arguments: ask_growth,
            }],
            usage: None,
        }], // arthur's own run: asks the Growth room by title
        text_script("ok"), // arthur's own final answer, then riley's and
                           // jason's room legs - all the same reply text.
    ]));
    let app = app_for(db, Arc::clone(&port) as Arc<dyn ModelPort>);

    // arthur is deliberately NOT a member - riley is "Growth"'s owner.
    let room_id = create_room(&app, "Growth", &["riley", "jason"], &cookie).await;

    let resp = app
        .clone()
        .oneshot(post_req(
            "/api/bots/arthur/messages",
            json!({"text": "ask the growth room to check the numbers"}),
            &cookie,
        ))
        .await
        .expect("oneshot");
    let _ = resp.into_body().collect().await.expect("collect");

    let mut posted_bot_id: Option<Value> = None;
    for _ in 0..300 {
        let messages = conversation_messages(&app, "riley", &room_id, &cookie).await;
        if let Some(m) = messages
            .iter()
            .find(|m| m["content"] == "check the numbers")
        {
            posted_bot_id = Some(m["botId"].clone());
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    assert_eq!(
        posted_bot_id,
        Some(Value::String("arthur".to_string())),
        "expected the post stamped under arthur's own name"
    );

    // The post itself plus riley and jason both weighing in.
    let n = wait_for_assistant_count(&app, "riley", &room_id, 3, &cookie).await;
    assert_eq!(n, 3, "got {n}");
}

// 11. F2: a `message_bot` call made DURING a room round is floored the same
//     way the round's own per-member turns are - a colleague's premium pin
//     does not get to opt out just because the ask came through
//     `message_bot` rather than the round engine directly. Bite: drop the
//     `model_for_run` wrap in `message_bot::run` and the delegated request
//     below reads Jason's raw pin instead of the room's cheap floor.
#[tokio::test]
async fn message_bot_floors_the_delegated_call_during_a_room_round() {
    let db = open_db();
    let cookie = seed_session(&db);
    seed_bot(&db, "arthur", "Arthur");
    seed_bot(&db, "riley", "Riley");
    seed_bot(&db, "jason", "Jason");
    // "opus" is a PREMIUM_MARKERS name - the exact shape of pin that ran the
    // bill up in production if a delegated call ever ignored the floor.
    pin_model(&db, "jason", "anthropic/claude-opus-5");

    let ask_jason = json!({"bot": "Jason", "question": "check the numbers"}).to_string();
    let port = Arc::new(ScriptedPort::new(vec![
        vec![ModelEvent::ToolCalls {
            calls: vec![ToolCall {
                id: "c1".to_string(),
                name: "message_bot".to_string(),
                arguments: ask_jason,
            }],
            usage: None,
        }], // arthur's room leg, step 1: asks Jason (a NON-member)
        text_script("Looks fine."), // the delegated call to Jason, arthur's
                                    // own final answer, and riley's own
                                    // room leg all reuse this same reply.
    ]));
    let app = app_for(db, Arc::clone(&port) as Arc<dyn ModelPort>);

    let room_id = create_room(&app, "Pair", &["arthur", "riley"], &cookie).await;
    send_message(&app, "arthur", &room_id, "go", &cookie).await;
    wait_for_assistant_count(&app, "arthur", &room_id, 2, &cookie).await;

    let requests = port.requests();
    let delegated = requests
        .iter()
        .find(|r| as_text(&r.messages[0].content).contains("You are **Jason**."))
        .expect("expected a delegated call addressed to Jason");
    assert_eq!(
        delegated.model,
        model::CHEAP_DEFAULT_MODEL,
        "the delegated call must go through the room's cheap-model floor, not Jason's own pin"
    );
}
