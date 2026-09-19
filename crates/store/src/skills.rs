//! S10-01: skills - a reusable set of instructions for how to do a task,
//! written once and available to every bot. Port of `skills.ts` (247 lines,
//! `projects/bullpen-night/src/server/skills.ts`), read-only reference,
//! never edited from here.
//!
//! 🔴 HOW A SKILL REACHES A BOT, and why it is not simply pasted in. Every
//! enabled skill contributes ONE LINE to the prompt - its name and when to
//! use it (`skill_index_for`). The body arrives only when the bot calls
//! `use_skill`. Fifty skill bodies in every prompt is a per-session tax paid
//! in full on the first token, forever, and it buries the instructions that
//! matter under ones that do not apply.
//!
//! A skill is DATA, not authority. The body is instructions for doing a job;
//! it cannot grant a tool, widen a permission, or change what a bot may
//! reach. Those live in `server::permissions` and are decided per run, same
//! TS split.

use crate::Db;
use chrono::{DateTime, Utc};
use rusqlite::{OptionalExtension, params};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

// The `skills`/`bot_skills` tables are created by migration 23
// (`crate::migrations::MIGRATIONS`), not by a self-creating function called
// from `Db::open` the way `goals`/`slack`/`vms` are - unlike those, this
// ticket owns a numbered migration slot. Its DDL uses `CREATE TABLE IF NOT
// EXISTS` for the same reason those self-creating functions do: in the TS
// original these tables are self-creating (`ensureSkillTables`), so a
// database already opened by live Bullpen has them, and the migration must
// stay a no-op there rather than fail on an existing table.

/// Raw row shape from the `skills` table.
struct SkillRow {
    id: String,
    name: String,
    description: String,
    body: String,
    source: String,
    created_at: String,
    updated_at: String,
}

fn skill_row_from_row(row: &rusqlite::Row) -> rusqlite::Result<SkillRow> {
    Ok(SkillRow {
        id: row.get(0)?,
        name: row.get(1)?,
        description: row.get(2)?,
        body: row.get(3)?,
        source: row.get(4)?,
        created_at: row.get(5)?,
        updated_at: row.get(6)?,
    })
}

const SKILL_COLUMNS: &str = "id, name, description, body, source, created_at, updated_at";

/// Wire shape for a skill - camelCase to match the TS JSON a client expects.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Skill {
    pub id: String,
    pub name: String,
    /// When to use it. This is the line that reaches the prompt.
    pub description: String,
    /// How to do it. Loaded only on demand, via `use_skill`.
    pub body: String,
    /// "bullpen" | "claude-code" - kept as a plain string, same convention
    /// `Condition.kind` uses elsewhere in this crate, rather than an enum
    /// with its own (de)serialisation.
    pub source: String,
    pub created_at: String,
    pub updated_at: String,
}

/// TS `toSkill`: any `source` other than the literal `"claude-code"` reads
/// back as `"bullpen"`, so a hand-edited or pre-existing row with a stray
/// value never surfaces a third, unsupported source.
fn to_skill(row: SkillRow) -> Skill {
    Skill {
        id: row.id,
        name: row.name,
        description: row.description,
        body: row.body,
        source: if row.source == "claude-code" {
            "claude-code".to_string()
        } else {
            "bullpen".to_string()
        },
        created_at: row.created_at,
        updated_at: row.updated_at,
    }
}

/// A skill's name is how a bot asks for it, so it has to be typeable and
/// stable. Lowercased and hyphenated, matching the directory names Claude
/// Code uses. An empty result means "not usable" - callers (`save_skill`)
/// refuse it rather than writing a blank-named row.
pub fn normalize_name(raw: &str) -> String {
    let lowered = raw.trim().to_lowercase();
    let mut out = String::with_capacity(lowered.len());
    let mut last_was_dash = false;
    for ch in lowered.chars() {
        if ch.is_ascii_alphanumeric() {
            out.push(ch);
            last_was_dash = false;
        } else if !last_was_dash {
            out.push('-');
            last_was_dash = true;
        }
    }
    let trimmed = out.trim_matches('-');
    trimmed.chars().take(64).collect()
}

pub fn list_skills(db: &Db) -> rusqlite::Result<Vec<Skill>> {
    let mut stmt = db
        .conn()
        .prepare(&format!("SELECT {SKILL_COLUMNS} FROM skills ORDER BY name"))?;
    let rows: Vec<SkillRow> = stmt
        .query_map([], skill_row_from_row)?
        .collect::<Result<Vec<_>, _>>()?;
    Ok(rows.into_iter().map(to_skill).collect())
}

pub fn get_skill(db: &Db, name: &str) -> rusqlite::Result<Option<Skill>> {
    let normalized = normalize_name(name);
    let mut stmt = db.conn().prepare(&format!(
        "SELECT {SKILL_COLUMNS} FROM skills WHERE name = ?1"
    ))?;
    stmt.query_row(params![normalized], skill_row_from_row)
        .optional()
        .map(|row| row.map(to_skill))
}

/// Input to `save_skill`.
#[derive(Clone, Debug, Default)]
pub struct SkillInput {
    pub name: String,
    pub description: String,
    pub body: String,
    /// `None` defaults to "bullpen", same as TS's `input.source ?? "bullpen"`.
    pub source: Option<String>,
}

/// Writes a skill, replacing one of the same name.
///
/// 🔴 Upsert rather than insert, because the import is re-runnable by
/// design (TS's own doc on `saveSkill`): Josh's Claude Code skills change,
/// and re-importing must update them rather than failing on a unique
/// constraint or quietly making a second copy. Returns `Ok(None)` when the
/// name normalises to empty - "not usable" is a caller-visible outcome, not
/// a database error, matching TS's `Skill | null`.
pub fn save_skill(
    db: &Db,
    input: SkillInput,
    now: DateTime<Utc>,
) -> rusqlite::Result<Option<Skill>> {
    let name = normalize_name(&input.name);
    if name.is_empty() {
        return Ok(None);
    }

    let stamp = now.to_rfc3339();
    db.conn().execute(
        "INSERT INTO skills (id, name, description, body, source, created_at, updated_at)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?6)
         ON CONFLICT(name) DO UPDATE SET description = excluded.description,
                                         body        = excluded.body,
                                         source      = excluded.source,
                                         updated_at  = excluded.updated_at",
        params![
            Uuid::new_v4().to_string(),
            name,
            input.description.trim(),
            input.body,
            input.source.as_deref().unwrap_or("bullpen"),
            stamp,
        ],
    )?;

    get_skill(db, &name)
}

/// Deletes a skill, and the `bot_skills` rows that reference it - a bare
/// `DELETE FROM skills` would leave orphaned `bot_skills` rows behind since
/// SQLite enforces no foreign key between them (matching TS's two explicit
/// statements, not a `REFERENCES`+`ON DELETE CASCADE` this schema never
/// declared). Returns whether a skill was actually removed.
pub fn delete_skill(db: &Db, name: &str) -> rusqlite::Result<bool> {
    let Some(skill) = get_skill(db, name)? else {
        return Ok(false);
    };
    db.conn().execute(
        "DELETE FROM bot_skills WHERE skill_id = ?1",
        params![skill.id],
    )?;
    db.conn()
        .execute("DELETE FROM skills WHERE id = ?1", params![skill.id])?;
    Ok(true)
}

/* ------------------------------------------------------- who may use what */

/// Which skills a bot has switched on.
///
/// 🔴 Opt-IN, not opt-out (TS's own doc on `skillsFor`): fifty-nine imported
/// skills on every bot would put fifty-nine lines of "when to use this" in
/// front of a model that needs two. A bot gets the skills Josh gives it.
pub fn skills_for(db: &Db, bot_id: &str) -> rusqlite::Result<Vec<Skill>> {
    let mut stmt = db.conn().prepare(
        "SELECT s.id, s.name, s.description, s.body, s.source, s.created_at, s.updated_at
           FROM skills s
           JOIN bot_skills b ON b.skill_id = s.id
          WHERE b.bot_id = ?1
          ORDER BY s.name",
    )?;
    let rows: Vec<SkillRow> = stmt
        .query_map(params![bot_id], skill_row_from_row)?
        .collect::<Result<Vec<_>, _>>()?;
    Ok(rows.into_iter().map(to_skill).collect())
}

/// Switches one skill on or off for one bot. Returns `false` when `name`
/// does not match any skill (the route layer turns that into a 404), same
/// as TS's `setBotSkill`.
pub fn set_bot_skill(db: &Db, bot_id: &str, name: &str, on: bool) -> rusqlite::Result<bool> {
    let Some(skill) = get_skill(db, name)? else {
        return Ok(false);
    };

    if on {
        db.conn().execute(
            "INSERT OR IGNORE INTO bot_skills (bot_id, skill_id) VALUES (?1, ?2)",
            params![bot_id, skill.id],
        )?;
    } else {
        db.conn().execute(
            "DELETE FROM bot_skills WHERE bot_id = ?1 AND skill_id = ?2",
            params![bot_id, skill.id],
        )?;
    }
    Ok(true)
}

/* ---------------------------------------------------------- the prompt half */

/// How much of a description survives into the prompt.
const DESCRIPTION_CAP: usize = 240;

/// The one-line-per-skill index that goes in a bot's system prompt.
///
/// Returns `""` when the bot has no skills, so the prompt gains nothing at
/// all rather than an empty heading explaining that there is nothing to
/// say - same as TS's `skillIndexFor`.
pub fn skill_index_for(db: &Db, bot_id: &str) -> String {
    let skills = skills_for(db, bot_id).unwrap_or_default();
    if skills.is_empty() {
        return String::new();
    }

    let lines: Vec<String> = skills
        .iter()
        .map(|s| {
            let collapsed = s
                .description
                .split_whitespace()
                .collect::<Vec<_>>()
                .join(" ");
            let when: String = collapsed.chars().take(DESCRIPTION_CAP).collect();
            format!("- {}: {}", s.name, when)
        })
        .collect();

    let mut out = vec![
        "## Skills you can load".to_string(),
        String::new(),
        "Each line is a job someone has already worked out how to do. When one fits".to_string(),
        "what you have been asked, call `use_skill` with its name and follow what it".to_string(),
        "says. Do not guess at a skill that is not listed here.".to_string(),
        String::new(),
    ];
    out.extend(lines);
    out.join("\n")
}

/// What `use_skill` hands back.
///
/// 🔴 Names what IS available rather than only refusing (TS's own doc): a
/// bot told "no" with no alternative invents a plausible neighbour and
/// calls that instead.
pub fn read_skill(db: &Db, bot_id: &str, name: &str) -> String {
    let wanted = normalize_name(name);
    let allowed = skills_for(db, bot_id).unwrap_or_default();
    match allowed.iter().find(|s| s.name == wanted) {
        Some(skill) => format!("# {}\n\n{}", skill.name, skill.body),
        None => {
            if allowed.is_empty() {
                format!("No skill called \"{wanted}\", and this bot has none enabled.")
            } else {
                let names: Vec<&str> = allowed.iter().map(|s| s.name.as_str()).collect();
                format!(
                    "No skill called \"{wanted}\". You have: {}.",
                    names.join(", ")
                )
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_normalize_name() {
        assert_eq!(normalize_name("  Hello World!! "), "hello-world");
        assert_eq!(normalize_name("---"), "");
        assert_eq!(normalize_name(""), "");
        assert_eq!(normalize_name("a".repeat(100).as_str()).len(), 64);
        assert_eq!(normalize_name("already-normal"), "already-normal");
        assert_eq!(normalize_name("multi   space___run"), "multi-space-run");
    }

    /// Ticket bite #8: `delete_skill` must remove the `bot_skills` rows
    /// too, not just the `skills` row - otherwise an orphan row (a
    /// `skill_id` that no `skills` row references any more) is left
    /// behind forever. An HTTP `GET` cannot prove this either way (the
    /// JOIN in `skills_for` excludes an orphan the same as a properly
    /// cleaned-up one), so this has to read `bot_skills` directly.
    #[test]
    fn delete_skill_removes_bot_skills_rows_too() {
        let db = Db::open(":memory:").expect("open :memory:");
        let skill = save_skill(
            &db,
            SkillInput {
                name: "orphan-check".to_string(),
                description: "d".to_string(),
                body: "b".to_string(),
                source: None,
            },
            Utc::now(),
        )
        .expect("save_skill query")
        .expect("save_skill must upsert a fresh name");
        assert!(
            set_bot_skill(&db, "bot-a", &skill.name, true).expect("set_bot_skill query"),
            "set_bot_skill must succeed for a skill that exists"
        );

        let before: i64 = db
            .conn()
            .query_row(
                "SELECT COUNT(*) FROM bot_skills WHERE skill_id = ?1",
                params![&skill.id],
                |row| row.get(0),
            )
            .expect("count bot_skills before delete");
        assert_eq!(before, 1, "sanity: the bot_skills row must exist first");

        assert!(
            delete_skill(&db, &skill.name).expect("delete_skill query"),
            "delete_skill must report a row was removed"
        );

        let after: i64 = db
            .conn()
            .query_row(
                "SELECT COUNT(*) FROM bot_skills WHERE skill_id = ?1",
                params![&skill.id],
                |row| row.get(0),
            )
            .expect("count bot_skills after delete");
        assert_eq!(
            after, 0,
            "no orphan bot_skills row may remain after delete_skill"
        );
    }
}
