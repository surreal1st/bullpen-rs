//! `escalate`: a bot admitting it is stuck asks for a stronger model. Chat
//! only - port of `app.ts:6234-6243`'s tool handler and `escalation.ts`'s
//! `climb` (230-333).
//!
//! 🔴 S2-04 narrowing, named here rather than silently: TS hands the SAME
//! conversation to a SECOND, separate run on the climbed model
//! (`onSelfEscalate`, `runs.ts:1618-1623` / `app.ts:745-772`) once the
//! current one finishes answering. This keeps the SAME run going on the
//! climbed model for its remaining steps instead - `tools/mod.rs`'s
//! `ToolBox` tracks "what model is this run on right now" for exactly this
//! tool to read and write, and `runs.rs`'s tool loop applies a climb the
//! instant this call returns.
//!
//! S2-F-04: `low_budget` (TS's `inLastReserve`, `spend.ts:154-162`) now
//! closes the top rung - see `climb`'s tier-2 arm. TS reads the LIVE
//! OpenRouter balance through a `CreditsPort`; that port lives on
//! `AppState` (`crates/server/src/spend.rs`, wired in `lib.rs`) but reaches
//! this deep only through `tools::build`'s dispatch closure
//! (`crates/server/src/tools/mod.rs`), which is outside this ticket's
//! owned files. `spend_in_last_reserve` below reads the same ceiling
//! against LOCALLY-summed month-to-date spend instead
//! (`spend::spend_by_bot`, the same source `GET /api/spend`'s bot
//! breakdown already uses): the same question, answered from a `&Db`
//! this function already has, off only by anything spent outside
//! Bullpen on the same key. Tightening this to the live `CreditsPort`
//! belongs to whichever ticket next touches `tools/mod.rs`'s dispatch
//! closure.

use model::ToolSpec;
use model::ladder::{self, EscalationKind, Trigger};
use serde::Deserialize;
use serde_json::json;
use store::Db;

pub fn spec() -> ToolSpec {
    ToolSpec {
        name: "escalate".to_string(),
        description: "Hand this conversation to a better-suited model because you are stuck. \
Use it only after you have genuinely tried, and say what you could not work out."
            .to_string(),
        parameters: json!({
            "type": "object",
            "properties": {
                "reason": { "type": "string", "description": "What you could not work out." },
                "kind": {
                    "type": "string",
                    "enum": ["code", "reason", "vision"],
                    "description": "What kind of stuck this is. code: writing, running or debugging software. vision: reading an image or screenshot. reason: everything else, including analysis, planning and judgement."
                }
            },
            "required": ["reason", "kind"]
        }),
    }
}

#[derive(Deserialize, Default)]
struct Args {
    #[serde(default)]
    reason: String,
    #[serde(default)]
    kind: Option<String>,
}

fn parse_kind(raw: Option<&str>) -> EscalationKind {
    match raw {
        Some("code") => EscalationKind::Code,
        Some("vision") => EscalationKind::Vision,
        // An unrecognised kind becomes "reason" rather than a refusal - the
        // bot has already established it is stuck, and the general rung is
        // the right place to land when it cannot say why. Port of TS's
        // `isEscalationKind(input.kind) ? input.kind : "reason"`.
        _ => EscalationKind::Reason,
    }
}

/// What `escalate` decided: the model `run_turn` should use for the rest of
/// this run, and why - folded into the notice it emits.
#[derive(Debug, Clone)]
pub struct Climb {
    pub model: String,
    pub note: String,
}

/// Port of TS's `ESCALATION_REFUSAL` (`escalation.ts:335-336`) verbatim.
const ESCALATION_REFUSAL: &str = "Escalation is not available on a scheduled run, because nobody is here to decide whether it is worth the money. Say plainly what you could not work out and stop.";

/// Runs the tool: chat only (`ladder::may_escalate`), climbs one rung on
/// the ladder by `kind`, from `current_model`.
pub fn run(db: &Db, trigger: Trigger, current_model: &str, args: &str) -> (String, Option<Climb>) {
    if !ladder::may_escalate(trigger) {
        return (ESCALATION_REFUSAL.to_string(), None);
    }

    let parsed: Args = serde_json::from_str(args).unwrap_or_default();
    let kind = parse_kind(parsed.kind.as_deref());
    let reason = parsed.reason;

    match climb(db, current_model, kind) {
        Ok(step) => {
            let text = format!(
                "Escalating. Keep your answer short; a better-suited model is about to take the same conversation. You said: {reason}"
            );
            (text, Some(step))
        }
        Err(refusal) => (refusal, None),
    }
}

/// Whether the LOCAL month-to-date spend is within the last 15% of the
/// platform ceiling - closes the top rung only, same guard TS's
/// `inLastReserve` computes from the live OpenRouter balance (see this
/// module's doc for why this reads local spend instead). A read failure
/// counts as low budget, same as `inLastReserve`'s `catch => true`.
fn spend_in_last_reserve(db: &Db) -> bool {
    let ceiling = crate::spend::get_ceiling(db);
    if ceiling <= 0.0 {
        return true;
    }
    let month = crate::spend::current_month(chrono::Utc::now());
    let spent: f64 = match crate::spend::spend_by_bot(db, &month) {
        Ok(rows) => rows.iter().map(|r| r.cost_usd).sum(),
        Err(_) => return true,
    };
    ceiling - spent <= ceiling * 0.15
}

/// One rung up, never two. Port of `escalation.ts`'s `climb` (280-333).
fn climb(db: &Db, current: &str, kind: EscalationKind) -> Result<Climb, String> {
    let tier = ladder::tier_of(db, current);
    if tier == 3 {
        return Err(
            "This is already the strongest model configured. There is nothing above it."
                .to_string(),
        );
    }
    if tier == 0 {
        let configured = ladder::tier1_model(db, kind);
        let collapsed =
            configured == ladder::mid_model(db) || configured == ladder::premium_model(db);
        let model = if collapsed {
            match kind {
                EscalationKind::Code => ladder::DEFAULT_TIER1.code.to_string(),
                EscalationKind::Reason => ladder::DEFAULT_TIER1.reason.to_string(),
                EscalationKind::Vision => ladder::DEFAULT_TIER1.vision.to_string(),
            }
        } else {
            configured
        };
        return Ok(Climb {
            model,
            note: format!("stuck on {}", kind.as_str()),
        });
    }
    if tier == 1 {
        let configured = ladder::mid_model(db);
        let collapsed = configured == ladder::premium_model(db);
        let model = if collapsed {
            ladder::DEFAULT_MID_MODEL.to_string()
        } else {
            configured
        };
        return Ok(Climb {
            model,
            note: format!("{}, and the specialist could not either", kind.as_str()),
        });
    }
    // tier == 2: the only rung left is the top one. Only this rung closes
    // for budget - a ladder that stopped entirely at 85% spent would turn
    // a spending guard into a work stoppage, and every rung underneath
    // still costs a fraction of this one.
    if spend_in_last_reserve(db) {
        return Err(
            "The strongest model is closed because the spend ceiling is nearly reached. Say \
             plainly what you could not work out and stop."
                .to_string(),
        );
    }
    Ok(Climb {
        model: ladder::premium_model(db),
        note: format!("{}, and nothing below this could do it", kind.as_str()),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn open_db() -> Db {
        Db::open(":memory:").expect("open :memory: db")
    }

    #[test]
    fn parses_known_and_unknown_kinds() {
        assert_eq!(parse_kind(Some("code")), EscalationKind::Code);
        assert_eq!(parse_kind(Some("vision")), EscalationKind::Vision);
        assert_eq!(parse_kind(Some("reason")), EscalationKind::Reason);
        assert_eq!(parse_kind(Some("nonsense")), EscalationKind::Reason);
        assert_eq!(parse_kind(None), EscalationKind::Reason);
    }

    #[test]
    fn climbs_from_tier_zero_to_the_kinds_own_tier1_model() {
        let db = open_db();
        let step = climb(&db, "some/unconfigured-model", EscalationKind::Code)
            .expect("should climb from tier 0");
        assert_eq!(step.model, ladder::DEFAULT_TIER1.code);
        assert_eq!(step.note, "stuck on code");
    }

    #[test]
    fn climbs_from_tier1_to_mid_and_mid_to_premium() {
        let db = open_db();
        let reason_model = ladder::tier1_model(&db, EscalationKind::Reason);
        let step = climb(&db, &reason_model, EscalationKind::Reason).expect("should climb to mid");
        assert_eq!(step.model, ladder::DEFAULT_MID_MODEL);

        let mid_model = ladder::mid_model(&db);
        let step = climb(&db, &mid_model, EscalationKind::Reason).expect("should climb to premium");
        assert_eq!(step.model, ladder::premium_model(&db));
    }

    #[test]
    fn refuses_to_climb_past_the_top_rung() {
        let db = open_db();
        let premium = ladder::premium_model(&db);
        let err = climb(&db, &premium, EscalationKind::Reason).expect_err("already maxed");
        assert!(err.contains("already the strongest"));
    }

    /// Seeds one provider-reported assistant message this calendar month,
    /// so `spend::spend_by_bot`'s current-month sum sees it - raw SQL
    /// rather than `store::append_message`, which always stamps `now()`
    /// and cannot backdate into a different month for the "two months"
    /// case elsewhere in this ticket's `tests/spend.rs`.
    fn seed_this_months_spend(db: &Db, bot_id: &str, cost_usd: f64) {
        db.conn()
            .execute(
                "INSERT INTO bots (id, name, purpose, instructions, model, created_at) \
                 VALUES (?1, ?1, '', '', NULL, '2026-01-01T00:00:00Z')",
                rusqlite::params![bot_id],
            )
            .expect("seed bot");
        let conversation_id =
            store::get_or_create_conversation(db, bot_id).expect("get_or_create_conversation");
        store::append_message(
            db,
            &conversation_id,
            "assistant",
            "done",
            store::NewMessage {
                usage: Some(store::Usage {
                    cost_usd,
                    ..Default::default()
                }),
                ..Default::default()
            },
        )
        .expect("seed assistant message");
    }

    #[test]
    fn climb_closes_the_top_rung_when_spend_is_in_the_last_reserve() {
        let db = open_db();
        crate::spend::set_ceiling(&db, 100.0).expect("set ceiling");
        seed_this_months_spend(&db, "spender", 90.0);

        let mid_model = ladder::mid_model(&db);
        let err =
            climb(&db, &mid_model, EscalationKind::Reason).expect_err("top rung should be closed");
        assert!(err.contains("nearly reached"), "{err}");
    }

    #[test]
    fn climb_still_reaches_premium_when_spend_is_fine() {
        let db = open_db();
        crate::spend::set_ceiling(&db, 100.0).expect("set ceiling");
        seed_this_months_spend(&db, "spender", 10.0);

        let mid_model = ladder::mid_model(&db);
        let step = climb(&db, &mid_model, EscalationKind::Reason).expect("should still climb");
        assert_eq!(step.model, ladder::premium_model(&db));
    }

    #[test]
    fn run_refuses_on_a_non_chat_trigger() {
        let db = open_db();
        let (text, climb) = run(
            &db,
            Trigger::Routine,
            "cheap/model",
            r#"{"reason":"stuck","kind":"code"}"#,
        );
        assert_eq!(text, ESCALATION_REFUSAL);
        assert!(climb.is_none());
    }

    #[test]
    fn run_climbs_on_a_chat_trigger() {
        let db = open_db();
        let (text, climb) = run(
            &db,
            Trigger::Chat,
            "cheap/model",
            r#"{"reason":"stuck","kind":"code"}"#,
        );
        assert!(text.starts_with("Escalating."));
        let step = climb.expect("should have climbed");
        assert_eq!(step.model, ladder::DEFAULT_TIER1.code);
    }
}
