//! A bot asking Josh a question, rather than guessing. Port of
//! `src/shared/askJosh.ts` from the TS original.

use serde::Deserialize;

/// The most options a question may carry.
pub const MAX_OPTIONS: usize = 4;

/// The most characters of a question or an option that survive.
pub const MAX_QUESTION_CHARS: usize = 500;
pub const MAX_OPTION_CHARS: usize = 120;

#[derive(Debug, Clone, Deserialize)]
pub struct AskJoshArgs {
    pub question: String,
    #[serde(default)]
    pub options: Vec<String>,
    #[serde(default)]
    pub wait: bool,
}

/// Tools whose result is supplied by Josh ANSWERING, not by the server running
/// anything.
pub const ANSWERABLE: &[&str] = &["ask_josh"];

pub fn is_answerable(tool_name: &str) -> bool {
    ANSWERABLE.contains(&tool_name)
}

/// Reads what the model sent. Total, never throwing. A malformed question
/// is still a question, and the failure mode that matters is a run dying
/// on its way to asking for help. Options are capped rather than refused:
/// a model that offers nine is being unhelpful, not dangerous, and four
/// buttons is what a phone can show.
pub fn parse_ask_josh(input: &str) -> AskJoshArgs {
    let raw = match serde_json::from_str::<serde_json::Value>(input) {
        Ok(serde_json::Value::Object(obj)) => obj,
        _ => {
            return AskJoshArgs {
                question: String::new(),
                options: Vec::new(),
                wait: false,
            };
        }
    };

    let question = raw
        .get("question")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .trim()
        .to_string();

    let options = raw
        .get("options")
        .and_then(|v| v.as_array())
        .map(|arr| {
            arr.iter()
                .filter_map(|o| o.as_str())
                .map(|s| s.trim())
                .filter(|s| !s.is_empty())
                .map(|s| s.chars().take(MAX_OPTION_CHARS).collect::<String>())
                .take(MAX_OPTIONS)
                .collect()
        })
        .unwrap_or_default();

    let wait = raw.get("wait").and_then(|v| v.as_bool()).unwrap_or(false);

    AskJoshArgs {
        question: question.chars().take(MAX_QUESTION_CHARS).collect(),
        options,
        wait,
    }
}
