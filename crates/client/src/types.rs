//! Roster JSON shape from `/api/roster`. Kept local to `client` rather than
//! `crates/shared` - S0-05 is not allowed to touch `shared` (another
//! builder owns `store`/`server`/`model`), and the real shared type lands
//! whenever S0-04 does.

use serde::Deserialize;

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
    #[serde(rename = "lastAt", default)]
    pub last_at: Option<i64>,
}

#[derive(Debug, Clone, PartialEq, Deserialize, Default)]
pub struct Roster {
    #[serde(default)]
    pub sections: Vec<Section>,
    #[serde(default)]
    pub bots: Vec<Bot>,
}
