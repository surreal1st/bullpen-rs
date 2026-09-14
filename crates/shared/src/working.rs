//! What a bot is doing RIGHT NOW, in one line a person can read at a glance.
//! Port of `src/shared/working.ts`.
//!
//! The server produces this line, not the client - a chained room round has
//! no client stream at all for member two onward, so a client that assembled
//! this itself would show the indicator for only the one bot it happened to
//! start. The phrasing is deliberately about the WORK, not the tool name.

use serde::{Deserialize, Serialize};

/// One bot's line for the working indicator. Port of the TS `WorkingBot`.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct WorkingBot {
    pub bot_id: String,
    pub name: String,
    /// `None` for an emoji-less bot, exactly as `RosterEntry.avatar` carries it.
    pub avatar: Option<String>,
    pub section_id: Option<String>,
    pub shape: Option<String>,
    /// The one line. Never empty - a run with nothing else to say is "Thinking".
    pub activity: String,
    /// Parked on an approval rather than running, which reads differently.
    pub waiting: bool,
}

/// Tools worth naming properly, because Josh will actually see these.
/// Everything absent falls through to [`generic`], which turns
/// `read_resource` into "Running read resource" - plain, correct, and never
/// a lie about what is happening. Verbatim from the TS `PHRASES`, plus
/// `create_room`/`add_to_room` - the two tools this Rust port adds that Grok
/// Bot never had.
fn phrase(tool: &str) -> Option<&'static str> {
    Some(match tool {
        "web_search" => "Searching the web",
        "fetch_url" => "Reading a web page",
        "browse" => "Opening a page on the desk",
        "read_page" => "Reading the page on the desk",
        "click" => "Clicking something on the desk",
        "type_text" => "Typing on the desk",
        "desk_act" => "Using the desk",
        "desk_shell" => "Running a command on the desk",
        "snap_desk" => "Looking at the desk",
        "record_desk" => "Recording the desk",
        "shell" => "Running a command",
        "run_in_background" => "Starting background work",
        "await_job" => "Waiting on its own job",
        "job_status" => "Checking its own job",
        "query_db" => "Querying a database",
        "search_memory" => "Searching its memory",
        "remember" => "Writing to its memory",
        "vault_search" => "Searching Josh's vault",
        "vault_read" => "Reading Josh's vault",
        "vault_write" => "Writing to Josh's vault",
        "search_history" => "Searching past sessions",
        "search_conversations" => "Searching its own threads",
        "message_bot" => "Asking a colleague",
        "ask_in_background" => "Asking a colleague in the background",
        "spawn_helper" => "Sending a helper on an errand",
        "ask_josh" => "Waiting on Josh",
        "say" => "Saying something",
        "draw_image" => "Drawing a picture",
        "deliver" => "Building a deliverable",
        "zenith_html" => "Building the Broadcast page",
        "read_file" => "Reading a file on Josh's computer",
        "repo_read" => "Reading its checkout",
        "repo_grep" => "Searching its checkout",
        "repo_run" => "Running something in its checkout",
        "repo_edit" => "Editing its checkout",
        "repo_pr" => "Opening a pull request",
        "escalate" => "Reaching for a better model",
        "use_skill" => "Reading a skill",
        "add_task" => "Updating its checklist",
        "update_task" => "Updating its checklist",
        "list_tasks" => "Reading its checklist",
        "set_goal" => "Updating a goal",
        "update_goal" => "Updating a goal",
        "reflect" => "Thinking it over",
        "watch_video" => "Watching a video",
        "review_media" => "Reviewing an attachment",
        "replay_demo" => "Replaying something Josh showed it",
        "purchase" => "Buying something",
        "hire_bot" => "Hiring someone",
        "propose_tool" => "Writing a new tool",
        "adopt_thread" => "Taking over a thread",
        "create_room" => "Starting a group chat",
        "add_to_room" => "Adding someone to a group chat",
        _ => return None,
    })
}

/// `read_resource` -> "Running read resource". Never a bare snake_case name.
fn generic(tool: &str) -> String {
    format!("Running {}", tool.replace('_', " "))
}

/// The line, from what the run is actually doing.
///
/// Order matters and is the whole of the logic: an approval is the most
/// important thing to say, because it is the one state that will sit there
/// forever until Josh acts. A tool in flight beats text, because a bot that
/// is mid-`web_search` has usually already written a sentence about it. Text
/// with no tool is a reply being written. Nothing at all is "Thinking",
/// which is the honest answer for the seconds between the run starting and
/// the first token. Port of the TS `activityLine`.
pub fn activity_line(waiting_on: Option<&str>, tool: Option<&str>, wrote_text: bool) -> String {
    if let Some(waiting_on) = waiting_on {
        return format!(
            "Waiting for you to approve {}",
            waiting_on.replace('_', " ")
        );
    }
    if let Some(tool) = tool {
        return phrase(tool)
            .map(str::to_string)
            .unwrap_or_else(|| generic(tool));
    }
    if wrote_text {
        "Writing a reply".to_string()
    } else {
        "Thinking".to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn prefers_an_approval_over_anything_else_the_run_was_doing() {
        assert_eq!(
            activity_line(Some("desk_shell"), Some("web_search"), true),
            "Waiting for you to approve desk shell"
        );
    }

    #[test]
    fn names_a_tool_josh_will_recognise_and_falls_back_to_plain_words_for_one_it_does_not() {
        assert_eq!(
            activity_line(None, Some("type_text"), false),
            "Typing on the desk"
        );
        assert_eq!(
            activity_line(None, Some("read_resource"), false),
            "Running read resource"
        );
    }

    #[test]
    fn is_never_a_bare_snake_case_tool_name() {
        for tool in ["web_search", "read_resource", "some_future_tool"] {
            assert!(!activity_line(None, Some(tool), false).contains('_'));
        }
    }

    #[test]
    fn says_thinking_before_the_first_token_and_writing_a_reply_after_it() {
        assert_eq!(activity_line(None, None, false), "Thinking");
        assert_eq!(activity_line(None, None, true), "Writing a reply");
    }
}
