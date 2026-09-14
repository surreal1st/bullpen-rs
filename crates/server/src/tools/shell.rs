//! `shell`: one command in your own sandbox, one result out. Spec ported
//! verbatim from `app.ts:5085-5095`.
//!
//! S2 ships no sandbox daemon - `BULLPEN_SANDBOX` never comes up here - so
//! this always answers with the TS `createUnavailableSandbox` text
//! (`index.ts:58`, `sandbox.ts:459-469`) instead of running anything. It
//! refuses rather than pretending: a bot that silently ran nothing would
//! look exactly like a bot whose command produced no output. `shell`
//! defaults to `ask` (S2-02's `default_decisions`), which is what S2-03's
//! approval plumbing needs a real gated tool to exercise - a sandbox that
//! actually executes is a later ticket.
use model::ToolSpec;
use serde_json::json;

pub fn spec() -> ToolSpec {
    ToolSpec {
        name: "shell".to_string(),
        description: "Run a shell command in your own private sandbox. You have a /work \
directory that keeps its contents between runs. There is no network in here."
            .to_string(),
        parameters: json!({
            "type": "object",
            "properties": {
                "command": { "type": "string", "description": "A shell command, run with sh -c." }
            },
            "required": ["command"]
        }),
    }
}

pub fn run(_args: &str) -> String {
    "No sandbox is available, so nothing was run. Sandboxing is off here. Set \
BULLPEN_SANDBOX=on where it is wanted."
        .to_string()
}
