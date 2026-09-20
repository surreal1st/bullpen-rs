//! Store-layer tests for bot lifecycle helpers (`duplicate_bot`, etc.).

use chrono::Utc;
use store::{
    BotDraft, Db, HardDeleteBotOutcome, create_bot, duplicate_bot, get_bot, hard_delete_bot,
    set_archived,
    skills::{SkillInput, save_skill, set_bot_skill, skills_for},
};

fn enabled_skill_names(db: &Db, bot_id: &str) -> Vec<String> {
    skills_for(db, bot_id)
        .expect("skills_for")
        .into_iter()
        .map(|skill| skill.name)
        .collect()
}

/// S10-05: duplicate must carry the source's opt-in skill toggles, not every
/// skill in the library.
#[test]
fn duplicate_bot_copies_enabled_bot_skills_only() {
    let db = Db::open(":memory:").expect("open :memory:");
    let source = create_bot(
        &db,
        BotDraft {
            name: "Source Bot".to_string(),
            purpose: "p".to_string(),
            instructions: "i".to_string(),
            model: None,
        },
    )
    .expect("create_bot");

    let now = Utc::now();
    let enabled = save_skill(
        &db,
        SkillInput {
            name: "enabled-on-source".to_string(),
            description: "d".to_string(),
            body: "b".to_string(),
            source: None,
        },
        now,
    )
    .expect("save_skill query")
    .expect("save enabled skill");
    save_skill(
        &db,
        SkillInput {
            name: "library-only".to_string(),
            description: "d".to_string(),
            body: "b".to_string(),
            source: None,
        },
        now,
    )
    .expect("save_skill query")
    .expect("save library skill");

    assert!(
        set_bot_skill(&db, &source.id, &enabled.name, true).expect("set_bot_skill query"),
        "set_bot_skill must succeed for a skill that exists"
    );

    let copy = duplicate_bot(&db, &source.id)
        .expect("duplicate_bot query")
        .expect("source bot must exist");

    assert_eq!(
        enabled_skill_names(&db, &copy.id),
        vec!["enabled-on-source"],
        "copy must inherit only the source's enabled skills"
    );
    assert_eq!(
        enabled_skill_names(&db, &source.id),
        vec!["enabled-on-source"],
        "source toggles must be unchanged after duplicate"
    );
}

/// SEC5-09: hard delete removes the bot row and its memory; live bots are refused.
#[test]
fn hard_delete_bot_requires_archive_and_removes_data() {
    let db = Db::open(":memory:").expect("open :memory:");
    let bot = create_bot(
        &db,
        BotDraft {
            name: "Gone Bot".to_string(),
            purpose: "p".to_string(),
            instructions: "i".to_string(),
            model: None,
        },
    )
    .expect("create_bot");

    assert_eq!(
        hard_delete_bot(&db, &bot.id).expect("hard_delete_bot"),
        HardDeleteBotOutcome::NotArchived,
        "a live bot must not be hard-deleted"
    );
    assert!(get_bot(&db, &bot.id).expect("get_bot").is_some());

    set_archived(&db, &bot.id, true).expect("archive");
    store::remember(&db, &bot.id, "secret memory", "josh").expect("remember");

    assert_eq!(
        hard_delete_bot(&db, &bot.id).expect("hard_delete_bot"),
        HardDeleteBotOutcome::Deleted
    );
    assert!(get_bot(&db, &bot.id).expect("get_bot").is_none());

    let memory_left: i64 = db
        .conn()
        .query_row(
            "SELECT COUNT(*) FROM memory_log WHERE bot_id = ?1",
            rusqlite::params![bot.id],
            |row| row.get(0),
        )
        .expect("count memory");
    assert_eq!(memory_left, 0, "memory_log rows must be removed");
}
