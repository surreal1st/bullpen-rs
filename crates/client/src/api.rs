//! Fetch helpers for a bot's conversation and the run stream, ported from
//! `projects/bullpen-night/src/client/App.tsx:315-380` (loading a
//! conversation), `:470-601` (`send`: POST messages, the SSE loop) and
//! `:1424-1449` (`readSse`).

use crate::transport::{Request, Response};
use crate::types::{
    ApprovalsResponse, AuthStatus, AutoReviewLogEntry, AutoReviewLogResponse, AutoReviewState, Bot,
    BotPatchResponse, BotToolsField, ConversationView, CoreStatus, Goal, MadeTool, MemoryEntry,
    MemoryEntryField, MemoryView, ModelError, ModelField, ModelsResponse, OpenQuestion,
    PendingApproval, PermissionsField, ProjectField, ProjectSummary, ProjectsField,
    QuestionsResponse, RoomResponse, RoomSummary, RoomsResponse, Routine, RoutineRun, RoutingState,
    RulesField, SharedCoreField, SharedLogField, Tier1Models, Tier1Response, WorkingBot,
    WorkingResponse,
};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;

/// `thread_id` selects a room's own conversation rather than the owner
/// bot's default one - S1-07b's rail opens a room through its owner
/// (`crates/client/src/rail.rs`'s `GroupRow`), same as the ticket's Target
/// says.
pub async fn fetch_conversation(
    bot_id: &str,
    thread_id: Option<&str>,
) -> Result<ConversationView, String> {
    let mut url = format!("/api/bots/{bot_id}/conversation");
    if let Some(thread_id) = thread_id {
        url.push_str("?thread=");
        url.push_str(thread_id);
    }
    let resp = Request::get(&url).send().await.map_err(|e| e.to_string())?;
    if !resp.ok() {
        return Err(format!("{url} -> {}", resp.status()));
    }
    resp.json::<ConversationView>()
        .await
        .map_err(|e| e.to_string())
}

pub async fn fetch_rooms() -> Result<Vec<RoomSummary>, String> {
    let resp = Request::get("/api/rooms")
        .send()
        .await
        .map_err(|e| e.to_string())?;
    if !resp.ok() {
        return Err(format!("/api/rooms -> {}", resp.status()));
    }
    let body = resp
        .json::<RoomsResponse>()
        .await
        .map_err(|e| e.to_string())?;
    Ok(body.rooms)
}

#[derive(Serialize)]
struct RoomBody<'a> {
    title: &'a str,
    #[serde(rename = "memberIds")]
    member_ids: &'a [String],
}

/// The server's own error shape on a rejected create/update, e.g. the
/// picker's cap or "pick at least two" - `{"error": "..."}"` from
/// `crates/server/src/routes/rooms.rs`.
#[derive(Deserialize)]
struct RoomError {
    error: String,
}

async fn room_result(resp: Response) -> Result<RoomSummary, String> {
    if resp.ok() {
        let body = resp
            .json::<RoomResponse>()
            .await
            .map_err(|e| e.to_string())?;
        return Ok(body.room);
    }
    let status = resp.status();
    match resp.json::<RoomError>().await {
        Ok(err) => Err(err.error),
        Err(_) => Err(format!("/api/rooms -> {status}")),
    }
}

pub async fn create_room(title: &str, member_ids: &[String]) -> Result<RoomSummary, String> {
    let resp = Request::post("/api/rooms")
        .json(&RoomBody { title, member_ids })
        .map_err(|e| e.to_string())?
        .send()
        .await
        .map_err(|e| e.to_string())?;
    room_result(resp).await
}

pub async fn update_room(
    id: &str,
    title: &str,
    member_ids: &[String],
) -> Result<RoomSummary, String> {
    let url = format!("/api/rooms/{id}");
    let resp = Request::patch(&url)
        .json(&RoomBody { title, member_ids })
        .map_err(|e| e.to_string())?
        .send()
        .await
        .map_err(|e| e.to_string())?;
    room_result(resp).await
}

pub async fn fetch_working(conversation_id: &str) -> Result<Vec<WorkingBot>, String> {
    let url = format!("/api/conversations/{conversation_id}/working");
    let resp = Request::get(&url).send().await.map_err(|e| e.to_string())?;
    if !resp.ok() {
        return Err(format!("{url} -> {}", resp.status()));
    }
    let body = resp
        .json::<WorkingResponse>()
        .await
        .map_err(|e| e.to_string())?;
    Ok(body.working)
}

/// One frame of `POST /api/bots/:id/messages`'s stream. A strict subset of
/// `StreamEvent` in `shared/types.ts:201-209` - S1-07a only had to react to
/// the three kinds the mock (and eventually S1-06) actually sent for a plain
/// send; `notice`/`tool_call`/`tool_result`/`approval_needed` still collapse
/// into `Ignored` rather than being treated as failures, so a kind this
/// ticket does not handle degrades instead of breaking the stream.
///
/// F7 (S1-F-11): `error` is no longer folded into `Ignored` - with no
/// OpenRouter key configured, sending a message used to leave the streaming
/// bubble blank and the composer re-enabled with no message anywhere on
/// screen, the run row's "No OpenRouter key..." visible only in server logs.
/// The server's SSE frame is `{"type":"error","message":"...","status":...}`
/// (`crates/server/src/routes/messages.rs::run_event_json`); `status` is not
/// carried here since `thread.rs`'s render (`.upstream-error`, ported from
/// `MessageRow.tsx:195-215`) only ever shows `message`.
#[derive(Debug, Clone, PartialEq)]
pub enum StreamEvent {
    Run { run_id: String },
    Delta { text: String },
    Done { model: Option<String> },
    Error { message: String },
    Ignored,
}

#[derive(Deserialize)]
struct RawFrame {
    #[serde(rename = "type")]
    kind: String,
    #[serde(default, rename = "runId")]
    run_id: Option<String>,
    #[serde(default)]
    text: Option<String>,
    #[serde(default)]
    model: Option<String>,
    #[serde(default)]
    message: Option<String>,
}

fn parse_frame(json: &str) -> Option<StreamEvent> {
    let raw: RawFrame = serde_json::from_str(json).ok()?;
    Some(match raw.kind.as_str() {
        "run" => StreamEvent::Run {
            run_id: raw.run_id.unwrap_or_default(),
        },
        "delta" => StreamEvent::Delta {
            text: raw.text.unwrap_or_default(),
        },
        "done" => StreamEvent::Done { model: raw.model },
        "error" => StreamEvent::Error {
            message: raw.message.unwrap_or_default(),
        },
        _ => StreamEvent::Ignored,
    })
}

/// Ported from `readSse` (`App.tsx:1424-1449`): buffers raw bytes, splits on
/// `\n`, and parses every `data: ...` line as one frame. Byte-based and pure
/// (no `web_sys`) so it can be driven one network chunk at a time from a
/// plain `cargo test` - see the bite check in the ticket's `## Result`,
/// proven here rather than only by eye in a browser.
pub fn feed(buffer: &mut Vec<u8>, chunk: &[u8]) -> Vec<StreamEvent> {
    buffer.extend_from_slice(chunk);
    let mut events = Vec::new();
    while let Some(pos) = buffer.iter().position(|&b| b == b'\n') {
        let mut line: Vec<u8> = buffer.drain(..=pos).collect();
        line.pop(); // the '\n'
        if line.last() == Some(&b'\r') {
            line.pop();
        }
        let line = String::from_utf8_lossy(&line);
        let Some(payload) = line.strip_prefix("data:") else {
            continue;
        };
        let payload = payload.trim();
        if payload.is_empty() {
            continue;
        }
        if let Some(event) = parse_frame(payload) {
            events.push(event);
        }
    }
    events
}

/// F14 (S1-F-11): opening a bot is what makes it read - ported from
/// `App.tsx:758`'s `fetch(/api/bots/${id}/seen)`. Fire this, then have the
/// caller refetch the roster so the dot clears; unlike the TS original this
/// does not parse the response body (it carries a fresh roster of its own,
/// but `app.rs` already owns a `fetch_roster` for that and there is no
/// reason for two different shapes of "the current roster" in one client).
pub async fn mark_bot_seen(bot_id: &str) -> Result<(), String> {
    let url = format!("/api/bots/{bot_id}/seen");
    let resp = Request::post(&url)
        .send()
        .await
        .map_err(|e| e.to_string())?;
    if !resp.ok() {
        return Err(format!("{url} -> {}", resp.status()));
    }
    Ok(())
}

/// Same as `mark_bot_seen`, for a group chat - ported from
/// `App.tsx:769`'s `fetch(/api/rooms/${room.id}/seen)`.
pub async fn mark_room_seen(room_id: &str) -> Result<(), String> {
    let url = format!("/api/rooms/{room_id}/seen");
    let resp = Request::post(&url)
        .send()
        .await
        .map_err(|e| e.to_string())?;
    if !resp.ok() {
        return Err(format!("{url} -> {}", resp.status()));
    }
    Ok(())
}

/// `GET /api/approvals` - every pending approval, across every bot.
/// `approvals.rs`'s pane groups these itself
/// (`shared::approval_groups::group_approvals`); this is the flat list, same
/// as the server route answers. Ported from `Approvals.tsx`'s `useApprovals`.
pub async fn fetch_approvals() -> Result<Vec<PendingApproval>, String> {
    let resp = Request::get("/api/approvals")
        .send()
        .await
        .map_err(|e| e.to_string())?;
    if !resp.ok() {
        return Err(format!("/api/approvals -> {}", resp.status()));
    }
    let body = resp
        .json::<ApprovalsResponse>()
        .await
        .map_err(|e| e.to_string())?;
    Ok(body.approvals)
}

#[derive(Serialize)]
struct DecideBody<'a> {
    approved: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    result: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    remember: Option<&'a str>,
}

/// `POST /api/approvals/:id` - decides one pending call. `result` carries a
/// question's typed/picked answer (`crates/server/src/routes/approvals.rs`
/// does not read it yet - `ask_josh` is `allow` by default, S2-02, so it
/// never actually reaches this pane in S2 - sent anyway so the field is
/// already in place once a bot's default is ever tightened to `ask`).
/// `remember` is the "Always allow"/"Never" press; S2-07 (blocked on S2-03)
/// owns turning it into an auto-review rule, so today the server accepts and
/// ignores it, same as `approved` alone would decide the call.
pub async fn decide_approval(
    id: &str,
    approved: bool,
    result: Option<&str>,
    remember: Option<&str>,
) -> Result<(), String> {
    let url = format!("/api/approvals/{id}");
    let resp = Request::post(&url)
        .json(&DecideBody {
            approved,
            result,
            remember,
        })
        .map_err(|e| e.to_string())?
        .send()
        .await
        .map_err(|e| e.to_string())?;
    if !resp.ok() {
        return Err(format!("{url} -> {}", resp.status()));
    }
    Ok(())
}

/// `GET /api/questions` - every open question, across every bot.
/// `questions.rs`'s pane filters this to the open conversation's bot itself,
/// same as the TS `Questions` component's own `mine` filter.
pub async fn fetch_questions() -> Result<Vec<OpenQuestion>, String> {
    let resp = Request::get("/api/questions")
        .send()
        .await
        .map_err(|e| e.to_string())?;
    if !resp.ok() {
        return Err(format!("/api/questions -> {}", resp.status()));
    }
    let body = resp
        .json::<QuestionsResponse>()
        .await
        .map_err(|e| e.to_string())?;
    Ok(body.questions)
}

#[derive(Serialize)]
struct AnswerBody<'a> {
    answer: &'a str,
}

/// `POST /api/questions/:id` - records Josh's answer; the server posts it
/// into the conversation as an ordinary message on success (204).
pub async fn answer_question(id: &str, answer: &str) -> Result<(), String> {
    let url = format!("/api/questions/{id}");
    let resp = Request::post(&url)
        .json(&AnswerBody { answer })
        .map_err(|e| e.to_string())?
        .send()
        .await
        .map_err(|e| e.to_string())?;
    if !resp.ok() {
        return Err(format!("{url} -> {}", resp.status()));
    }
    Ok(())
}

/// `GET /api/auth/status` - the gate's first question on every boot. Ported
/// from `Gate.tsx:67-77`.
pub async fn auth_status() -> Result<AuthStatus, String> {
    let resp = Request::get("/api/auth/status")
        .send()
        .await
        .map_err(|e| e.to_string())?;
    if !resp.ok() {
        return Err(format!("/api/auth/status -> {}", resp.status()));
    }
    resp.json::<AuthStatus>().await.map_err(|e| e.to_string())
}

#[derive(Serialize)]
struct LoginBody<'a> {
    password: &'a str,
}

#[derive(Deserialize)]
struct LoginError {
    error: String,
}

/// `POST /api/auth/login` - ported from `Gate.tsx:88-123`'s `signIn`. The
/// server answers with a `Set-Cookie` on success; `with_credentials()` is
/// belt-and-suspenders for a same-origin fetch on web (the browser default
/// already sends cookies here), kept explicit per the ticket's own
/// instruction so a future reverse-proxy split between client and API
/// origins does not silently drop the session cookie. S13a-01: on native
/// this is a no-op - `transport::native`'s shared client already carries
/// the session cookie via its own cookie store, with no per-request browser
/// cookie jar to opt into.
pub async fn login(password: &str) -> Result<(), String> {
    let resp = Request::post("/api/auth/login")
        .with_credentials()
        .json(&LoginBody { password })
        .map_err(|e| e.to_string())?
        .send()
        .await
        .map_err(|e| e.to_string())?;
    if resp.ok() {
        return Ok(());
    }
    match resp.json::<LoginError>().await {
        Ok(err) => Err(err.error),
        Err(_) => Err("That did not work.".to_string()),
    }
}

#[derive(Serialize)]
struct SendBody<'a> {
    text: &'a str,
    #[serde(rename = "threadId", skip_serializing_if = "Option::is_none")]
    thread_id: Option<&'a str>,
}

/// Sends the message and drives `on_event` for every frame in the response
/// stream. A `content-type: application/json` response - a run already in
/// flight absorbed this message as an interjection, `App.tsx:503-509` -
/// calls `on_event` zero times and returns `Ok(())`, same as the original.
/// `thread_id` posts into a room's own conversation rather than the owner
/// bot's default one - see `fetch_conversation`'s doc.
///
/// S13a-01: this used to be wasm-only (`#[cfg(feature = "web")]`), reading
/// the body through a raw `web_sys::ReadableStreamDefaultReader` inline.
/// That reader is now `transport::BodyStream` (`web.rs`'s `WebBody` on
/// wasm32, `native.rs`'s `NativeBody` over `reqwest` everywhere else), so
/// this function is unchanged in shape but portable: the same `feed()` byte
/// buffering drives both platforms.
pub async fn send_message(
    bot_id: &str,
    text: &str,
    thread_id: Option<&str>,
    mut on_event: impl FnMut(StreamEvent),
) -> Result<(), String> {
    let url = format!("/api/bots/{bot_id}/messages");
    let resp = Request::post(&url)
        .json(&SendBody { text, thread_id })
        .map_err(|e| e.to_string())?
        .send()
        .await
        .map_err(|e| e.to_string())?;

    if !resp.ok() {
        return Err(format!("{url} -> {}", resp.status()));
    }

    let is_json = resp
        .content_type()
        .is_some_and(|ct| ct.contains("application/json"));
    if is_json {
        return Ok(());
    }

    let mut stream = resp.into_body_stream();
    let mut buffer = Vec::new();
    while let Some(chunk) = stream.next_chunk().await? {
        for event in feed(&mut buffer, &chunk) {
            on_event(event);
        }
    }
    Ok(())
}

/* -------------------------------------------------------------- S2-09b: settings */

/// Shared GET for the three plain model settings (`/api/default-model`,
/// `/api/mid-model`, `/api/premium-model`): all answer `{"model": "..."}"`.
async fn get_model_field(url: &str) -> Result<String, String> {
    let resp = Request::get(url).send().await.map_err(|e| e.to_string())?;
    if !resp.ok() {
        return Err(format!("{url} -> {}", resp.status()));
    }
    resp.json::<ModelField>()
        .await
        .map(|b| b.model)
        .map_err(|e| e.to_string())
}

/// Shared PUT for the same three settings. A refusal (premium default, an
/// empty id) comes back as a 400 with `{"error": "..."}"` - surfaced so the
/// caller can show it, same as `General.tsx`'s own `refusal` banner.
async fn put_model_field(url: &str, model: &str) -> Result<String, String> {
    #[derive(Serialize)]
    struct Req<'a> {
        model: &'a str,
    }
    let resp = Request::put(url)
        .json(&Req { model })
        .map_err(|e| e.to_string())?
        .send()
        .await
        .map_err(|e| e.to_string())?;
    if resp.ok() {
        return resp
            .json::<ModelField>()
            .await
            .map(|b| b.model)
            .map_err(|e| e.to_string());
    }
    // S13a-01: `status` is captured before `.json()` consumes `resp` -
    // `transport::Response::json` takes `self`, not `&self` (unlike
    // `gloo_net::http::Response::json`), since the native half buffers a
    // stream it can only read once.
    let status = resp.status();
    match resp.json::<ModelError>().await {
        Ok(err) => Err(err.error),
        Err(_) => Err(format!("{url} -> {status}")),
    }
}

pub async fn fetch_default_model() -> Result<String, String> {
    get_model_field("/api/default-model").await
}

pub async fn put_default_model(model: &str) -> Result<String, String> {
    put_model_field("/api/default-model", model).await
}

pub async fn fetch_mid_model() -> Result<String, String> {
    get_model_field("/api/mid-model").await
}

pub async fn put_mid_model(model: &str) -> Result<String, String> {
    put_model_field("/api/mid-model", model).await
}

pub async fn fetch_premium_model() -> Result<String, String> {
    get_model_field("/api/premium-model").await
}

pub async fn put_premium_model(model: &str) -> Result<String, String> {
    put_model_field("/api/premium-model", model).await
}

pub async fn fetch_tier1_models() -> Result<Tier1Models, String> {
    let resp = Request::get("/api/tier1-models")
        .send()
        .await
        .map_err(|e| e.to_string())?;
    if !resp.ok() {
        return Err(format!("/api/tier1-models -> {}", resp.status()));
    }
    resp.json::<Tier1Response>()
        .await
        .map(|b| b.models)
        .map_err(|e| e.to_string())
}

pub async fn put_tier1_model(kind: &str, model: &str) -> Result<String, String> {
    #[derive(Serialize)]
    struct Req<'a> {
        kind: &'a str,
        model: &'a str,
    }
    let resp = Request::put("/api/tier1-models")
        .json(&Req { kind, model })
        .map_err(|e| e.to_string())?
        .send()
        .await
        .map_err(|e| e.to_string())?;
    if resp.ok() {
        return resp
            .json::<ModelField>()
            .await
            .map(|b| b.model)
            .map_err(|e| e.to_string());
    }
    // S13a-01: see `put_model_field`'s comment above on why `status` must
    // be captured before `.json()`.
    let status = resp.status();
    match resp.json::<ModelError>().await {
        Ok(err) => Err(err.error),
        Err(_) => Err(format!("/api/tier1-models -> {status}")),
    }
}

pub async fn fetch_routing() -> Result<RoutingState, String> {
    let resp = Request::get("/api/routing")
        .send()
        .await
        .map_err(|e| e.to_string())?;
    if !resp.ok() {
        return Err(format!("/api/routing -> {}", resp.status()));
    }
    resp.json::<RoutingState>().await.map_err(|e| e.to_string())
}

async fn put_routing(body: serde_json::Value) -> Result<RoutingState, String> {
    let resp = Request::put("/api/routing")
        .json(&body)
        .map_err(|e| e.to_string())?
        .send()
        .await
        .map_err(|e| e.to_string())?;
    if !resp.ok() {
        return Err(format!("/api/routing -> {}", resp.status()));
    }
    resp.json::<RoutingState>().await.map_err(|e| e.to_string())
}

pub async fn put_routing_enabled(enabled: bool) -> Result<RoutingState, String> {
    put_routing(serde_json::json!({ "enabled": enabled })).await
}

pub async fn put_routing_text(text: &str) -> Result<RoutingState, String> {
    put_routing(serde_json::json!({ "text": text })).await
}

/// S4-05: `GET /api/auto-review/judge` - the platform toggle for the risky-
/// tool judge (`crates/server/src/judge.rs`, default ON when absent).
pub async fn fetch_judge() -> Result<AutoReviewState, String> {
    let resp = Request::get("/api/auto-review/judge")
        .send()
        .await
        .map_err(|e| e.to_string())?;
    if !resp.ok() {
        return Err(format!("/api/auto-review/judge -> {}", resp.status()));
    }
    resp.json::<AutoReviewState>()
        .await
        .map_err(|e| e.to_string())
}

/// `PUT /api/auto-review/judge`.
pub async fn put_judge(enabled: bool) -> Result<AutoReviewState, String> {
    let resp = Request::put("/api/auto-review/judge")
        .json(&serde_json::json!({ "enabled": enabled }))
        .map_err(|e| e.to_string())?
        .send()
        .await
        .map_err(|e| e.to_string())?;
    if !resp.ok() {
        return Err(format!("/api/auto-review/judge -> {}", resp.status()));
    }
    resp.json::<AutoReviewState>()
        .await
        .map_err(|e| e.to_string())
}

/// `GET /api/auto-review/log?limit=<limit>` - the "Last 20 judgements" table
/// on the Auto review settings card.
pub async fn fetch_judge_log(limit: u32) -> Result<Vec<AutoReviewLogEntry>, String> {
    let url = format!("/api/auto-review/log?limit={limit}");
    let resp = Request::get(&url).send().await.map_err(|e| e.to_string())?;
    if !resp.ok() {
        return Err(format!("{url} -> {}", resp.status()));
    }
    resp.json::<AutoReviewLogResponse>()
        .await
        .map(|body| body.entries)
        .map_err(|e| e.to_string())
}

pub async fn fetch_rules() -> Result<String, String> {
    let resp = Request::get("/api/rules")
        .send()
        .await
        .map_err(|e| e.to_string())?;
    if !resp.ok() {
        return Err(format!("/api/rules -> {}", resp.status()));
    }
    resp.json::<RulesField>()
        .await
        .map(|b| b.rules)
        .map_err(|e| e.to_string())
}

pub async fn put_rules(rules: &str) -> Result<String, String> {
    #[derive(Serialize)]
    struct Req<'a> {
        rules: &'a str,
    }
    let resp = Request::put("/api/rules")
        .json(&Req { rules })
        .map_err(|e| e.to_string())?
        .send()
        .await
        .map_err(|e| e.to_string())?;
    if !resp.ok() {
        return Err(format!("/api/rules -> {}", resp.status()));
    }
    resp.json::<RulesField>()
        .await
        .map(|b| b.rules)
        .map_err(|e| e.to_string())
}

/// `GET /api/models?q=...`, `all=1` for the full catalogue - ported from
/// `ModelPicker.tsx`'s own fetch. `transport::encode_uri_component` matches
/// the TS `encodeURIComponent` this is a straight port of (S13a-01: was
/// `js_sys::encode_uri_component`, wasm-only - see that function's doc).
pub async fn fetch_models(query: &str, show_all: bool) -> Result<ModelsResponse, String> {
    let encoded = crate::transport::encode_uri_component(query);
    let url = if show_all {
        format!("/api/models?all=1&q={encoded}")
    } else {
        format!("/api/models?q={encoded}")
    };
    let resp = Request::get(&url).send().await.map_err(|e| e.to_string())?;
    if !resp.ok() {
        return Err(format!("{url} -> {}", resp.status()));
    }
    resp.json::<ModelsResponse>()
        .await
        .map_err(|e| e.to_string())
}

pub async fn fetch_permissions(bot_id: &str) -> Result<HashMap<String, String>, String> {
    let url = format!("/api/bots/{bot_id}/permissions");
    let resp = Request::get(&url).send().await.map_err(|e| e.to_string())?;
    if !resp.ok() {
        return Err(format!("{url} -> {}", resp.status()));
    }
    resp.json::<PermissionsField>()
        .await
        .map(|b| b.permissions)
        .map_err(|e| e.to_string())
}

pub async fn put_permissions(
    bot_id: &str,
    permissions: &HashMap<String, String>,
) -> Result<HashMap<String, String>, String> {
    #[derive(Serialize)]
    struct Req<'a> {
        permissions: &'a HashMap<String, String>,
    }
    let url = format!("/api/bots/{bot_id}/permissions");
    let resp = Request::put(&url)
        .json(&Req { permissions })
        .map_err(|e| e.to_string())?
        .send()
        .await
        .map_err(|e| e.to_string())?;
    if !resp.ok() {
        return Err(format!("{url} -> {}", resp.status()));
    }
    resp.json::<PermissionsField>()
        .await
        .map(|b| b.permissions)
        .map_err(|e| e.to_string())
}

/// `GET /api/bot-tools` - W5's bot-written-tool list. Not built on the Rust
/// server yet, so a 404/network failure degrades to an empty list, same as
/// `PermissionsEditor.tsx`'s own `.catch(() => setMade([]))`.
pub async fn fetch_bot_tools() -> Vec<MadeTool> {
    let Ok(resp) = Request::get("/api/bot-tools").send().await else {
        return Vec::new();
    };
    if !resp.ok() {
        return Vec::new();
    }
    resp.json::<BotToolsField>()
        .await
        .map(|b| b.tools)
        .unwrap_or_default()
}

/// `PATCH /api/bots/:id` - pins a model and/or sets the reasoning effort
/// (`crates/server/src/routes/bots.rs`, added by this same ticket). `body`
/// carries only the keys being changed - `null` for `model` clears the pin,
/// matching the route's own "key present" semantics.
pub async fn patch_bot(bot_id: &str, body: serde_json::Value) -> Result<Bot, String> {
    let url = format!("/api/bots/{bot_id}");
    let resp = Request::patch(&url)
        .json(&body)
        .map_err(|e| e.to_string())?
        .send()
        .await
        .map_err(|e| e.to_string())?;
    if resp.ok() {
        return resp
            .json::<BotPatchResponse>()
            .await
            .map(|b| b.bot)
            .map_err(|e| e.to_string());
    }
    // S13a-01: see `put_model_field`'s comment on why `status` must be
    // captured before `.json()`.
    let status = resp.status();
    match resp.json::<ModelError>().await {
        Ok(err) => Err(err.error),
        Err(_) => Err(format!("{url} -> {status}")),
    }
}

/* --------------------------------------------------------------- S3-05: memory */

/// `GET /api/bots/:id/memory[?q=...]` - core, its token budget, and the log,
/// filtered server-side when `query` is non-empty (`routes/memory.rs`'s
/// `get_bot_memory` reads `q` and calls `store::memory::search_log`). Every
/// caller in `memory_editor.rs` passes the search box's current value, empty
/// or not - there is no separate "plain" fetch.
pub async fn fetch_bot_memory_query(bot_id: &str, query: &str) -> Result<MemoryView, String> {
    let url = if query.is_empty() {
        format!("/api/bots/{bot_id}/memory")
    } else {
        let encoded = crate::transport::encode_uri_component(query);
        format!("/api/bots/{bot_id}/memory?q={encoded}")
    };
    let resp = Request::get(&url).send().await.map_err(|e| e.to_string())?;
    if !resp.ok() {
        return Err(format!("{url} -> {}", resp.status()));
    }
    resp.json::<MemoryView>().await.map_err(|e| e.to_string())
}

/// `POST /api/bots/:id/memory` - a durable entry with no TTL (`source:
/// "josh"`, `routes/memory.rs::post_bot_memory`). The pane's "Remember"
/// row - see `S3-F-01c`'s doc on `memory_editor.rs` for why the note form
/// alone could not express this.
pub async fn remember_entry(bot_id: &str, content: &str) -> Result<MemoryEntry, String> {
    #[derive(Serialize)]
    struct Req<'a> {
        content: &'a str,
    }
    let url = format!("/api/bots/{bot_id}/memory");
    let resp = Request::post(&url)
        .json(&Req { content })
        .map_err(|e| e.to_string())?
        .send()
        .await
        .map_err(|e| e.to_string())?;
    if !resp.ok() {
        return Err(format!("{url} -> {}", resp.status()));
    }
    resp.json::<MemoryEntryField>()
        .await
        .map(|b| b.entry)
        .map_err(|e| e.to_string())
}

/// `PUT /api/bots/:id/memory/core`.
pub async fn put_bot_memory_core(bot_id: &str, core: &str) -> Result<CoreStatus, String> {
    #[derive(Serialize)]
    struct Req<'a> {
        core: &'a str,
    }
    let url = format!("/api/bots/{bot_id}/memory/core");
    let resp = Request::put(&url)
        .json(&Req { core })
        .map_err(|e| e.to_string())?
        .send()
        .await
        .map_err(|e| e.to_string())?;
    if !resp.ok() {
        return Err(format!("{url} -> {}", resp.status()));
    }
    resp.json::<CoreStatus>().await.map_err(|e| e.to_string())
}

/// `POST /api/bots/:id/memory/notes` - a temporary entry with a TTL, in
/// seconds.
pub async fn post_bot_memory_note(
    bot_id: &str,
    content: &str,
    ttl_seconds: u64,
) -> Result<MemoryEntry, String> {
    #[derive(Serialize)]
    #[serde(rename_all = "camelCase")]
    struct Req<'a> {
        content: &'a str,
        ttl_seconds: u64,
    }
    let url = format!("/api/bots/{bot_id}/memory/notes");
    let resp = Request::post(&url)
        .json(&Req {
            content,
            ttl_seconds,
        })
        .map_err(|e| e.to_string())?
        .send()
        .await
        .map_err(|e| e.to_string())?;
    if !resp.ok() {
        return Err(format!("{url} -> {}", resp.status()));
    }
    resp.json::<MemoryEntryField>()
        .await
        .map(|b| b.entry)
        .map_err(|e| e.to_string())
}

/// `DELETE /api/bots/:id/memory/:entryId`.
pub async fn delete_bot_memory_entry(bot_id: &str, entry_id: &str) -> Result<(), String> {
    let url = format!("/api/bots/{bot_id}/memory/{entry_id}");
    let resp = Request::delete(&url)
        .send()
        .await
        .map_err(|e| e.to_string())?;
    if !resp.ok() {
        return Err(format!("{url} -> {}", resp.status()));
    }
    Ok(())
}

/// `GET /api/shared-core`.
pub async fn fetch_shared_core() -> Result<String, String> {
    let resp = Request::get("/api/shared-core")
        .send()
        .await
        .map_err(|e| e.to_string())?;
    if !resp.ok() {
        return Err(format!("/api/shared-core -> {}", resp.status()));
    }
    resp.json::<SharedCoreField>()
        .await
        .map(|b| b.core)
        .map_err(|e| e.to_string())
}

/// `PUT /api/shared-core`.
pub async fn put_shared_core(core: &str) -> Result<String, String> {
    #[derive(Serialize)]
    struct Req<'a> {
        core: &'a str,
    }
    let resp = Request::put("/api/shared-core")
        .json(&Req { core })
        .map_err(|e| e.to_string())?
        .send()
        .await
        .map_err(|e| e.to_string())?;
    if !resp.ok() {
        return Err(format!("/api/shared-core -> {}", resp.status()));
    }
    resp.json::<SharedCoreField>()
        .await
        .map(|b| b.core)
        .map_err(|e| e.to_string())
}

/// `GET /api/projects`.
pub async fn fetch_projects() -> Result<Vec<ProjectSummary>, String> {
    let resp = Request::get("/api/projects")
        .send()
        .await
        .map_err(|e| e.to_string())?;
    if !resp.ok() {
        return Err(format!("/api/projects -> {}", resp.status()));
    }
    resp.json::<ProjectsField>()
        .await
        .map(|b| b.projects)
        .map_err(|e| e.to_string())
}

/// `POST /api/projects`.
pub async fn create_project(name: &str) -> Result<ProjectSummary, String> {
    #[derive(Serialize)]
    struct Req<'a> {
        name: &'a str,
    }
    let resp = Request::post("/api/projects")
        .json(&Req { name })
        .map_err(|e| e.to_string())?
        .send()
        .await
        .map_err(|e| e.to_string())?;
    if !resp.ok() {
        return Err(format!("/api/projects -> {}", resp.status()));
    }
    resp.json::<ProjectField>()
        .await
        .map(|b| b.project)
        .map_err(|e| e.to_string())
}

/// `POST /api/projects/:id/members`.
pub async fn add_project_member(project_id: &str, bot_id: &str) -> Result<(), String> {
    #[derive(Serialize)]
    struct Req<'a> {
        #[serde(rename = "botId")]
        bot_id: &'a str,
    }
    let url = format!("/api/projects/{project_id}/members");
    let resp = Request::post(&url)
        .json(&Req { bot_id })
        .map_err(|e| e.to_string())?
        .send()
        .await
        .map_err(|e| e.to_string())?;
    if !resp.ok() {
        return Err(format!("{url} -> {}", resp.status()));
    }
    Ok(())
}

/// `GET /api/memory/shared`.
pub async fn fetch_shared_memory() -> Result<Vec<MemoryEntry>, String> {
    let resp = Request::get("/api/memory/shared")
        .send()
        .await
        .map_err(|e| e.to_string())?;
    if !resp.ok() {
        return Err(format!("/api/memory/shared -> {}", resp.status()));
    }
    resp.json::<SharedLogField>()
        .await
        .map(|b| b.log)
        .map_err(|e| e.to_string())
}

/// `DELETE /api/memory/shared/:entryId`.
pub async fn delete_shared_memory_entry(entry_id: &str) -> Result<(), String> {
    let url = format!("/api/memory/shared/{entry_id}");
    let resp = Request::delete(&url)
        .send()
        .await
        .map_err(|e| e.to_string())?;
    if !resp.ok() {
        return Err(format!("{url} -> {}", resp.status()));
    }
    Ok(())
}

/* --------------------------------------------------------- S5-05: routines */

/// `GET /api/routines?bot=...`'s response shape.
#[derive(Deserialize, Default)]
struct RoutinesField {
    #[serde(default)]
    routines: Vec<Routine>,
}

/// `POST /api/routines`, `PATCH /api/routines/:id` and `POST /api/routines/
/// :id/active`'s success shape (`crates/server/src/routes/routines.rs`
/// always answers `{"routine": {...}}` on all three).
#[derive(Deserialize)]
struct RoutineField {
    routine: Routine,
}

/// A rejected create/update/patch's shape (a bad schedule, a missing name):
/// `{"error": "..."}"` from `crates/server/src/error.rs::AppError`. This is
/// the "live description" the create/edit form ultimately trusts -
/// `routines_editor.rs`'s own client-side preview is best-effort only, see
/// that module's doc.
#[derive(Deserialize)]
struct RoutineError {
    error: String,
}

async fn routine_result(resp: Response) -> Result<Routine, String> {
    if resp.ok() {
        let body = resp
            .json::<RoutineField>()
            .await
            .map_err(|e| e.to_string())?;
        return Ok(body.routine);
    }
    let status = resp.status();
    match resp.json::<RoutineError>().await {
        Ok(err) => Err(err.error),
        Err(_) => Err(format!("/api/routines -> {status}")),
    }
}

/// `GET /api/routines?bot=:botId` - one bot's routines only, same query
/// param `ListQuery` in the route reads.
pub async fn fetch_routines(bot_id: &str) -> Result<Vec<Routine>, String> {
    let url = format!("/api/routines?bot={bot_id}");
    let resp = Request::get(&url).send().await.map_err(|e| e.to_string())?;
    if !resp.ok() {
        return Err(format!("{url} -> {}", resp.status()));
    }
    resp.json::<RoutinesField>()
        .await
        .map(|b| b.routines)
        .map_err(|e| e.to_string())
}

/// `POST /api/routines`'s body. `kind`/`tool`/`tool_args` are S5b-07's
/// addition - `None` (the field skipped entirely) is a prompt-kind routine,
/// same as before this ticket (`CreateRoutineBody::kind` on the server
/// defaults to `"prompt"` when the key is absent). Public so
/// `routines_editor.rs` can build one directly rather than this module
/// growing a same-shaped positional-argument function for every new field
/// S5b lands (the ticket adds three at once).
#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CreateRoutineReq<'a> {
    pub bot_id: &'a str,
    pub name: &'a str,
    pub prompt: &'a str,
    pub schedule: &'a str,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub kind: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tool: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tool_args: Option<&'a str>,
}

/// `POST /api/routines`.
pub async fn create_routine(req: &CreateRoutineReq<'_>) -> Result<Routine, String> {
    let resp = Request::post("/api/routines")
        .json(req)
        .map_err(|e| e.to_string())?
        .send()
        .await
        .map_err(|e| e.to_string())?;
    routine_result(resp).await
}

/// `PATCH /api/routines/:id`'s body. S5b-07 widens this past the S5-05
/// name/prompt/schedule trio with the tool and hook fields (`conditions`
/// stays out - no UI for it here). `hook_events`/`hook_match` are nested
/// `Option<Option<_>>` on purpose, mirroring `UpdateRoutineBody` on the
/// server: the OUTER `None` (skipped by `skip_serializing_if`) means "do not
/// touch this field"; `Some(None)` serializes to JSON `null` (serde's
/// ordinary `Option<T>` behaviour on the INNER option), which the route
/// reads as "clear it"; `Some(Some(v))` sets it. `routines_editor.rs`'s hook
/// form always sends one of the three explicitly, never leaves this to
/// chance.
#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct UpdateRoutineReq<'a> {
    pub name: &'a str,
    pub prompt: &'a str,
    pub schedule: &'a str,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub kind: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tool: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tool_args: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub hook_kind: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub hook_events: Option<Option<Vec<String>>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub hook_match: Option<Option<&'a str>>,
}

/// `PATCH /api/routines/:id`.
pub async fn update_routine(id: &str, req: &UpdateRoutineReq<'_>) -> Result<Routine, String> {
    let url = format!("/api/routines/{id}");
    let resp = Request::patch(&url)
        .json(req)
        .map_err(|e| e.to_string())?
        .send()
        .await
        .map_err(|e| e.to_string())?;
    routine_result(resp).await
}

/// `{"secret": "...", "url": "..."}"`, `POST /api/routines/:id/hook`'s 201
/// body.
#[derive(Deserialize)]
struct HookMintField {
    secret: String,
    url: String,
}

/// `POST /api/routines/:id/hook` - mints a fresh webhook secret, returned
/// ONCE (`routes/hooks.rs`'s own doc: `GET /api/routines` only ever says
/// `hasHook: true` afterward). The caller (`routines_editor.rs`'s
/// minted-secret UI) must show it immediately, offer a copy button, and
/// never persist or log it - there is no second chance to fetch it.
pub async fn mint_routine_hook(id: &str) -> Result<(String, String), String> {
    let url = format!("/api/routines/{id}/hook");
    let resp = Request::post(&url)
        .send()
        .await
        .map_err(|e| e.to_string())?;
    if !resp.ok() {
        // S13a-01: see `put_model_field`'s comment on why `status` must be
        // captured before `.json()`.
        let status = resp.status();
        return match resp.json::<RoutineError>().await {
            Ok(err) => Err(err.error),
            Err(_) => Err(format!("{url} -> {status}")),
        };
    }
    resp.json::<HookMintField>()
        .await
        .map(|b| (b.secret, b.url))
        .map_err(|e| e.to_string())
}

/// `DELETE /api/routines/:id/hook` - clears the webhook secret so the
/// routine no longer accepts deliveries.
pub async fn clear_routine_hook(id: &str) -> Result<(), String> {
    let url = format!("/api/routines/{id}/hook");
    let resp = Request::delete(&url)
        .send()
        .await
        .map_err(|e| e.to_string())?;
    if !resp.ok() {
        return Err(format!("{url} -> {}", resp.status()));
    }
    Ok(())
}

#[derive(Serialize)]
struct ActiveReq {
    active: bool,
}

/// `POST /api/routines/:id/active` - `active: true` also clears any pause
/// reason server-side (`routes/routines.rs::post_routine_active` calls
/// `resume_routine` first).
pub async fn set_routine_active(id: &str, active: bool) -> Result<Routine, String> {
    let url = format!("/api/routines/{id}/active");
    let resp = Request::post(&url)
        .json(&ActiveReq { active })
        .map_err(|e| e.to_string())?
        .send()
        .await
        .map_err(|e| e.to_string())?;
    routine_result(resp).await
}

/// `DELETE /api/routines/:id`.
pub async fn delete_routine(id: &str) -> Result<(), String> {
    let url = format!("/api/routines/{id}");
    let resp = Request::delete(&url)
        .send()
        .await
        .map_err(|e| e.to_string())?;
    if !resp.ok() {
        return Err(format!("{url} -> {}", resp.status()));
    }
    Ok(())
}

/// `GET /api/routines/:id/runs` - the last 20 runs, newest first.
pub async fn fetch_routine_runs(id: &str) -> Result<Vec<RoutineRun>, String> {
    #[derive(Deserialize, Default)]
    struct RunsField {
        #[serde(default)]
        runs: Vec<RoutineRun>,
    }
    let url = format!("/api/routines/{id}/runs");
    let resp = Request::get(&url).send().await.map_err(|e| e.to_string())?;
    if !resp.ok() {
        return Err(format!("{url} -> {}", resp.status()));
    }
    resp.json::<RunsField>()
        .await
        .map(|b| b.runs)
        .map_err(|e| e.to_string())
}

/// `POST /api/routines/preview {"schedule": "<phrase>"}` ->
/// `{"schedule": {...}, "scheduleText": "..."}` (200) or `{"error": "..."}`
/// (400). S5-F-02 (F5): the create/edit form's live "-> description" now
/// debounces into this route instead of mirroring the schedule grammar
/// client-side (`routines_editor.rs`'s deleted `preview_schedule`) - the
/// server's own parser decides, so the preview can never show green over a
/// save the server would actually reject.
pub async fn preview_schedule(schedule: &str) -> Result<String, String> {
    #[derive(Serialize)]
    struct PreviewReq<'a> {
        schedule: &'a str,
    }
    #[derive(Deserialize)]
    #[serde(rename_all = "camelCase")]
    struct PreviewOk {
        schedule_text: String,
    }
    let resp = Request::post("/api/routines/preview")
        .json(&PreviewReq { schedule })
        .map_err(|e| e.to_string())?
        .send()
        .await
        .map_err(|e| e.to_string())?;
    if !resp.ok() {
        // S13a-01: see `put_model_field`'s comment on why `status` must be
        // captured before `.json()`.
        let status = resp.status();
        return match resp.json::<RoutineError>().await {
            Ok(err) => Err(err.error),
            Err(_) => Err(format!("/api/routines/preview -> {status}")),
        };
    }
    resp.json::<PreviewOk>()
        .await
        .map(|b| b.schedule_text)
        .map_err(|e| e.to_string())
}

/// `POST /api/routines/:id/run` - "Run now": fires regardless of schedule
/// or active state, 201 with `{"runId": "..."}`. Not in the TS reference
/// (`RoutinesEditor.tsx` has no such button) - this ticket adds it since
/// the route already exists (S5-03/04) and a routine that starts paused
/// with nothing to show yet is a worse first run than one Josh can poke.
pub async fn run_routine_now(id: &str) -> Result<String, String> {
    #[derive(Deserialize)]
    #[serde(rename_all = "camelCase")]
    struct RunIdField {
        run_id: String,
    }
    let url = format!("/api/routines/{id}/run");
    let resp = Request::post(&url)
        .send()
        .await
        .map_err(|e| e.to_string())?;
    if !resp.ok() {
        // S13a-01: see `put_model_field`'s comment on why `status` must be
        // captured before `.json()`.
        let status = resp.status();
        return match resp.json::<RoutineError>().await {
            Ok(err) => Err(err.error),
            Err(_) => Err(format!("{url} -> {status}")),
        };
    }
    resp.json::<RunIdField>()
        .await
        .map(|b| b.run_id)
        .map_err(|e| e.to_string())
}

/* ----------------------------------------------------------- S5b-07: goals */

/// `GET /api/goals?bot=...`'s response shape.
#[derive(Deserialize, Default)]
struct GoalsField {
    #[serde(default)]
    goals: Vec<Goal>,
}

/// `POST /api/goals` and `PATCH /api/goals/:id`'s success shape
/// (`crates/server/src/routes/goals.rs` always answers `{"goal": {...}}` on
/// both).
#[derive(Deserialize)]
struct GoalField {
    goal: Goal,
}

/// A rejected create/patch's shape, e.g. "Say what the goal is." or the
/// done-needs-a-note refusal: `{"error": "..."}"`.
#[derive(Deserialize)]
struct GoalError {
    error: String,
}

async fn goal_result(resp: Response) -> Result<Goal, String> {
    if resp.ok() {
        let body = resp.json::<GoalField>().await.map_err(|e| e.to_string())?;
        return Ok(body.goal);
    }
    let status = resp.status();
    match resp.json::<GoalError>().await {
        Ok(err) => Err(err.error),
        Err(_) => Err(format!("/api/goals -> {status}")),
    }
}

/// `GET /api/goals?bot=:botId` - one bot's goals only, same `bot` query
/// param `GET /api/routines` reads.
pub async fn fetch_goals(bot_id: &str) -> Result<Vec<Goal>, String> {
    let url = format!("/api/goals?bot={bot_id}");
    let resp = Request::get(&url).send().await.map_err(|e| e.to_string())?;
    if !resp.ok() {
        return Err(format!("{url} -> {}", resp.status()));
    }
    resp.json::<GoalsField>()
        .await
        .map(|b| b.goals)
        .map_err(|e| e.to_string())
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct CreateGoalReq<'a> {
    bot_id: &'a str,
    objective: &'a str,
    done_when: &'a str,
    #[serde(skip_serializing_if = "Option::is_none")]
    budget_tokens: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    budget_until: Option<&'a str>,
}

/// `POST /api/goals`.
pub async fn create_goal(
    bot_id: &str,
    objective: &str,
    done_when: &str,
    budget_tokens: Option<f64>,
    budget_until: Option<&str>,
) -> Result<Goal, String> {
    let resp = Request::post("/api/goals")
        .json(&CreateGoalReq {
            bot_id,
            objective,
            done_when,
            budget_tokens,
            budget_until,
        })
        .map_err(|e| e.to_string())?
        .send()
        .await
        .map_err(|e| e.to_string())?;
    goal_result(resp).await
}

/// `PATCH /api/goals/:id` - a raw JSON patch, same reason
/// `routes/goals.rs::patch_goal` reads a raw `serde_json::Value` server-side:
/// `budgetTokens`/`budgetUntil` need "key absent" (leave), "key + null"
/// (clear) and "key + value" (set) to stay three distinguishable cases,
/// which a typed `Option<Option<T>>` struct field cannot reproduce through
/// serde's derive on ITS OWN either - see that module's doc. `goals_editor.rs`
/// builds the object with `serde_json::json!` at each call site instead of a
/// second typed struct here, since every caller already knows exactly which
/// keys it wants present.
pub async fn patch_goal(id: &str, body: serde_json::Value) -> Result<Goal, String> {
    let url = format!("/api/goals/{id}");
    let resp = Request::patch(&url)
        .json(&body)
        .map_err(|e| e.to_string())?
        .send()
        .await
        .map_err(|e| e.to_string())?;
    goal_result(resp).await
}

/// `DELETE /api/goals/:id`.
pub async fn delete_goal(id: &str) -> Result<(), String> {
    let url = format!("/api/goals/{id}");
    let resp = Request::delete(&url)
        .send()
        .await
        .map_err(|e| e.to_string())?;
    if !resp.ok() {
        return Err(format!("{url} -> {}", resp.status()));
    }
    Ok(())
}

/// `GET /api/goals/:id/runs` - the last 20 sessions, newest first. Reuses
/// `RoutineRun` (see that type's own doc) - `store::goals::GoalRun` is a
/// byte-identical wire shape to `store::RoutineRun`.
pub async fn fetch_goal_runs(id: &str) -> Result<Vec<RoutineRun>, String> {
    #[derive(Deserialize, Default)]
    struct RunsField {
        #[serde(default)]
        runs: Vec<RoutineRun>,
    }
    let url = format!("/api/goals/{id}/runs");
    let resp = Request::get(&url).send().await.map_err(|e| e.to_string())?;
    if !resp.ok() {
        return Err(format!("{url} -> {}", resp.status()));
    }
    resp.json::<RunsField>()
        .await
        .map(|b| b.runs)
        .map_err(|e| e.to_string())
}

/// `POST /api/goals/:id/run` - "Run now": fires regardless of the goal's own
/// schedule, 201 with `{"runId": "..."}"`. Same posture as
/// `run_routine_now` above.
pub async fn run_goal_now(id: &str) -> Result<String, String> {
    #[derive(Deserialize)]
    #[serde(rename_all = "camelCase")]
    struct RunIdField {
        run_id: String,
    }
    let url = format!("/api/goals/{id}/run");
    let resp = Request::post(&url)
        .send()
        .await
        .map_err(|e| e.to_string())?;
    if !resp.ok() {
        // S13a-01: see `put_model_field`'s comment on why `status` must be
        // captured before `.json()`.
        let status = resp.status();
        return match resp.json::<GoalError>().await {
            Ok(err) => Err(err.error),
            Err(_) => Err(format!("{url} -> {status}")),
        };
    }
    resp.json::<RunIdField>()
        .await
        .map(|b| b.run_id)
        .map_err(|e| e.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn feed_parses_a_complete_frame() {
        let mut buffer = Vec::new();
        let events = feed(&mut buffer, b"data: {\"type\":\"delta\",\"text\":\"hi\"}\n");
        assert_eq!(events, vec![StreamEvent::Delta { text: "hi".into() }]);
        assert!(buffer.is_empty(), "the complete line must not linger");
    }

    #[test]
    fn feed_ignores_non_data_lines_and_blank_data() {
        let mut buffer = Vec::new();
        let events = feed(&mut buffer, b": ping\ndata: \ndata:  \n");
        assert!(events.is_empty());
    }

    /// F7's bite check: fold `"error"` back into the `_ => StreamEvent::Ignored`
    /// catch-all in `parse_frame` and this goes red - a run that fails
    /// (`{"type":"error","message":"..."}`, `routes/messages.rs::run_event_json`)
    /// would once again vanish instead of reaching `thread.rs`'s bubble.
    #[test]
    fn feed_parses_an_error_frame() {
        let mut buffer = Vec::new();
        let events = feed(
            &mut buffer,
            b"data: {\"type\":\"error\",\"message\":\"No OpenRouter key configured.\"}\n",
        );
        assert_eq!(
            events,
            vec![StreamEvent::Error {
                message: "No OpenRouter key configured.".into()
            }]
        );
    }

    /// The bite check: break `feed`'s buffering (e.g. reset `buffer` at the
    /// top of the function instead of extending it) and this goes red - the
    /// frame is split exactly where a `fetch` body chunk boundary can land.
    #[test]
    fn feed_holds_a_partial_frame_across_a_chunk_boundary() {
        let mut buffer = Vec::new();
        let first = feed(&mut buffer, b"data: {\"type\":\"delta\",\"te");
        assert!(first.is_empty(), "no complete line yet");
        let second = feed(&mut buffer, b"xt\":\"hi\"}\n");
        assert_eq!(second, vec![StreamEvent::Delta { text: "hi".into() }]);
    }

    #[test]
    fn feed_assembles_a_run_then_two_deltas_then_done_across_arbitrary_chunks() {
        let mut buffer = Vec::new();
        let mut all = Vec::new();
        for chunk in [
            b"data: {\"type\":\"run\",\"runId\":\"r1\"}\ndata: {\"typ".as_slice(),
            b"e\":\"delta\",\"text\":\"a\"}\ndata: {\"type\":\"delta\",\"text\":\"b\"}\n"
                .as_slice(),
            b"data: {\"type\":\"done\",\"model\":\"mock\"}\n".as_slice(),
        ] {
            all.extend(feed(&mut buffer, chunk));
        }
        assert_eq!(
            all,
            vec![
                StreamEvent::Run {
                    run_id: "r1".into()
                },
                StreamEvent::Delta { text: "a".into() },
                StreamEvent::Delta { text: "b".into() },
                StreamEvent::Done {
                    model: Some("mock".into())
                },
            ]
        );
    }
}
