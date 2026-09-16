//! `read_file`: reads a file on JOSH'S OWN COMPUTER, not this server -
//! `crates/server/src/tools/read_file.rs` is a DIFFERENT tool
//! (`sandbox_read`) over the bot's own container volume; see that file's
//! module doc for why it could not take this name. Port of TS's
//! `read_file` spec (`app.ts:5662-5676`) and its `NOT_ON_THIS_MACHINE`
//! fallback (`clientTools.ts:29-30`, executed at `app.ts:6529-6531`).
//!
//! §3 of the design: approving IS performing the read. The server never has
//! the file - the desktop client reads it locally and posts the contents
//! back as the approval's `result`, which `runs.rs::decide_approval`
//! substitutes for the tool's own output once `"read_file"` is in
//! `CLIENT_FULFILLED`. This module's `run` exists only for the paths where
//! that substitution never happens: the approval expired unfulfilled, a
//! browser (which cannot read Josh's disk) approved it and posted no
//! `result`, or - the bug this tool exists to prevent - something reaches
//! the server's own dispatch for this name at all. In every one of those
//! cases the honest answer is `NOT_ON_THIS_MACHINE`, never an empty string
//! (a model reads `""` as "the file was blank", design §4.7) and never an
//! attempt to open the path itself - this crate runs on meridian, and the
//! path is meaningless there.
use model::ToolSpec;
use serde_json::json;

/// Verbatim TS text (`clientTools.ts:29-30`) - explains itself to the model
/// so a bot that reads it can tell Josh what happened instead of retrying
/// forever.
pub const NOT_ON_THIS_MACHINE: &str = "That file is on Josh's own computer, not on the server \
you run on. He has to approve the request in the Bullpen desktop app, which reads it and sends \
the contents back. If he is in a browser, ask him to open the desktop app.";

pub fn spec() -> ToolSpec {
    ToolSpec {
        name: "read_file".to_string(),
        description: "Read a file on Josh's own computer, by its full path, such as \
C:\\Users\\rain\\notes.md. This is HIS machine, not the server you run on, so he has to approve \
each one and it only works while he has the Bullpen desktop app open. Ask for a specific file \
he has mentioned - you cannot list folders or go looking."
            .to_string(),
        parameters: json!({
            "type": "object",
            "properties": {
                "path": {
                    "type": "string",
                    "description": "The full path to one file, exactly as Josh wrote it."
                }
            },
            "required": ["path"]
        }),
    }
}

/// Always `NOT_ON_THIS_MACHINE`, regardless of `args` - this crate has no
/// business reading `args.path`, because doing so at all would mean the
/// server just performed the read it exists to refuse. See design §7 bite
/// 1: a real file present at the given path must not change this answer.
pub fn run() -> String {
    NOT_ON_THIS_MACHINE.to_string()
}
