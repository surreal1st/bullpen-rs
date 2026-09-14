//! Roster JSON shape from `/api/roster`. Kept local to `client` rather than
//! `crates/shared` - S0-05 is not allowed to touch `shared` (another
//! builder owns `store`/`server`/`model`), and the real shared type lands
//! whenever S0-04 does.

use serde::{Deserialize, Serialize};

/// Who said a message. Ported from `Message.role` in `shared/types.ts`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Role {
    User,
    Assistant,
}

/// One message in a conversation, from `GET /api/bots/:id/conversation`.
/// A strict subset of `Message` in `shared/types.ts:101-134` - S1-07a's
/// bubble has no use yet for `attachment`, `reaction` or the guest `botId`
/// (no rooms, no attachments: see the ticket's "SKIP" list), so they are
/// left off rather than carried as unused fields.
#[derive(Debug, Clone, PartialEq, Deserialize)]
pub struct Message {
    pub id: String,
    pub role: Role,
    pub content: String,
    #[serde(default)]
    pub model: Option<String>,
    #[serde(default)]
    pub error: Option<String>,
    #[serde(rename = "createdAt")]
    pub created_at: String,
}

/// `GET /api/bots/:id/conversation`'s response shape. A strict subset of
/// `ConversationView` in `shared/types.ts` - `effectiveModel` etc. are for
/// the off-model badge, out of scope here (see `bubble.rs`'s doc comment).
#[derive(Debug, Clone, PartialEq, Deserialize)]
pub struct ConversationView {
    #[serde(rename = "conversationId")]
    pub conversation_id: String,
    pub messages: Vec<Message>,
}

#[derive(Debug, Clone, PartialEq, Deserialize)]
pub struct Section {
    pub id: String,
    pub name: String,
}

#[derive(Debug, Clone, PartialEq, Deserialize)]
pub struct Bot {
    pub id: String,
    pub name: String,
    #[serde(default)]
    pub purpose: String,
    #[serde(rename = "sectionId", default)]
    pub section_id: Option<String>,
    #[serde(default)]
    pub pinned: bool,
    #[serde(default)]
    pub hidden: bool,
    #[serde(default)]
    pub avatar: Option<String>,
    #[serde(default)]
    pub shape: Option<String>,
    #[serde(default)]
    pub busy: bool,
    #[serde(default)]
    pub unread: u32,
    #[serde(default)]
    pub preview: Option<String>,
    // S1-07b: fixed from `Option<i64>` - the real server (S1-06) sends an
    // ISO-8601 string here, same as `RoomSummary.last_at` below. S0-05
    // guessed epoch millis before a real server existed to check against;
    // `scripts/mock-roster.mjs`'s own fixture only ever sent `null`, so
    // nothing caught it until a roster fetch here hit a bot with a real
    // `lastAt` and failed outright ("Roster failed to load: invalid type:
    // string ..., expected i64").
    #[serde(rename = "lastAt", default)]
    pub last_at: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Deserialize, Default)]
pub struct Roster {
    #[serde(default)]
    pub sections: Vec<Section>,
    #[serde(default)]
    pub bots: Vec<Bot>,
}

/// A group chat, as the rail's GROUP CHATS section draws it. Mirrors
/// `shared::RoomSummary` (`crates/shared/src/lib.rs`) - kept local for the
/// same reason `Bot`/`Section` are: this crate does not depend on `shared`.
#[derive(Debug, Clone, PartialEq, Deserialize)]
pub struct RoomSummary {
    pub id: String,
    pub title: String,
    /// Every bot in the room, in the order Josh picked them. The first id is
    /// the conversation's owner - the bot id a client posts messages to and
    /// reads `/api/bots/:id/conversation?thread=<roomId>` through.
    #[serde(rename = "memberIds")]
    pub member_ids: Vec<String>,
    #[serde(default)]
    pub unread: u32,
    #[serde(default)]
    pub preview: String,
    #[serde(rename = "lastAt", default)]
    pub last_at: Option<String>,
    #[serde(rename = "createdAt")]
    pub created_at: String,
}

/// `GET /api/rooms`'s response shape.
#[derive(Debug, Clone, Deserialize, Default)]
pub struct RoomsResponse {
    #[serde(default)]
    pub rooms: Vec<RoomSummary>,
}

/// `POST /api/rooms` and `PATCH /api/rooms/:id`'s success shape.
#[derive(Debug, Clone, Deserialize)]
pub struct RoomResponse {
    pub room: RoomSummary,
}

/// One bot's line for the working indicator, from
/// `GET /api/conversations/:id/working`. Mirrors `shared::working::WorkingBot`.
/// `Serialize` too - `working_bar.rs` re-serialises a fetched list to
/// compare it against the last one it rendered (the TS `JSON.stringify`
/// posture ported byte-for-byte, see that module's doc).
#[derive(Debug, Clone, PartialEq, Deserialize, Serialize)]
pub struct WorkingBot {
    #[serde(rename = "botId")]
    pub bot_id: String,
    pub name: String,
    #[serde(default)]
    pub avatar: Option<String>,
    #[serde(rename = "sectionId", default)]
    pub section_id: Option<String>,
    #[serde(default)]
    pub shape: Option<String>,
    pub activity: String,
    /// Parked on an approval rather than running - a still face, not an
    /// animating one.
    pub waiting: bool,
}

/// `GET /api/conversations/:id/working`'s response shape.
#[derive(Debug, Clone, Deserialize, Default)]
pub struct WorkingResponse {
    #[serde(default)]
    pub working: Vec<WorkingBot>,
}

/// `GET /api/auth/status`'s response shape - the gate's first question on
/// every boot (`app.rs`, ported from `Gate.tsx:67-77`). `role` is not
/// carried: this client has no member-vs-owner distinction yet (S5b's
/// invites, `role` in the TS response, are out of scope for S1-F-11).
#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AuthStatus {
    pub configured: bool,
    pub signed_in: bool,
}
