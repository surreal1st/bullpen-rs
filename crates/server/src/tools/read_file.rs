//! `sandbox_read`: read a file from a bot's own `/work` sandbox volume -
//! the same container `shell` runs in. Lives in a file named `read_file.rs`
//! (S6L-02's own naming) but the TOOL is deliberately NOT called
//! `read_file` - see below.
//!
//! S6-lite invention, not a TS port. TS's real `read_file` (`app.ts:5664-
//! 5680`) reads a file on JOSH'S OWN COMPUTER through a client-fulfilled
//! approval (`app.ts:6520-6531`'s `isClientFulfilled` - the desktop app
//! computes the result, not the server) - out of S6-lite's scope, and
//! already promised to every bot verbatim in `prompt.rs`'s
//! `WHERE_YOU_ARE`: "His WORKSTATION... Use the `read_file` tool. It works
//! through the Bullpen desktop app... and it asks him to approve each
//! read." Building a DIFFERENT tool under that same name would make that
//! prompt text a lie the moment a bot called it - it would silently read
//! the bot's own `/work` instead of Josh's machine, with no desktop app
//! and no per-read approval. `sandbox_read` answers a narrower, purely
//! server-side question instead: a file already sitting in `/work`, which
//! `sandbox.rs`'s `validate_read_path` confines before anything shells
//! out. See `permissions.rs`'s `default_decisions` doc for the same split.
use model::ToolSpec;
use serde::Deserialize;
use serde_json::json;

use crate::sandbox::Sandbox;

use super::fence_tool_output;

pub fn spec() -> ToolSpec {
    ToolSpec {
        name: "sandbox_read".to_string(),
        description: "Read a file from your own /work directory - the same sandbox `shell` \
runs in. The path is relative to /work and cannot leave it. This is NOT Josh's computer - use \
`read_file` for that, if you have it."
            .to_string(),
        parameters: json!({
            "type": "object",
            "properties": {
                "path": {
                    "type": "string",
                    "description": "A path under /work, e.g. \"notes.txt\" or \"logs/run.txt\"."
                }
            },
            "required": ["path"]
        }),
    }
}

#[derive(Deserialize, Default)]
struct Args {
    #[serde(default)]
    path: String,
}

/// Reads `path` from `sandbox`'s `/work` and fences the content as
/// untrusted data (same reasoning as `shell::run`) - a file the bot's own
/// commands wrote is exactly as capable of carrying an injected
/// instruction as a command's stdout is. An error (no sandbox, or a path
/// `validate_read_path` refuses) is returned as-is: that text is this
/// tool's own refusal, not data a file produced.
pub async fn run(sandbox: &dyn Sandbox, bot_id: &str, args: &str) -> String {
    let parsed: Args = serde_json::from_str(args).unwrap_or_default();
    let path = parsed.path.trim();
    if path.is_empty() {
        return "No path was given.".to_string();
    }

    match sandbox.read_file(bot_id, path).await {
        Ok(content) => fence_tool_output(&content),
        Err(detail) => detail,
    }
}
