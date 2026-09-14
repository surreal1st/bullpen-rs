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
//! instant this call returns. No `low_budget` top-rung closing either:
//! that reads the live OpenRouter balance, which nothing wires into a tool
//! call yet (S2-05 owns the spend ceiling) - so the top rung is never
//! closed for budget from here, only `may_escalate` gates this tool at all.

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

/// One rung up, never two. Port of `escalation.ts`'s `climb` (280-333),
/// minus `low_budget` (see this module's doc).
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
    // tier == 2: the only rung left is the top one.
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
