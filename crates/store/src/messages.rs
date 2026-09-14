//! Messages within a conversation. Port of the TS `store.ts`'s
//! `appendMessage`, `listMessages`, `deleteMessage`, `rowToMessage`.

use crate::Db;
use crate::conversations::now_iso;
use rusqlite::params;
use shared::{Message, MessageAttachment};

struct MessageRow {
    id: String,
    role: String,
    content: String,
    model: Option<String>,
    error: Option<String>,
    created_at: String,
    attachment_id: Option<String>,
    bot_id: Option<String>,
    reactions: Option<String>,
}

/// The single active reaction key, or `None`. Never fails on a malformed
/// value - it just reads as unreacted, matching the TS `parseReaction`.
fn parse_reaction(raw: Option<&str>) -> Option<String> {
    let raw = raw?;
    if raw.is_empty() {
        return None;
    }
    let value: serde_json::Value = serde_json::from_str(raw).ok()?;
    value.get("emoji")?.as_str().map(str::to_string)
}

fn row_to_message(row: MessageRow, attachment: Option<MessageAttachment>) -> Message {
    Message {
        id: row.id,
        role: row.role,
        content: row.content,
        model: row.model,
        error: row.error,
        created_at: row.created_at,
        attachment_id: row.attachment_id,
        bot_id: row.bot_id,
        attachment,
        reaction: parse_reaction(row.reactions.as_deref()),
    }
}

/// Every message in a conversation, oldest first. Joined against
/// `attachments` so a client with twenty screenshots in it draws them
/// without twenty extra round trips. Mirrors the TS `listMessages`.
pub fn list_messages(db: &Db, conversation_id: &str) -> rusqlite::Result<Vec<Message>> {
    let mut stmt = db.conn().prepare(
        "SELECT m.id, m.role, m.content, m.model, m.error, m.created_at, m.attachment_id, m.bot_id, m.reactions,
                a.name, a.content_type, a.bytes
           FROM messages m
           LEFT JOIN attachments a ON a.id = m.attachment_id
          WHERE m.conversation_id = ?1 ORDER BY m.seq",
    )?;

    let rows = stmt
        .query_map(params![conversation_id], |row| {
            let message = MessageRow {
                id: row.get(0)?,
                role: row.get(1)?,
                content: row.get(2)?,
                model: row.get(3)?,
                error: row.get(4)?,
                created_at: row.get(5)?,
                attachment_id: row.get(6)?,
                bot_id: row.get(7)?,
                reactions: row.get(8)?,
            };
            let a_name: Option<String> = row.get(9)?;
            let a_type: Option<String> = row.get(10)?;
            let a_bytes: Option<i64> = row.get(11)?;
            Ok((message, a_name, a_type, a_bytes))
        })?
        .collect::<Result<Vec<_>, _>>()?;

    Ok(rows
        .into_iter()
        .map(|(row, a_name, a_type, a_bytes)| {
            let attachment = match (&row.attachment_id, a_name) {
                (Some(id), Some(name)) => Some(MessageAttachment {
                    id: id.clone(),
                    name,
                    content_type: a_type.unwrap_or_else(|| "application/octet-stream".to_string()),
                    bytes: a_bytes.unwrap_or(0),
                }),
                _ => None,
            };
            row_to_message(row, attachment)
        })
        .collect())
}

/// Usage reported by the provider for one message. Cost is PROVIDER-REPORTED
/// per request, never computed here - see migration 3's comment in
/// `migrations.rs`.
#[derive(Debug, Clone, Default)]
pub struct Usage {
    pub cost_usd: f64,
    pub input_tokens: i64,
    pub output_tokens: i64,
    pub cached_tokens: i64,
}

/// What a caller supplies beyond the required role/content. Mirrors the
/// optional fields on the TS `appendMessage`'s `message` argument.
#[derive(Debug, Clone, Default)]
pub struct NewMessage {
    pub model: Option<String>,
    pub error: Option<String>,
    /// Set when a file came with this message.
    pub attachment_id: Option<String>,
    /// Which bot said it, when that is not the conversation's owner. `None`
    /// for almost every message.
    pub bot_id: Option<String>,
    pub usage: Option<Usage>,
}

/// Appends one message, assigning it the next `seq` in the conversation.
/// Mirrors the TS `appendMessage`.
pub fn append_message(
    db: &Db,
    conversation_id: &str,
    role: &str,
    content: &str,
    extra: NewMessage,
) -> rusqlite::Result<Message> {
    let next_seq: i64 = db.conn().query_row(
        "SELECT COALESCE(MAX(seq), 0) + 1 FROM messages WHERE conversation_id = ?1",
        params![conversation_id],
        |row| row.get(0),
    )?;

    let id = uuid::Uuid::new_v4().to_string();
    let created_at = now_iso();

    db.conn().execute(
        "INSERT INTO messages (id, conversation_id, seq, role, content, model, error, created_at,
                                attachment_id, bot_id, cost_usd, input_tokens, output_tokens, cached_tokens)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14)",
        params![
            id,
            conversation_id,
            next_seq,
            role,
            content,
            extra.model,
            extra.error,
            created_at,
            extra.attachment_id,
            extra.bot_id,
            extra.usage.as_ref().map(|u| u.cost_usd),
            extra.usage.as_ref().map(|u| u.input_tokens),
            extra.usage.as_ref().map(|u| u.output_tokens),
            extra.usage.as_ref().map(|u| u.cached_tokens),
        ],
    )?;

    Ok(Message {
        id,
        role: role.to_string(),
        content: content.to_string(),
        model: extra.model,
        error: extra.error,
        created_at,
        attachment_id: extra.attachment_id,
        bot_id: extra.bot_id,
        // Matches the TS return value: a brand new message has no
        // attachment object built (only `attachmentId`) and has never been
        // reacted to.
        attachment: None,
        reaction: None,
    })
}

/// Removes one message outright, rather than editing it in place. Mirrors
/// the TS `deleteMessage`; generic on purpose, nothing about it is
/// room-specific.
pub fn delete_message(db: &Db, id: &str) -> rusqlite::Result<()> {
    db.conn()
        .execute("DELETE FROM messages WHERE id = ?1", params![id])?;
    Ok(())
}
