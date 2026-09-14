//! Questions table. Self-creating, like `skills` and `push_devices`.
//! Port of `src/server/conversing.ts`'s questions table.

use crate::Db;
use crate::conversations::now_iso;
use rusqlite::params;

/// One open question from the database.
#[derive(Debug, Clone)]
pub struct OpenQuestion {
    pub id: String,
    pub bot_id: String,
    pub conversation_id: String,
    pub question: String,
    pub options: Vec<String>,
    pub asked_at: String,
}

struct QuestionRow {
    id: String,
    bot_id: String,
    conversation_id: String,
    #[allow(dead_code)]
    message_id: Option<String>,
    question: String,
    options: String,
    #[allow(dead_code)]
    answer: Option<String>,
    asked_at: String,
    #[allow(dead_code)]
    answered_at: Option<String>,
}

/// Ensure the questions table exists. Called from `Db::open`.
pub fn ensure_table(db: &Db) -> rusqlite::Result<()> {
    db.conn().execute_batch(
        "CREATE TABLE IF NOT EXISTS questions (
            id              TEXT PRIMARY KEY,
            bot_id          TEXT NOT NULL,
            conversation_id TEXT NOT NULL,
            message_id      TEXT,
            question        TEXT NOT NULL,
            options         TEXT NOT NULL DEFAULT '[]',
            answer          TEXT,
            asked_at        TEXT NOT NULL,
            answered_at     TEXT
        );
        CREATE INDEX IF NOT EXISTS idx_questions_open
            ON questions(bot_id, answered_at, asked_at DESC);",
    )?;
    Ok(())
}

/// Parse options from JSON string, recovering gracefully on malformed input.
fn parse_options(options_json: &str) -> Vec<String> {
    match serde_json::from_str::<serde_json::Value>(options_json) {
        Ok(serde_json::Value::Array(arr)) => arr
            .iter()
            .filter_map(|v| v.as_str().map(|s| s.to_string()))
            .collect(),
        _ => Vec::new(),
    }
}

fn row_to_open_question(row: QuestionRow) -> OpenQuestion {
    OpenQuestion {
        id: row.id,
        bot_id: row.bot_id,
        conversation_id: row.conversation_id,
        question: row.question,
        options: parse_options(&row.options),
        asked_at: row.asked_at,
    }
}

/// Insert a new question. Returns the id.
pub fn insert_question(
    db: &Db,
    bot_id: &str,
    conversation_id: &str,
    message_id: Option<&str>,
    question: &str,
    options: &[String],
) -> rusqlite::Result<String> {
    let id = uuid::Uuid::new_v4().to_string();
    let options_json = serde_json::to_string(options).unwrap_or_else(|_| "[]".to_string());

    db.conn().execute(
        "INSERT INTO questions (id, bot_id, conversation_id, message_id, question, options, asked_at)
         VALUES (?, ?, ?, ?, ?, ?, ?)",
        params![
            &id,
            bot_id,
            conversation_id,
            message_id,
            question,
            options_json,
            now_iso()
        ],
    )?;

    Ok(id)
}

/// Get all open questions for a specific bot.
pub fn list_open(db: &Db, bot_id: &str) -> rusqlite::Result<Vec<OpenQuestion>> {
    let mut stmt = db.conn().prepare(
        "SELECT id, bot_id, conversation_id, message_id, question, options, answer, asked_at, answered_at
           FROM questions
          WHERE bot_id = ?1 AND answered_at IS NULL
          ORDER BY asked_at DESC",
    )?;

    let rows = stmt.query_map(params![bot_id], |row| {
        Ok(QuestionRow {
            id: row.get(0)?,
            bot_id: row.get(1)?,
            conversation_id: row.get(2)?,
            message_id: row.get(3)?,
            question: row.get(4)?,
            options: row.get(5)?,
            answer: row.get(6)?,
            asked_at: row.get(7)?,
            answered_at: row.get(8)?,
        })
    })?;

    rows.map(|r| r.map(row_to_open_question))
        .collect::<Result<Vec<_>, _>>()
}

/// Get all open questions across all bots.
pub fn list_all_open(db: &Db) -> rusqlite::Result<Vec<OpenQuestion>> {
    let mut stmt = db.conn().prepare(
        "SELECT id, bot_id, conversation_id, message_id, question, options, answer, asked_at, answered_at
           FROM questions
          WHERE answered_at IS NULL
          ORDER BY asked_at DESC",
    )?;

    let rows = stmt.query_map([], |row| {
        Ok(QuestionRow {
            id: row.get(0)?,
            bot_id: row.get(1)?,
            conversation_id: row.get(2)?,
            message_id: row.get(3)?,
            question: row.get(4)?,
            options: row.get(5)?,
            answer: row.get(6)?,
            asked_at: row.get(7)?,
            answered_at: row.get(8)?,
        })
    })?;

    rows.map(|r| r.map(row_to_open_question))
        .collect::<Result<Vec<_>, _>>()
}

/// Record an answer to a question.
pub fn answer_question(db: &Db, id: &str, answer: &str) -> rusqlite::Result<()> {
    db.conn().execute(
        "UPDATE questions SET answer = ?, answered_at = ? WHERE id = ?",
        params![answer, now_iso(), id],
    )?;
    Ok(())
}
