//! Types both halves of Bullpen use. Port of `src/shared` in the TS original.

pub mod ask_josh;
pub mod faces;
pub mod mentions;
pub mod nothing_new;
pub mod working;

use serde::{Deserialize, Serialize};
use std::str::FromStr;

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Bot {
    pub id: String,
    pub name: String,
    pub purpose: String,
    pub instructions: String,
    pub model: Option<String>,
    pub archived: bool,
    pub has_routine: bool,
    pub section_id: Option<String>,
    pub pinned: bool,
    pub hidden: bool,
    pub avatar: Option<String>,
    pub shape: Option<String>,
    pub effort: Effort,
    pub is_template: bool,
    pub voice: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RosterEntry {
    pub id: String,
    pub name: String,
    pub purpose: String,
    pub instructions: String,
    pub model: Option<String>,
    pub archived: bool,
    pub has_routine: bool,
    pub section_id: Option<String>,
    pub pinned: bool,
    pub hidden: bool,
    pub avatar: Option<String>,
    pub shape: Option<String>,
    pub effort: Effort,
    pub is_template: bool,
    pub voice: Option<String>,
    pub unread: u32,
    pub preview: String,
    pub last_at: Option<String>,
    pub busy: bool,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum Effort {
    Low,
    Medium,
    High,
}

impl Effort {
    pub fn as_str(&self) -> &str {
        match self {
            Effort::Low => "low",
            Effort::Medium => "medium",
            Effort::High => "high",
        }
    }
}

impl FromStr for Effort {
    type Err = ();

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        Ok(match s {
            "low" => Effort::Low,
            "high" => Effort::High,
            _ => Effort::Medium,
        })
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Section {
    pub id: String,
    pub name: String,
    pub position: i32,
}

/// One message in a conversation or room. Port of the TS `Message`
/// (`src/shared/types.ts`).
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Message {
    pub id: String,
    /// `"user"` or `"assistant"`.
    pub role: String,
    pub content: String,
    /// The model that actually produced this, resolved by the provider.
    pub model: Option<String>,
    /// Set when the run failed. Carries the real upstream error.
    pub error: Option<String>,
    pub created_at: String,
    /// Set when a file came with this message.
    pub attachment_id: Option<String>,
    /// Which bot said it, when that is not the conversation's owner. `None`
    /// for almost every message.
    pub bot_id: Option<String>,
    /// Enough to draw the file without a second request per message.
    pub attachment: Option<MessageAttachment>,
    /// The key of the reaction Josh picked on this message. `None` (the
    /// ordinary case) draws no pill.
    pub reaction: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct MessageAttachment {
    pub id: String,
    pub name: String,
    pub content_type: String,
    pub bytes: i64,
}

/// The owner and room membership of one conversation. Port of the TS
/// `ConversationInfo` (`server/threads.ts`).
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Conversation {
    pub id: String,
    pub bot_id: String,
    /// `"chat"` or `"room"`.
    pub kind: String,
    /// Guest bot ids, when this is a room. Empty for an ordinary chat.
    pub members: Vec<String>,
}

/// One bot's conversation, as its own thread strip draws it. Port of the TS
/// `Thread` (`src/shared/types.ts`).
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ThreadSummary {
    pub id: String,
    pub bot_id: String,
    pub bot_name: String,
    pub title: String,
    pub message_count: i64,
    pub last_at: Option<String>,
    pub created_at: String,
    /// `"room"` only when other bots were named as members when the thread
    /// was made.
    pub kind: String,
    /// Guest bot ids in this room, excluding the owner. Empty for an
    /// ordinary chat.
    pub members: Vec<String>,
}

/// A group chat, as the rail's own GROUP CHATS section draws it. Port of the
/// TS `RoomSummary` (`src/shared/types.ts`).
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RoomSummary {
    pub id: String,
    pub title: String,
    /// Every bot in the room, in the order Josh picked them. The first id is
    /// the conversation's owner in the database - an implementation detail
    /// the rail never shows, but also the bot id a client posts messages to.
    pub member_ids: Vec<String>,
    /// Assistant messages that arrived since Josh last opened this room.
    pub unread: u32,
    /// First line of the most recent message, whoever sent it.
    pub preview: String,
    pub last_at: Option<String>,
    pub created_at: String,
}
