//! S1-04 acceptance: block order, the history budget, recall, room
//! instructions, and the house-rules cap. `Db::open(":memory:")` + a bot,
//! no HTTP - `build_prompt` and friends are exercised directly.

use model::MessageContent;
use server::prompt::{
    HistoryTurn, build_prompt, house_rules, room_instruction, set_house_rules,
    with_room_instruction,
};
use shared::nothing_new::NOTHING_NEW;
use store::Db;

fn seed_bot(db: &Db, id: &str, name: &str, purpose: &str, instructions: &str) {
    db.conn()
        .execute(
            "INSERT INTO bots (id, name, purpose, instructions, model, created_at) VALUES (?1, ?2, ?3, ?4, NULL, '2026-01-01T00:00:00Z')",
            rusqlite::params![id, name, purpose, instructions],
        )
        .unwrap();
}

fn system_text(messages: &[model::ModelMessage]) -> String {
    match &messages[0].content {
        MessageContent::Text(t) => t.clone(),
        MessageContent::Parts(_) => panic!("system message must be plain text"),
    }
}

#[test]
fn block_order_is_where_you_are_rules_about_josh_instructions_known_remembered() {
    let db = Db::open(":memory:").unwrap();
    seed_bot(&db, "t", "T", "testing", "UNIQUE-INSTRUCTIONS-MARKER");
    db.settings_set("memory.shared_core", "ABOUT-JOSH-MARKER")
        .unwrap();
    store::set_core(&db, "t", "KNOWN-CORE-MARKER").unwrap();
    store::remember(&db, "t", "REMEMBERED-LOG-MARKER", "bot").unwrap();

    let bot = store::get_bot(&db, "t").unwrap().unwrap();
    let messages = build_prompt(&db, &bot, &[]);
    let system = system_text(&messages);

    let where_you_are = system.find("## Where you are").unwrap();
    let rules = system.find("## Rules for every bot").unwrap();
    let about_josh = system.find("ABOUT-JOSH-MARKER").unwrap();
    let instructions = system.find("UNIQUE-INSTRUCTIONS-MARKER").unwrap();
    let known = system.find("## What you already know").unwrap();
    let remembered = system.find("## What you know").unwrap();

    assert!(
        where_you_are < rules,
        "WHERE_YOU_ARE must precede house rules"
    );
    assert!(rules < about_josh, "house rules must precede ## About Josh");
    assert!(
        about_josh < instructions,
        "## About Josh must precede the bot's instructions"
    );
    assert!(
        instructions < known,
        "instructions must precede ## What you already know"
    );
    assert!(
        known < remembered,
        "## What you already know must precede the recall tier's ## What you know"
    );
}

#[test]
fn history_budget_keeps_the_newest_and_drops_the_oldest() {
    let db = Db::open(":memory:").unwrap();
    seed_bot(&db, "t", "T", "", "You are T.");
    let bot = store::get_bot(&db, "t").unwrap().unwrap();

    // ~200 tokens (~800 chars) each; 90 of them is far past the 6000-token
    // budget. Alternate role, ending on a user turn so no trailer is added.
    let filler = "x".repeat(780);
    let mut history = Vec::new();
    for i in 0..90 {
        let content = format!("msg-{i:03} {filler}");
        history.push(if i % 2 == 0 {
            HistoryTurn::user(content)
        } else {
            HistoryTurn::assistant(content)
        });
    }
    // Force the last turn to be a user turn so build_prompt does not append
    // the NO_NEW_MESSAGE_TRAILER, which would otherwise be the "last message".
    if history.last().unwrap().role != "user" {
        history.push(HistoryTurn::user(format!("msg-090 {filler}")));
    }

    let messages = build_prompt(&db, &bot, &history);

    // The oldest message is gone.
    assert!(
        messages.iter().all(|m| match &m.content {
            MessageContent::Text(t) => !t.contains("msg-000 "),
            MessageContent::Parts(_) => true,
        }),
        "the oldest history message must have been dropped by the budget"
    );

    // The last message in the prompt is the newest history message.
    let last = messages.last().unwrap();
    let last_text = match &last.content {
        MessageContent::Text(t) => t.clone(),
        MessageContent::Parts(_) => panic!("expected text"),
    };
    assert!(
        last_text.starts_with("msg-090") || last_text.starts_with("msg-089"),
        "expected the newest history message last, got: {}",
        &last_text[..20.min(last_text.len())]
    );
}

#[test]
fn recall_holds_the_newest_within_budget_and_names_the_older_count() {
    let db = Db::open(":memory:").unwrap();
    seed_bot(&db, "t", "T", "", "You are T.");
    let bot = store::get_bot(&db, "t").unwrap().unwrap();

    // ~200 tokens each; 30 entries is far past the 1200-token recall budget.
    let filler = "x".repeat(780);
    for i in 0..30 {
        store::remember(&db, "t", &format!("entry-{i:03} {filler}"), "bot").unwrap();
    }

    let messages = build_prompt(&db, &bot, &[]);
    let system = system_text(&messages);

    assert!(system.contains("## What you know"));
    let kept = system.lines().filter(|l| l.starts_with("- entry-")).count();
    assert!(kept > 0, "the newest entries must be kept");
    assert!(
        kept < 30,
        "30 entries at ~200 tokens must not all fit in a 1200-token budget"
    );

    // The newest entry (entry-029) must be present, the oldest (entry-000) must be absent.
    assert!(
        system.contains("entry-029"),
        "the newest entry (entry-029) must be kept"
    );
    assert!(
        !system.contains("entry-000"),
        "the oldest entry (entry-000) must be dropped"
    );

    let older_line = system
        .lines()
        .find(|l| l.starts_with("There are ") && l.contains("older notes"))
        .expect("an 'older notes' line naming the count");
    let older_count: usize = older_line
        .split_whitespace()
        .nth(2)
        .and_then(|n| n.parse().ok())
        .expect("the older-notes line must name a number");
    assert_eq!(kept + older_count, 30);
}

#[test]
fn with_room_instruction_appends_one_trailing_user_turn() {
    let db = Db::open(":memory:").unwrap();
    seed_bot(&db, "t", "T", "", "You are T.");
    seed_bot(&db, "u", "U", "", "You are U.");
    seed_bot(&db, "v", "V", "", "You are V.");
    let all_ids = vec!["t".to_string(), "u".to_string(), "v".to_string()];

    let base = vec![model::ModelMessage::user("hello room")];

    let note = room_instruction(&db, &all_ids, "t", false);
    let with_note = with_room_instruction(base.clone(), &note);
    assert_eq!(with_note.len(), base.len() + 1);
    let last = with_note.last().unwrap();
    assert_eq!(last.role, "user");
    let text = match &last.content {
        MessageContent::Text(t) => t.clone(),
        MessageContent::Parts(_) => panic!("expected text"),
    };
    assert!(text.contains("1 to 3 sentences"));
    assert!(text.contains(NOTHING_NEW));

    let mandatory_note = room_instruction(&db, &all_ids, "t", true);
    let with_mandatory = with_room_instruction(base, &mandatory_note);
    let mandatory_text = match &with_mandatory.last().unwrap().content {
        MessageContent::Text(t) => t.clone(),
        MessageContent::Parts(_) => panic!("expected text"),
    };
    assert!(!mandatory_text.contains(NOTHING_NEW));
    assert!(mandatory_text.contains("@everyone"));
}

#[test]
fn set_house_rules_caps_at_4000_chars() {
    let db = Db::open(":memory:").unwrap();
    let stored = set_house_rules(&db, &"a".repeat(5000));
    assert_eq!(stored.len(), 4000);
    assert_eq!(house_rules(&db).len(), 4000);
}

#[test]
fn history_budget_uses_utf16_code_units() {
    // F22: verify recent_history counts UTF-16 code units like TS, not UTF-8 bytes.
    // Test with messages where UTF-16 code unit count differs from byte count
    // to prove the fix is working correctly.

    // Budget: 6000 tokens * 4 = 24000 UTF-16 code units
    // Emoji 😀 outside BMP: 1 scalar, 2 UTF-16 units, 4 UTF-8 bytes
    // Test with 50 emoji per message = 100 UTF-16 units or 200 UTF-8 bytes
    // Correct UTF-16: 240 messages fit. Wrong UTF-8: 120 messages fit.
    let emoji_msg = "😀".repeat(50);
    let messages: Vec<HistoryTurn> = (0..180)
        .map(|i| HistoryTurn::user(format!("msg-{:03} {}", i, emoji_msg)))
        .collect();

    let kept = server::prompt::recent_history(&messages);

    // UTF-16 counting: 180 messages fit. UTF-8 counting: ~120 messages fit.
    assert!(
        kept.len() >= 150,
        "with UTF-16 code unit counting, should keep most of 180 emoji messages (kept {})",
        kept.len()
    );
}
