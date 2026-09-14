use crate::Db;
use shared::{Bot, Effort, RosterEntry};
use std::collections::HashMap;

struct RailRow {
    bot_id: String,
    unread: u32,
    preview: Option<String>,
    last_at: Option<String>,
    busy: i32,
}

#[derive(Clone)]
struct BotRow {
    id: String,
    name: String,
    purpose: String,
    instructions: String,
    model: Option<String>,
    archived_at: Option<String>,
    has_routine: i32,
    section_id: Option<String>,
    pinned_at: Option<String>,
    hidden_at: Option<String>,
    avatar: Option<String>,
    shape: Option<String>,
    effort: Option<String>,
    is_template: i32,
    voice: Option<String>,
}

fn row_to_bot(row: BotRow) -> Bot {
    Bot {
        id: row.id,
        name: row.name,
        purpose: row.purpose,
        instructions: row.instructions,
        model: row.model,
        archived: row.archived_at.is_some(),
        has_routine: row.has_routine == 1,
        section_id: row.section_id,
        pinned: row.pinned_at.is_some(),
        hidden: row.hidden_at.is_some(),
        avatar: row.avatar,
        shape: row.shape,
        effort: row
            .effort
            .as_deref()
            .unwrap_or("medium")
            .parse()
            .unwrap_or(Effort::Medium),
        is_template: row.is_template == 1,
        voice: row.voice,
    }
}

/// Strip markdown and truncate to 140 chars. Matches TS `firstLine` function.
/// `pub(crate)`: `rooms.rs` reuses it for a room's own preview line.
pub(crate) fn first_line(content: &str) -> String {
    let lines: Vec<&str> = content
        .split('\n')
        .map(|l| l.trim())
        .filter(|l| !l.is_empty() && !is_hr(l))
        .collect();

    if lines.is_empty() {
        return String::new();
    }

    let mut line = lines[0].to_string();

    // Remove headings (1-6 hashes followed by space)
    let trimmed = line.trim_start_matches('#').trim_start();
    if trimmed != line {
        line = trimmed.to_string();
    }

    // Remove list markers
    if line.starts_with("- ") || line.starts_with("* ") || line.starts_with("+ ") {
        line = line.chars().skip(2).collect::<String>();
    }

    // Remove blockquote markers
    if line.starts_with("> ") {
        line = line.chars().skip(2).collect::<String>();
    } else if line.starts_with(">") {
        line = line.chars().skip(1).collect::<String>();
    }

    // Remove code formatting
    line = line.replace("```", "").replace("`", "");

    // Remove bold
    line = line.replace("**", "");
    line = line.replace("__", "");

    // Remove italic
    line = line.replace("*", "").replace("_", "");

    // Remove links
    while let Some(start) = line.find('[') {
        if let Some(end) = line.find(']') {
            // B1: a preview line can carry a `]` that has nothing to do with
            // this `[` (a bot replying `Done] see [docs](https://x)` finds
            // `]` at index 4, `[` at index 13) - slicing `start + 1..end`
            // there is an out-of-order byte range and panics. Bail out of
            // the link-removal pass instead of taking the string apart.
            if end <= start {
                break;
            }
            if let Some(paren_start) = line[end..].find('(') {
                let paren_end = line[end + paren_start..].find(')');
                if let Some(paren_end) = paren_end {
                    let link_text = &line[start + 1..end];
                    line = format!(
                        "{}{}{}",
                        &line[..start],
                        link_text,
                        &line[end + paren_start + paren_end + 1..]
                    );
                    continue;
                }
            }
            break;
        }
        break;
    }

    line.trim().chars().take(140).collect()
}

/// Check if a line is a horizontal rule (3+ dashes, asterisks, or underscores)
fn is_hr(line: &str) -> bool {
    if line.len() < 3 {
        return false;
    }
    line.chars().all(|c| c == '-' || c == '*' || c == '_')
}

/// List all non-archived bots as roster entries with unread counts, preview, lastAt, and busy status.
pub fn list_roster(db: &Db) -> rusqlite::Result<Vec<RosterEntry>> {
    // First, fetch the rail data (unread, preview, last_at, busy) for each bot
    let mut stmt = db.conn().prepare(
        "SELECT b.id AS bot_id,
                (SELECT COUNT(*)
                   FROM messages m
                   JOIN conversations c ON c.id = m.conversation_id
                  WHERE c.bot_id = b.id
                    AND c.kind != 'room'
                    AND m.role = 'assistant'
                    AND m.created_at > COALESCE(b.last_seen_at, '')) AS unread,
                (SELECT m.content
                   FROM messages m
                   JOIN conversations c ON c.id = m.conversation_id
                  WHERE c.bot_id = b.id
                    AND c.kind != 'room'
                  ORDER BY m.created_at DESC, m.seq DESC LIMIT 1) AS preview,
                (SELECT m.created_at
                   FROM messages m
                   JOIN conversations c ON c.id = m.conversation_id
                  WHERE c.bot_id = b.id
                    AND c.kind != 'room'
                  ORDER BY m.created_at DESC, m.seq DESC LIMIT 1) AS last_at,
                (SELECT COUNT(*)
                   FROM runs r
                  WHERE r.bot_id = b.id
                    AND r.status IN ('running', 'waiting')) AS busy
           FROM bots b
          WHERE b.archived_at IS NULL",
    )?;

    let rail_rows = stmt
        .query_map([], |row| {
            Ok(RailRow {
                bot_id: row.get(0)?,
                unread: row.get(1)?,
                preview: row.get(2)?,
                last_at: row.get(3)?,
                busy: row.get(4)?,
            })
        })?
        .collect::<Result<Vec<_>, _>>()?;

    let by_bot: HashMap<String, RailRow> = rail_rows
        .into_iter()
        .map(|r| (r.bot_id.clone(), r))
        .collect();

    // Fetch all non-archived bots
    let mut stmt = db.conn().prepare(
        "SELECT id, name, purpose, instructions, model, archived_at, has_routine,
                section_id, pinned_at, hidden_at, avatar, shape, effort, is_template, voice
         FROM bots WHERE archived_at IS NULL ORDER BY name",
    )?;

    let bots = stmt
        .query_map([], |row| {
            Ok(BotRow {
                id: row.get(0)?,
                name: row.get(1)?,
                purpose: row.get(2)?,
                instructions: row.get(3)?,
                model: row.get(4)?,
                archived_at: row.get(5)?,
                has_routine: row.get(6)?,
                section_id: row.get(7)?,
                pinned_at: row.get(8)?,
                hidden_at: row.get(9)?,
                avatar: row.get(10)?,
                shape: row.get(11)?,
                effort: row.get(12)?,
                is_template: row.get(13)?,
                voice: row.get(14)?,
            })
        })?
        .collect::<Result<Vec<_>, _>>()?;

    // Map bots to roster entries
    let roster = bots
        .into_iter()
        .map(|bot_row| {
            let bot = row_to_bot(bot_row.clone());
            let rail = by_bot.get(&bot.id);
            let preview = rail
                .and_then(|r| r.preview.as_deref())
                .map(first_line)
                .unwrap_or_default();

            RosterEntry {
                id: bot.id,
                name: bot.name,
                purpose: bot.purpose,
                instructions: bot.instructions,
                model: bot.model,
                archived: bot.archived,
                has_routine: bot.has_routine,
                section_id: bot.section_id,
                pinned: bot.pinned,
                hidden: bot.hidden,
                avatar: bot.avatar,
                shape: bot.shape,
                effort: bot.effort,
                is_template: bot.is_template,
                voice: bot.voice,
                unread: rail.map(|r| r.unread).unwrap_or(0),
                preview,
                last_at: rail.and_then(|r| r.last_at.clone()),
                busy: rail.map(|r| r.busy > 0).unwrap_or(false),
            }
        })
        .collect();

    Ok(roster)
}
