//! Fetch helpers for a bot's conversation and the run stream, ported from
//! `projects/bullpen-night/src/client/App.tsx:315-380` (loading a
//! conversation), `:470-601` (`send`: POST messages, the SSE loop) and
//! `:1424-1449` (`readSse`).

use crate::types::{
    ApprovalsResponse, AuthStatus, ConversationView, OpenQuestion, PendingApproval,
    QuestionsResponse, RoomResponse, RoomSummary, RoomsResponse, WorkingBot, WorkingResponse,
};
use gloo_net::http::{Request, Response};
use serde::{Deserialize, Serialize};

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
/// server answers with a `Set-Cookie` on success; `credentials: include` is
/// belt-and-suspenders for a same-origin fetch (the browser default already
/// sends cookies here), kept explicit per the ticket's own instruction so a
/// future reverse-proxy split between client and API origins does not
/// silently drop the session cookie.
pub async fn login(password: &str) -> Result<(), String> {
    let resp = Request::post("/api/auth/login")
        .credentials(web_sys::RequestCredentials::Include)
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
#[cfg(feature = "web")]
pub async fn send_message(
    bot_id: &str,
    text: &str,
    thread_id: Option<&str>,
    mut on_event: impl FnMut(StreamEvent),
) -> Result<(), String> {
    use wasm_bindgen::{JsCast, JsValue};
    use wasm_bindgen_futures::JsFuture;
    use web_sys::ReadableStreamDefaultReader;

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

    let content_type = resp.headers().get("content-type").unwrap_or_default();
    if content_type.contains("application/json") {
        return Ok(());
    }

    let stream = resp
        .body()
        .ok_or_else(|| "send returned no body".to_string())?;
    let reader: ReadableStreamDefaultReader = stream.get_reader().unchecked_into();

    let mut buffer = Vec::new();
    loop {
        let result = JsFuture::from(reader.read())
            .await
            .map_err(|e| format!("{e:?}"))?;
        let done = js_sys::Reflect::get(&result, &JsValue::from_str("done"))
            .map_err(|e| format!("{e:?}"))?
            .as_bool()
            .unwrap_or(true);
        if done {
            break;
        }
        let value = js_sys::Reflect::get(&result, &JsValue::from_str("value"))
            .map_err(|e| format!("{e:?}"))?;
        let chunk = js_sys::Uint8Array::new(&value).to_vec();
        for event in feed(&mut buffer, &chunk) {
            on_event(event);
        }
    }
    Ok(())
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
