use model::CHEAP_DEFAULT_MODEL;
use model::ladder::{
    EscalationKind, Trigger, looks_premium, may_escalate, mid_model, model_for_run, premium_model,
    safe_fallback, set_default_model, set_mid_model, set_premium_model, tier_of, tier1_model,
};
use store::Db;

fn open_db() -> Db {
    Db::open(":memory:").expect("failed to open in-memory db")
}

#[test]
fn model_for_run_leaves_a_conversation_you_started_alone() {
    let db = open_db();
    let result = model_for_run(&db, Trigger::Chat, "anthropic/claude-opus-5", false);
    assert_eq!(result, "anthropic/claude-opus-5");
}

#[test]
fn model_for_run_forces_anything_on_a_timer_down_to_the_cheap_model() {
    let db = open_db();
    for model in &[
        "anthropic/claude-fable-5.1",
        "openai/sol-2",
        "google/astra-ultra",
        "anthropic/claude-opus-5",
    ] {
        let result = model_for_run(&db, Trigger::Routine, model, false);
        assert_eq!(result, CHEAP_DEFAULT_MODEL, "failed for {}", model);
    }
}

#[test]
fn model_for_run_also_catches_whatever_is_configured_as_the_escalation_target() {
    let db = open_db();
    set_premium_model(&db, "vendor/quiet-name");
    assert!(!looks_premium("vendor/quiet-name"));
    let result = model_for_run(&db, Trigger::Routine, "vendor/quiet-name", false);
    assert_eq!(result, CHEAP_DEFAULT_MODEL);
}

#[test]
fn model_for_run_leaves_a_cheap_model_on_a_timer_alone() {
    let db = open_db();
    let result = model_for_run(&db, Trigger::Routine, "openai/gpt-oss-20b", false);
    assert_eq!(result, "openai/gpt-oss-20b");
}

#[test]
fn may_escalate_only_lets_a_conversation_escalate() {
    assert!(may_escalate(Trigger::Chat));
    assert!(!may_escalate(Trigger::Routine));
    assert!(!may_escalate(Trigger::Webhook));
}

#[test]
fn tier_of_unknown_model_is_zero() {
    let db = open_db();
    assert_eq!(tier_of(&db, "unknown/model-1"), 0);
}

#[test]
fn tier_of_tier1_reason_model_is_one() {
    let db = open_db();
    let model = tier1_model(&db, EscalationKind::Reason);
    assert_eq!(tier_of(&db, &model), 1);
}

#[test]
fn tier_of_mid_model_is_two() {
    let db = open_db();
    let model = mid_model(&db);
    assert_eq!(tier_of(&db, &model), 2);
}

#[test]
fn tier_of_premium_model_is_three() {
    let db = open_db();
    let model = premium_model(&db);
    assert_eq!(tier_of(&db, &model), 3);
}

#[test]
fn tier_of_configured_mid_model_whose_name_looks_premium_is_still_two() {
    let db = open_db();
    set_mid_model(&db, "vendor/fable-name");
    // The name looks premium but the configuration says tier 2
    assert_eq!(tier_of(&db, "vendor/fable-name"), 2);
}

#[test]
fn safe_fallback_never_downgrades_to_a_premium_model() {
    let db = open_db();
    set_default_model(&db, "anthropic/claude-fable-5.1");
    let result = safe_fallback(&db);
    assert_eq!(result, CHEAP_DEFAULT_MODEL);
    assert!(!looks_premium(&result));
}

#[test]
fn safe_fallback_honors_an_ordinary_configured_default() {
    let db = open_db();
    set_default_model(&db, "openai/gpt-oss-20b");
    let result = safe_fallback(&db);
    assert_eq!(result, "openai/gpt-oss-20b");
}

#[test]
fn model_for_run_chat_with_sonnet_and_room_true_returns_cheap_default() {
    let db = open_db();
    let result = model_for_run(&db, Trigger::Chat, "anthropic/claude-sonnet-5", true);
    assert_eq!(result, CHEAP_DEFAULT_MODEL);
}

#[test]
fn model_for_run_chat_with_sonnet_and_room_false_returns_sonnet() {
    let db = open_db();
    let result = model_for_run(&db, Trigger::Chat, "anthropic/claude-sonnet-5", false);
    assert_eq!(result, "anthropic/claude-sonnet-5");
}

#[test]
fn model_for_run_routine_with_gemini_flash_lite_returns_unchanged() {
    let db = open_db();
    let result = model_for_run(&db, Trigger::Routine, "google/gemini-2.5-flash-lite", false);
    assert_eq!(result, "google/gemini-2.5-flash-lite");
}
