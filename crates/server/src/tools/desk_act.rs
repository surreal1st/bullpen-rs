//! `desk_act`: the xdotool coordinate surface - the actual computer-use
//! capability. Port of TS `deskAction` and its constants
//! (`bullpen-night/src/server/desk.ts:489-497`, `:585-660`), the spec
//! (`app.ts:5223-5268`) and the dispatch loop (`app.ts:7123-7150`).
//!
//! 🔴 Reaches past the browser entirely, same as `desk_shell`
//! (`tools::desk_shell`'s own header doc) - `xdotool` on `DISPLAY :1` is the
//! one thing that reaches a terminal, a file manager, or any window at
//! all, the gap `click`/`type_text` (CDP-by-text, inside Chromium only) do
//! not cover.
//!
//! Runs THROUGH `desk_shell`'s own seams - `desk_shell_result`/
//! `desk_shell_stdin` (`super::desk_shell`) - never a second path to
//! docker: `xdotool` is just another command in the same container, the
//! same reasoning `desk_shell`'s own module doc gives for staying off
//! `crate::sandbox`.
//!
//! S8c-03's own deviation from TS's spec wording: see `desk_act_spec`'s own
//! doc for why every sentence telling the model to call a nonexistent
//! `snap_desk` was cut rather than ported verbatim.

use std::sync::OnceLock;

use model::ToolSpec;
use regex::Regex;
use serde_json::{Value, json};

use crate::desk::DeskConfig;
use crate::vm::DockerRun;

use super::desk_shell::{ShellResult, desk_shell_result, desk_shell_stdin};
use super::fence_tool_output;

/* --------------------------------------------------------------- constants */

/// TS `COORD_MAX` (`desk.ts:489`). Pixel coordinates above this are refused
/// rather than clamped - a bot that thinks the screen is bigger than it
/// actually is should be told so, not silently redirected somewhere it
/// never asked for.
const COORD_MAX: i64 = 4096;

/// TS `AMOUNT_MAX` (`desk.ts:490`) - scroll `amount`, in wheel clicks.
const AMOUNT_MAX: i64 = 20;

/// TS `WAIT_MAX_MS` (`desk.ts:491`).
const WAIT_MAX_MS: i64 = 5_000;

/// TS `TEXT_MAX_CHARS` (`desk.ts:492`). Counted in `char`s (Unicode scalar
/// values), the same char-safety deviation `tools::desk_shell::MAX_SHELL_BYTES`'s
/// own doc already gives for the identical reason: TS's `.length` counts
/// UTF-16 code units, a different number again for anything outside the
/// Basic Multilingual Plane, and this port does not reproduce that.
const TEXT_MAX_CHARS: usize = 2_000;

/// TS `DEFAULT_SCROLL_AMOUNT` (`desk.ts:493`).
const DEFAULT_SCROLL_AMOUNT: i64 = 3;

/// TS `POST_ACTION_SETTLE_MS` (`desk.ts:497`) - TS's own comment: "So the
/// next screenshot sees the result of the action just taken, not a
/// half-finished drag." bullpen-rs has no screenshot yet (see
/// `desk_act_spec`'s own doc), but the same settle reasoning still applies
/// to whatever a bot does next - another `desk_act` call, or a `desk_shell`
/// command that reads the result.
const POST_ACTION_SETTLE_MS: u64 = 300;

/// TS `KNOWN_KINDS` (`app.ts:7125`) - the seven action kinds this tool
/// accepts. Checked by `run_desk_act`'s own loop below BEFORE `desk_action`
/// ever sees an item - the same two-layer split TS itself has (`app.ts`'s
/// dispatch loop gates `kind`; `desk.ts`'s `deskAction` gates every OTHER
/// field). Not collapsed into one layer: a caller that skips the outer gate
/// (this module's own unit tests, calling `desk_action` directly) is then
/// exercising exactly what TS's `deskAction` alone can see, no more and no
/// less.
const KNOWN_KINDS: [&str; 7] = ["click", "move", "drag", "scroll", "type", "key", "wait"];

/// TS `KEYS_PATTERN` (`desk.ts:495`) - xdotool key syntax: names and
/// modifiers joined with `+`, such as `ctrl+l`. Compiled once behind a
/// `OnceLock` rather than a fresh `regex::Regex::new` per call (the
/// `egress.rs` precedent this crate otherwise follows for one-off request
/// parsing) because `key` is a plausibly high-frequency action and its
/// pattern never changes between calls.
///
/// 🔴 **Anchored at both ends on purpose - this is the ticket's named
/// security property.** `regex::Regex::is_match` is a SEARCH over the
/// haystack, not a full-string match: an unanchored `[A-Za-z0-9+_]{1,40}`
/// would report a match against `"a; touch /tmp/pwned"` (the leading `"a"`
/// alone satisfies the character class) even though the full string is
/// plainly not xdotool key syntax, and `keys` is interpolated straight into
/// a shell command below with no other check protecting it. Keeping
/// `^`/`$` means `is_match` can only succeed when the ENTIRE haystack, from
/// position 0 to its end, satisfies the class - see bite (b) in this
/// ticket's Result for the guard-removed version proved unsafe.
fn keys_pattern() -> &'static Regex {
    static PATTERN: OnceLock<Regex> = OnceLock::new();
    PATTERN.get_or_init(|| Regex::new(r"^[A-Za-z0-9+_]{1,40}$").expect("KEYS_PATTERN must compile"))
}

/* ------------------------------------------------------------------ sleep */

/// Test/production seam for the settle delay after each action - port of
/// TS `DeskActionDeps.sleep` (`desk.ts:567-574`), scoped down to ONLY
/// sleep: TS's own `deps.shell`/`deps.shellStdin` have no Rust equivalent
/// to add here, because `docker: &dyn DockerRun` is already this port's DI
/// seam for both of those (`desk_shell_result`/`desk_shell_stdin` take it
/// directly). Mirrors `workers::Clock`'s identical reasoning ("Mirrors TS's
/// deps.now/deps.sleep") rather than reusing that trait itself - `Clock`
/// also carries `now_ms`, a capability `desk_action` has no use for and
/// would have to fake for no reason.
#[async_trait::async_trait]
pub trait Sleeper: Send + Sync {
    async fn sleep(&self, ms: u64);
}

/// The real clock. `tools/mod.rs`'s `desk_act` dispatch arm uses this;
/// every test in this module uses a no-op fake instead, so a test does not
/// really wait `POST_ACTION_SETTLE_MS` per action - "a test suite that
/// really sleeps is a test suite nobody runs" (this ticket's own words).
pub struct RealSleeper;

#[async_trait::async_trait]
impl Sleeper for RealSleeper {
    async fn sleep(&self, ms: u64) {
        tokio::time::sleep(std::time::Duration::from_millis(ms)).await;
    }
}

/* -------------------------------------------------------------- coord/point */

/// Port of TS `isCoord` (`desk.ts:499-501`), against a raw `serde_json::Value`
/// rather than a typed struct field. TS's own `DeskAction` fields are
/// `unknown` on purpose (`desk.ts:475-478`'s own comment: "this is what a
/// bot's tool-call arguments look like before anything has checked them"):
/// this function is the ONE place that checks them, and handing it a typed
/// struct via `serde`'s derive would move that check to `serde` ITSELF,
/// before this function ever runs, under `serde`'s own coercion/defaulting
/// rules rather than TS's - a missing `x` on a `#[serde(default)]` field
/// would silently become the coordinate `0` instead of the refusal a bot
/// needs to read to correct itself. `Value::as_f64` already returns `None`
/// for every JSON type but `Number` (a JSON string `"100"`, `null`, `true`,
/// an object, an array all fail here the same way TS's own
/// `typeof n === "number"` guard rejects them), so no separate type match
/// is needed. Rejects fractional coordinates the same way `Number.isInteger`
/// does - JS makes no textual distinction between `100` and `100.0` (both
/// are simply the number 100), so this checks the VALUE's integrality
/// (`f.fract() == 0.0`), never whether the JSON literal happened to carry a
/// decimal point.
fn is_coord(v: Option<&Value>) -> bool {
    v.and_then(Value::as_f64)
        .is_some_and(|f| f.is_finite() && f.fract() == 0.0 && f >= 0.0 && f <= COORD_MAX as f64)
}

/// Port of TS `isPoint` (`desk.ts:503-507`): `null` and anything that is
/// not a JSON object are refused before ever reading `.x`/`.y`, matching
/// TS's own `p === null || typeof p !== "object"` guard. A JSON array also
/// satisfies TS's `typeof === "object"`, but `.x`/`.y` on one are
/// `undefined`, so `isCoord(undefined)` still fails there - this match
/// reaches the same `false` for a `Value::Array` by a different route (the
/// `Some(Value::Object(_))` arm simply never matches it), not by a special
/// case.
fn is_point(v: Option<&Value>) -> bool {
    match v {
        Some(Value::Object(map)) => is_coord(map.get("x")) && is_coord(map.get("y")),
        _ => false,
    }
}

/// Reads a coordinate already proved valid by `is_coord`. `.expect()` is
/// safe here ONLY because every caller calls `is_coord` (directly or via
/// `xy`/`is_point`) on the exact same `Option<&Value>` first and refuses
/// before ever reaching this - never called on unvalidated input.
fn coord(v: Option<&Value>) -> i64 {
    v.and_then(Value::as_f64)
        .expect("coord must be validated by is_coord before calling coord()") as i64
}

/// Shared by `click`/`move`/`scroll` (`desk.ts:608-609`, `:615-617`,
/// `:627-629`) - all three refuse with the identical message when `x`/`y`
/// fail `is_coord`, so this is one function rather than the same lines
/// copied three times.
fn xy(action: &Value) -> Result<(i64, i64), String> {
    let x = action.get("x");
    let y = action.get("y");
    if !is_coord(x) || !is_coord(y) {
        return Err(format!(
            "x and y must be whole numbers from 0 to {COORD_MAX}."
        ));
    }
    Ok((coord(x), coord(y)))
}

/// Port of TS `xdotoolButton` (`desk.ts:510-512`): left=1, middle=2,
/// right=3. `button` is `unknown`, so this checks the exact string the same
/// way TS's `===` chain does - anything else, including a non-string
/// value, falls through to the default (left) rather than refusing,
/// matching TS's own ternary exactly (it never refuses a bad `button`,
/// only defaults it; only `keys` and the coordinate/amount/ms/text fields
/// are refusal surfaces).
fn xdotool_button(v: Option<&Value>) -> u8 {
    match v.and_then(Value::as_str) {
        Some("right") => 3,
        Some("middle") => 2,
        _ => 1,
    }
}

/* --------------------------------------------------------- per-kind builders */

/// What a validated non-`wait` action turns into: either a plain command
/// (no stdin) or `type`'s validated text, which travels over
/// `desk_shell_stdin`'s own `stdin` parameter. TS expresses this same fork
/// with two loose local variables, `command`/`stdin` (`desk.ts:604-605`,
/// `stdin === undefined ? shell(...) : shellStdin(...)` at `:657`) - a
/// shape a later edit could set inconsistently (both, or neither). This
/// enum makes the two cases mutually exclusive by construction instead.
enum PreparedAction {
    Command(String),
    TypedText(String),
}

/// Port of TS's `click` branch (`desk.ts:607-613`).
fn click_command(action: &Value) -> Result<String, String> {
    let (x, y) = xy(action)?;
    let button = xdotool_button(action.get("button"));
    let double = matches!(action.get("double"), Some(Value::Bool(true)));
    let repeat = if double { "--repeat 2 " } else { "" };
    Ok(format!(
        "DISPLAY=:1 xdotool mousemove {x} {y} click {repeat}{button}"
    ))
}

/// Port of TS's `move` branch (`desk.ts:614-618`).
fn move_command(action: &Value) -> Result<String, String> {
    let (x, y) = xy(action)?;
    Ok(format!("DISPLAY=:1 xdotool mousemove {x} {y}"))
}

/// Port of TS's `drag` branch (`desk.ts:619-625`).
fn drag_command(action: &Value) -> Result<String, String> {
    let from = action.get("from");
    let to = action.get("to");
    if !is_point(from) || !is_point(to) {
        return Err(format!(
            "from and to must both have whole numbers from 0 to {COORD_MAX}."
        ));
    }
    // Safe: `is_point` just proved both are `Value::Object` with valid
    // `x`/`y` coordinates.
    let from = from
        .and_then(Value::as_object)
        .expect("checked by is_point");
    let to = to.and_then(Value::as_object).expect("checked by is_point");
    let (fx, fy) = (coord(from.get("x")), coord(from.get("y")));
    let (tx, ty) = (coord(to.get("x")), coord(to.get("y")));
    Ok(format!(
        "DISPLAY=:1 xdotool mousemove {fx} {fy} mousedown 1 mousemove {tx} {ty} mouseup 1"
    ))
}

/// Port of TS's `scroll` branch (`desk.ts:626-638`). `amount`'s default
/// applies ONLY when the field is missing or JSON `null` (TS's `??`,
/// nullish coalescing - it does NOT trigger for `0`, which is exactly why
/// `amount: 0` is a REFUSAL rather than a silent default in this ticket's
/// own validation table). Missing/`Value::Null` both map to
/// `DEFAULT_SCROLL_AMOUNT` here; anything else is validated as given, same
/// as `is_coord`'s float/negative/range checks.
fn scroll_command(action: &Value) -> Result<String, String> {
    let (x, y) = xy(action)?;

    let button = match action.get("direction").and_then(Value::as_str) {
        Some("up") => 4,
        Some("down") => 5,
        _ => return Err("direction must be \"up\" or \"down\".".to_string()),
    };

    let amount_field = action.get("amount");
    let nullish = matches!(amount_field, None | Some(Value::Null));
    let amount_raw = if nullish {
        Some(DEFAULT_SCROLL_AMOUNT as f64)
    } else {
        amount_field.and_then(Value::as_f64)
    };
    let amount = amount_raw
        .filter(|f| f.is_finite() && f.fract() == 0.0 && *f >= 1.0 && *f <= AMOUNT_MAX as f64);
    let Some(amount) = amount else {
        return Err(format!(
            "amount must be a whole number from 1 to {AMOUNT_MAX}."
        ));
    };
    let amount = amount as i64;

    Ok(format!(
        "DISPLAY=:1 xdotool mousemove {x} {y} click --repeat {amount} {button}"
    ))
}

/// Port of TS's `type` branch (`desk.ts:639-645`). Returns the validated
/// text itself, never a command - `desk_action` below sends it over
/// `desk_shell_stdin`'s own `stdin` parameter, never interpolated into the
/// command string. 🔴 This is the ticket's other named security property
/// (bite a): typed text must never reach argv. Length is counted in
/// `char`s, not bytes - see `TEXT_MAX_CHARS`'s own doc for why.
fn type_command(action: &Value) -> Result<String, String> {
    let text = action.get("text").and_then(Value::as_str);
    match text {
        Some(t) if !t.is_empty() && t.chars().count() <= TEXT_MAX_CHARS => Ok(t.to_string()),
        _ => Err(format!("text must be 1 to {TEXT_MAX_CHARS} characters.")),
    }
}

/// Port of TS's `key` branch - TS's final, unconditional `else`
/// (`desk.ts:646-654`). 🔴 The OTHER injection surface this ticket names:
/// `keys` is interpolated straight into the command string below, safe
/// ONLY because `keys_pattern()` is anchored at both ends - see that
/// function's own doc and bite (b) in this ticket's Result.
fn key_command(action: &Value) -> Result<String, String> {
    let keys = action.get("keys").and_then(Value::as_str);
    match keys {
        Some(k) if keys_pattern().is_match(k) => Ok(format!("DISPLAY=:1 xdotool key {k}")),
        _ => Err(
            "keys must be xdotool key syntax like ctrl+l or Return, matching \
/^[A-Za-z0-9+_]{1,40}$/."
                .to_string(),
        ),
    }
}

/// Port of TS's `wait` branch (`desk.ts:595-602`) - the ONE kind that
/// returns without ever building a command or touching `desk_shell`'s own
/// seams, and the ONE kind that sleeps its OWN duration rather than
/// `POST_ACTION_SETTLE_MS`.
async fn desk_action_wait(action: &Value, sleeper: &dyn Sleeper) -> ShellResult {
    let ms = action
        .get("ms")
        .and_then(Value::as_f64)
        .filter(|f| f.is_finite() && f.fract() == 0.0 && *f >= 0.0 && *f <= WAIT_MAX_MS as f64);
    let Some(ms) = ms else {
        return ShellResult {
            ok: false,
            output: format!("ms must be a whole number from 0 to {WAIT_MAX_MS}."),
        };
    };
    let ms = ms as u64;
    sleeper.sleep(ms).await;
    ShellResult {
        ok: true,
        output: format!("Waited {ms}ms."),
    }
}

/* ---------------------------------------------------------------- the engine */

/// Port of TS `deskAction` (`desk.ts:586-660`): one coordinate-level action
/// on the calling bot's own machine, run through `desk_shell`'s own seams
/// (`desk_shell_result`/`desk_shell_stdin`) exactly like `desk_shell` does -
/// `xdotool` on `DISPLAY :1` is just another command in that same
/// container, so this never opens a second path to docker.
///
/// Validates BEFORE ever building a command string, same order TS's own
/// if/else chain checks in - a bad coordinate, an out-of-range amount or
/// duration, or a `keys` string that is not plain xdotool syntax never
/// reaches the shell at all, and never sleeps either (TS's own early
/// `return` inside each branch never reaches its trailing `sleep` -
/// `desk.ts:657-659` runs only once a `command`/`stdin` was actually
/// built). Sleeps `POST_ACTION_SETTLE_MS` after every action that DID run,
/// success or failure alike - ported faithfully: TS's own tail runs
/// unconditionally on `result`, not only when `result.ok`.
///
/// `action` is expected to already have a known `kind` - `run_desk_act`'s
/// own loop below gates that BEFORE ever calling this, the same two-layer
/// split TS has across `app.ts`/`desk.ts`.
pub async fn desk_action(
    docker: &dyn DockerRun,
    config: &DeskConfig,
    action: &Value,
    sleeper: &dyn Sleeper,
) -> ShellResult {
    let kind = action.get("kind").and_then(Value::as_str).unwrap_or("");

    if kind == "wait" {
        return desk_action_wait(action, sleeper).await;
    }

    let prepared: Result<PreparedAction, String> = match kind {
        "click" => click_command(action).map(PreparedAction::Command),
        "move" => move_command(action).map(PreparedAction::Command),
        "drag" => drag_command(action).map(PreparedAction::Command),
        "scroll" => scroll_command(action).map(PreparedAction::Command),
        "type" => type_command(action).map(PreparedAction::TypedText),
        // TS's own final `else` (`desk.ts:646`) has no explicit
        // `kind === "key"` check either - safe ONLY because its one caller
        // (`app.ts`'s dispatch loop, `run_desk_act` below) already gated
        // `kind` against `KNOWN_KINDS` before ever calling `deskAction`.
        // Ported faithfully, not re-checked here: a redundant check would
        // be dead code no caller in this codebase can ever exercise.
        _ => key_command(action).map(PreparedAction::Command),
    };

    let prepared = match prepared {
        Ok(p) => p,
        Err(output) => return ShellResult { ok: false, output },
    };

    let result = match prepared {
        PreparedAction::Command(cmd) => desk_shell_result(docker, config, &cmd).await,
        PreparedAction::TypedText(text) => {
            desk_shell_stdin(
                docker,
                config,
                "DISPLAY=:1 xdotool type --delay 20 --file -",
                &text,
            )
            .await
        }
    };

    sleeper.sleep(POST_ACTION_SETTLE_MS).await;
    result
}

/* -------------------------------------------------------------------- spec */

/// Spec text ADAPTED from TS `app.ts:5223-5268`, NOT verbatim - see this
/// ticket's own "the spec text needs adapting" section. Two changes to the
/// top-level `description`, nothing else (the `parameters` schema below
/// mentions `snap_desk` nowhere and needed no change):
///
/// 1. TS's "the shared computer" -> "this bot's machine", the wording
///    S8c-01 already established for the identical reason
///    (`desk::RealCdpVersion`'s own doc: "This bot's browser is not
///    answering.").
/// 2. TS tells the model to "Call snap_desk first ... then snap_desk
///    again" - **`snap_desk` does not exist in this crate.** No route from
///    a bot's tool call to a screenshot exists yet
///    (`model::ContentPart::ImageUrl` has no constructor anywhere in
///    `crates/server`); Josh decided 2026-09-17 that a bot SHOULD get its
///    screenshot back, but that is a separate, not-yet-built design
///    effort. Pointing a model at a tool that will always answer
///    `"Unknown tool: snap_desk"` is the exact defect `review_media` was
///    held back over (this ticket's own words) - that sentence is CUT, not
///    ported, and replaced with the plain truth: mouse actions need
///    coordinates read off a picture this bot has no way to get yet, so
///    `key`/`type`/`wait` (driving a window from the keyboard) are what
///    actually works today. The "prefer browse/click/type_text, they cost
///    far less" guidance survives verbatim - still true, still the cheaper
///    path for anything inside the browser.
pub fn desk_act_spec() -> ToolSpec {
    ToolSpec {
        name: "desk_act".to_string(),
        description: "Press buttons and move the mouse on this bot's machine by pixel \
coordinates - reaching anything on screen, not only what a browser shows. Mouse actions need \
coordinates read off a picture of the screen, and there is no way yet to get one - key, type and \
wait, which drive a window from the keyboard, are what actually works today. Prefer browse, \
click and type_text for anything inside the browser - they read the page instead of a picture \
and cost far less."
            .to_string(),
        parameters: json!({
            "type": "object",
            "properties": {
                "actions": {
                    "type": "array",
                    "maxItems": 10,
                    "description": "Up to 10 actions, run in order. Stops at the first that fails.",
                    "items": {
                        "type": "object",
                        "properties": {
                            "kind": {
                                "type": "string",
                                "enum": ["click", "move", "drag", "scroll", "type", "key", "wait"],
                                "description": "click/move/scroll need x,y. drag needs from and \
        to, each {x,y}. type needs text. key needs keys (xdotool syntax, e.g. ctrl+l or Return). wait \
        needs ms."
                            },
                            "x": { "type": "number", "description": "0-4096." },
                            "y": { "type": "number", "description": "0-4096." },
                            "button": {
                                "type": "string",
                                "enum": ["left", "right", "middle"],
                                "description": "click only. Default left."
                            },
                            "double": { "type": "boolean", "description": "click only. Double-click." },
                            "from": {
                                "type": "object",
                                "description": "drag only.",
                                "properties": { "x": { "type": "number" }, "y": { "type": "number" } }
                            },
                            "to": {
                                "type": "object",
                                "description": "drag only.",
                                "properties": { "x": { "type": "number" }, "y": { "type": "number" } }
                            },
                            "direction": { "type": "string", "enum": ["up", "down"], "description": "scroll only." },
                            "amount": { "type": "number", "description": "scroll only, 1-20. Default 3." },
                            "text": { "type": "string", "description": "type only, up to 2000 characters." },
                            "keys": { "type": "string", "description": "key only, xdotool key syntax." },
                            "ms": { "type": "number", "description": "wait only, up to 5000." }
                        },
                        "required": ["kind"]
                    }
                }
            },
            "required": ["actions"]
        }),
    }
}

/* ---------------------------------------------------------------- the tool */

/// Port of the `desk_act` dispatch loop (`app.ts:7123-7150`). Unlike
/// `desk_shell`'s single-command tool, the "one call, up to 10 actions,
/// stop at the first failure" batching lives HERE, in the TOOL ENTRY
/// POINT, exactly where TS puts it (`app.ts`, not `desk.ts`) - `deskAction`
/// itself only ever runs ONE action.
///
/// `sleeper` is threaded all the way through from the caller (contrast
/// `run_desk_shell`, which has nothing to inject) so this crate's OWN
/// tests - not just `desk_action`'s own unit tests - can drive the full
/// batch loop, including bite (c)'s three-action test, without real
/// `POST_ACTION_SETTLE_MS` waits stacking up. `tools/mod.rs`'s `desk_act`
/// dispatch arm passes `&RealSleeper` for production; nothing else in this
/// crate needs to know a `Sleeper` exists.
pub async fn run_desk_act(
    docker: &dyn DockerRun,
    config: &DeskConfig,
    args: &str,
    sleeper: &dyn Sleeper,
) -> String {
    const MAX_ACTIONS: usize = 10;

    let parsed: Value = serde_json::from_str(args).unwrap_or(Value::Null);
    let raw: Vec<Value> = parsed
        .get("actions")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();

    if raw.is_empty() {
        return "No actions were given.".to_string();
    }
    if raw.len() > MAX_ACTIONS {
        return format!("At most {MAX_ACTIONS} actions per call, got {}.", raw.len());
    }

    let mut lines: Vec<String> = Vec::with_capacity(raw.len());
    for (i, item) in raw.iter().enumerate() {
        let known = item
            .get("kind")
            .and_then(Value::as_str)
            .is_some_and(|k| KNOWN_KINDS.contains(&k));
        if !item.is_object() || !known {
            lines.push(format!(
                "{}. refused: each action needs a known \"kind\".",
                i + 1
            ));
            break;
        }

        let result = desk_action(docker, config, item, sleeper).await;

        // Decision (fencing): the numeric prefix and "(done)"/"(no output)"
        // are SERVER-generated - same reasoning `tools::desk_shell`'s own
        // module doc gives for its refusal strings staying unfenced. Only
        // the actual per-action command output is machine-derived, off a
        // container with a network route, so only IT goes through
        // `fence_tool_output`, and only when non-empty - the exact
        // condition `run_desk_shell` itself already fences under, applied
        // per line instead of to one combined block.
        let line = if result.output.is_empty() {
            if result.ok {
                "(done)".to_string()
            } else {
                "(no output)".to_string()
            }
        } else {
            fence_tool_output(&result.output)
        };
        lines.push(format!("{}. {line}", i + 1));

        if !result.ok {
            break;
        }
    }

    lines.join("\n")
}

#[cfg(test)]
mod tests {
    use super::*;
    use async_trait::async_trait;
    use std::sync::Mutex;
    use store::vms::DockerResult;

    fn config() -> DeskConfig {
        DeskConfig {
            cdp: "http://127.0.0.1:9400".to_string(),
            view: "http://127.0.0.1:6400".to_string(),
            container: "bullpen-vm-arthur".to_string(),
            docker_host: "unix:///test.sock".to_string(),
        }
    }

    /// No-op `Sleeper` - every test in this module uses this instead of
    /// `RealSleeper`, so none of them really wait `POST_ACTION_SETTLE_MS`
    /// or a `wait` action's own `ms`.
    struct NoSleep;

    #[async_trait]
    impl Sleeper for NoSleep {
        async fn sleep(&self, _ms: u64) {}
    }

    /// Panics if `call` or `call_with_stdin` is ever reached - used by every
    /// validation-refusal test below to prove the refusal happens BEFORE
    /// any command is built, exactly like `desk_shell.rs`'s own
    /// `PanicsIfCalled`-style fakes.
    struct PanicsIfCalled;

    #[async_trait]
    impl DockerRun for PanicsIfCalled {
        async fn call(&self, args: &[&str], _timeout_ms: u64) -> DockerResult {
            panic!("a refused action must never reach docker, got call({args:?})");
        }

        async fn call_with_stdin(
            &self,
            args: &[&str],
            _stdin: &str,
            _timeout_ms: u64,
        ) -> DockerResult {
            panic!("a refused action must never reach docker, got call_with_stdin({args:?})");
        }
    }

    /// Records every `call` and `call_with_stdin` invocation - argv and
    /// stdin (empty string for `call`) - and answers a scripted sequence of
    /// results, one per call, cycling the last if more calls arrive than
    /// scripted. Used for bites (a)/(c)/(d) and the "runs the command"
    /// happy-path tests, where a real (fake) docker call is expected.
    struct RecordingDocker {
        calls: Mutex<Vec<(Vec<String>, String)>>,
        responses: Vec<DockerResult>,
    }

    impl RecordingDocker {
        fn always_ok(stdout: &str) -> Self {
            Self::scripted(vec![DockerResult {
                ok: true,
                stdout: stdout.to_string(),
                stderr: String::new(),
            }])
        }

        fn scripted(responses: Vec<DockerResult>) -> Self {
            Self {
                calls: Mutex::new(Vec::new()),
                responses,
            }
        }

        fn calls(&self) -> Vec<(Vec<String>, String)> {
            self.calls.lock().unwrap().clone()
        }

        fn next_response(&self, index: usize) -> DockerResult {
            self.responses
                .get(index)
                .or_else(|| self.responses.last())
                .cloned()
                .unwrap_or(DockerResult {
                    ok: true,
                    stdout: String::new(),
                    stderr: String::new(),
                })
        }
    }

    #[async_trait]
    impl DockerRun for RecordingDocker {
        async fn call(&self, args: &[&str], _timeout_ms: u64) -> DockerResult {
            let mut calls = self.calls.lock().unwrap();
            let index = calls.len();
            calls.push((args.iter().map(|s| s.to_string()).collect(), String::new()));
            self.next_response(index)
        }

        async fn call_with_stdin(
            &self,
            args: &[&str],
            stdin: &str,
            _timeout_ms: u64,
        ) -> DockerResult {
            let mut calls = self.calls.lock().unwrap();
            let index = calls.len();
            calls.push((
                args.iter().map(|s| s.to_string()).collect(),
                stdin.to_string(),
            ));
            self.next_response(index)
        }
    }

    fn ok_result() -> DockerResult {
        DockerResult {
            ok: true,
            stdout: String::new(),
            stderr: String::new(),
        }
    }

    /// Empty output on purpose: these tests are about the BATCH LOOP
    /// (stopping at the first failure, and the "(no output)" substitution),
    /// not about fencing - `machine_derived_output_is_fenced_but_the_prefix_is_not`
    /// below already covers a failure/success with real text in it.
    fn fail_result() -> DockerResult {
        DockerResult {
            ok: false,
            stdout: String::new(),
            stderr: String::new(),
        }
    }

    /* -------------------------------------------------- validation table */

    /// 🔴 Table test named in this ticket: every listed validation refusal,
    /// against the raw per-kind builders directly - none of these ever
    /// reach a `DockerRun`, so there is no fake to wire up and no sleep to
    /// skip. Each case's expected string is TS's own refusal message,
    /// verbatim.
    #[test]
    fn validation_refusals() {
        let coord_msg = format!("x and y must be whole numbers from 0 to {COORD_MAX}.");
        let point_msg = format!("from and to must both have whole numbers from 0 to {COORD_MAX}.");
        let amount_msg = format!("amount must be a whole number from 1 to {AMOUNT_MAX}.");
        let text_msg = format!("text must be 1 to {TEXT_MAX_CHARS} characters.");
        let keys_msg = "keys must be xdotool key syntax like ctrl+l or Return, matching \
/^[A-Za-z0-9+_]{1,40}$/."
            .to_string();

        // Every builder shares the signature `Result<String, String>`
        // (`click_command`/`move_command`/`drag_command`/`scroll_command`/
        // `type_command`/`key_command`), so each case just calls the right
        // one eagerly and the loop below only ever compares `Result`s - no
        // dispatch-by-name needed.
        type Case = (&'static str, Result<String, String>, Result<(), String>);
        let cases: Vec<Case> = vec![
            (
                "click: float x",
                click_command(&json!({"x": 100.5, "y": 10})),
                Err(coord_msg.clone()),
            ),
            (
                "click: negative x",
                click_command(&json!({"x": -1, "y": 10})),
                Err(coord_msg.clone()),
            ),
            (
                "click: x over COORD_MAX",
                click_command(&json!({"x": COORD_MAX + 1, "y": 10})),
                Err(coord_msg.clone()),
            ),
            (
                "click: string x",
                click_command(&json!({"x": "100", "y": 10})),
                Err(coord_msg.clone()),
            ),
            (
                "click: missing y",
                click_command(&json!({"x": 10})),
                Err(coord_msg.clone()),
            ),
            (
                "click: null y",
                click_command(&json!({"x": 10, "y": null})),
                Err(coord_msg.clone()),
            ),
            (
                "click: x at COORD_MAX is fine",
                click_command(&json!({"x": COORD_MAX, "y": 0})),
                Ok(()),
            ),
            (
                "drag: missing to",
                drag_command(&json!({"from": {"x": 1, "y": 1}})),
                Err(point_msg.clone()),
            ),
            (
                "scroll: amount 0",
                scroll_command(&json!({"x": 1, "y": 1, "direction": "up", "amount": 0})),
                Err(amount_msg.clone()),
            ),
            (
                "scroll: amount 21",
                scroll_command(&json!({"x": 1, "y": 1, "direction": "up", "amount": 21})),
                Err(amount_msg.clone()),
            ),
            (
                "scroll: amount null defaults, is fine",
                scroll_command(&json!({"x": 1, "y": 1, "direction": "up", "amount": null})),
                Ok(()),
            ),
            (
                "scroll: direction sideways",
                scroll_command(&json!({"x": 1, "y": 1, "direction": "sideways"})),
                Err("direction must be \"up\" or \"down\".".to_string()),
            ),
            (
                "type: empty text",
                type_command(&json!({"text": ""})),
                Err(text_msg.clone()),
            ),
            (
                "type: text at 2001 chars",
                type_command(&json!({"text": "a".repeat(TEXT_MAX_CHARS + 1)})),
                Err(text_msg.clone()),
            ),
            (
                "type: text at 2000 chars is fine",
                type_command(&json!({"text": "a".repeat(TEXT_MAX_CHARS)})),
                Ok(()),
            ),
            (
                "key: semicolon injection attempt",
                key_command(&json!({"keys": "a; touch /tmp/pwned"})),
                Err(keys_msg.clone()),
            ),
            (
                "key: not a string",
                key_command(&json!({"keys": 5})),
                Err(keys_msg.clone()),
            ),
            (
                "key: valid ctrl+l",
                key_command(&json!({"keys": "ctrl+l"})),
                Ok(()),
            ),
        ];

        for (name, got, expected) in cases {
            match expected {
                Ok(()) => assert!(got.is_ok(), "{name}: expected Ok, got {got:?}"),
                Err(msg) => assert_eq!(got, Err(msg), "{name}"),
            }
        }
    }

    #[tokio::test]
    async fn wait_ms_negative_and_over_max_are_refused_without_sleeping() {
        for ms in [json!(-1), json!(5001)] {
            let action = json!({"kind": "wait", "ms": ms});
            let result = desk_action(&PanicsIfCalled, &config(), &action, &NoSleep).await;
            assert!(!result.ok, "ms={ms:?} should be refused, got {result:?}");
            assert_eq!(
                result.output,
                format!("ms must be a whole number from 0 to {WAIT_MAX_MS}.")
            );
        }
    }

    #[tokio::test]
    async fn wait_valid_ms_sleeps_and_never_touches_docker() {
        let action = json!({"kind": "wait", "ms": 5000});
        let result = desk_action(&PanicsIfCalled, &config(), &action, &NoSleep).await;
        assert!(result.ok);
        assert_eq!(result.output, "Waited 5000ms.");
    }

    /* --------------------------------------------------------------- bites */

    /// 🔴 BITE (a) target. GUARD-PRESENT: a `type` action whose text is a
    /// shell-injection attempt reaches `call_with_stdin`'s own `stdin`
    /// parameter and never appears anywhere in the recorded argv.
    #[tokio::test]
    async fn bite_a_typed_text_never_reaches_argv() {
        let docker = RecordingDocker::always_ok("");
        let payload = "; rm -rf / #";
        let action = json!({"kind": "type", "text": payload});

        let result = desk_action(&docker, &config(), &action, &NoSleep).await;

        assert!(result.ok, "expected the type action to succeed: {result:?}");
        let calls = docker.calls();
        assert_eq!(
            calls.len(),
            1,
            "type must go through call_with_stdin exactly once"
        );
        let (args, stdin) = &calls[0];
        assert_eq!(
            stdin, payload,
            "the payload must reach call_with_stdin's own stdin"
        );
        assert!(
            !args.iter().any(|a| a.contains("rm -rf")),
            "the payload must never appear in argv, got {args:?}"
        );
        assert!(
            args.contains(&"-i".to_string()),
            "the stdin path must pass -i"
        );
        assert!(
            args.iter()
                .any(|a| a == "DISPLAY=:1 xdotool type --delay 20 --file -"),
            "the command itself carries no typed text, got {args:?}"
        );
    }

    /// 🔴 BITE (b) target. GUARD-PRESENT: `keys` containing a shell
    /// separator is refused by `KEYS_PATTERN` and no command ever reaches
    /// docker (`PanicsIfCalled` would panic if it did). See this ticket's
    /// Result for the literal red/green proving the guard-removed
    /// (unanchored) version fails this exact test.
    #[tokio::test]
    async fn bite_b_keys_is_fully_anchored() {
        let action = json!({"kind": "key", "keys": "a; touch /tmp/pwned"});

        let result = desk_action(&PanicsIfCalled, &config(), &action, &NoSleep).await;

        assert!(
            !result.ok,
            "an unanchored-looking keys string must be refused"
        );
        assert_eq!(
            result.output,
            "keys must be xdotool key syntax like ctrl+l or Return, matching \
/^[A-Za-z0-9+_]{1,40}$/."
        );
    }

    /// 🔴 BITE (c) target. GUARD-PRESENT: three actions where the second
    /// fails produce exactly TWO numbered lines, and the fake records
    /// exactly two `call`s - the third action never runs.
    #[tokio::test]
    async fn bite_c_a_failed_action_stops_the_batch() {
        let docker = RecordingDocker::scripted(vec![ok_result(), fail_result(), ok_result()]);
        let args = json!({
            "actions": [
                {"kind": "key", "keys": "a"},
                {"kind": "key", "keys": "b"},
                {"kind": "key", "keys": "c"},
            ]
        })
        .to_string();

        let output = run_desk_act(&docker, &config(), &args, &NoSleep).await;

        let lines: Vec<&str> = output.lines().collect();
        assert_eq!(
            lines.len(),
            2,
            "expected exactly two numbered lines, got {output:?}"
        );
        assert!(lines[0].starts_with("1. "));
        assert!(lines[1].starts_with("2. "));
        assert_eq!(
            docker.calls().len(),
            2,
            "the third action must never reach docker"
        );
    }

    /// Happy path: three actions, all succeed, three lines, three calls -
    /// the mirror image of bite (c), proving the loop does not stop early
    /// when nothing fails.
    #[tokio::test]
    async fn all_actions_succeeding_runs_every_one() {
        let docker = RecordingDocker::scripted(vec![ok_result(), ok_result(), ok_result()]);
        let args = json!({
            "actions": [
                {"kind": "key", "keys": "a"},
                {"kind": "key", "keys": "b"},
                {"kind": "key", "keys": "c"},
            ]
        })
        .to_string();

        let output = run_desk_act(&docker, &config(), &args, &NoSleep).await;

        assert_eq!(output.lines().count(), 3, "got {output:?}");
        assert_eq!(docker.calls().len(), 3);
    }

    #[tokio::test]
    async fn empty_output_reads_as_done_when_ok_and_no_output_when_not() {
        let docker = RecordingDocker::scripted(vec![ok_result(), fail_result()]);
        let args = json!({
            "actions": [
                {"kind": "key", "keys": "a"},
                {"kind": "key", "keys": "b"},
            ]
        })
        .to_string();

        let output = run_desk_act(&docker, &config(), &args, &NoSleep).await;

        assert_eq!(output, "1. (done)\n2. (no output)");
    }

    #[tokio::test]
    async fn no_actions_is_refused_before_touching_docker() {
        let output = run_desk_act(&PanicsIfCalled, &config(), r#"{"actions":[]}"#, &NoSleep).await;
        assert_eq!(output, "No actions were given.");
    }

    #[tokio::test]
    async fn more_than_ten_actions_is_refused_before_touching_docker() {
        let actions: Vec<Value> = (0..11).map(|_| json!({"kind": "wait", "ms": 0})).collect();
        let args = json!({ "actions": actions }).to_string();

        let output = run_desk_act(&PanicsIfCalled, &config(), &args, &NoSleep).await;

        assert_eq!(output, "At most 10 actions per call, got 11.");
    }

    #[tokio::test]
    async fn an_unknown_kind_is_refused_and_breaks_the_batch() {
        let docker = RecordingDocker::always_ok("");
        let args = json!({
            "actions": [
                {"kind": "teleport", "x": 1, "y": 1},
                {"kind": "key", "keys": "a"},
            ]
        })
        .to_string();

        let output = run_desk_act(&docker, &config(), &args, &NoSleep).await;

        assert_eq!(output, "1. refused: each action needs a known \"kind\".");
        assert!(
            docker.calls().is_empty(),
            "an unknown kind must never reach docker"
        );
    }

    #[tokio::test]
    async fn a_non_object_action_is_refused() {
        let output = run_desk_act(
            &PanicsIfCalled,
            &config(),
            r#"{"actions":["not an object"]}"#,
            &NoSleep,
        )
        .await;
        assert_eq!(output, "1. refused: each action needs a known \"kind\".");
    }

    #[tokio::test]
    async fn machine_derived_output_is_fenced_but_the_prefix_is_not() {
        let docker = RecordingDocker::always_ok("some xdotool stderr text\n");
        let args = json!({ "actions": [{"kind": "key", "keys": "a"}] }).to_string();

        let output = run_desk_act(&docker, &config(), &args, &NoSleep).await;

        assert!(output.starts_with("1. <<<TOOL_OUTPUT_DATA>>>"));
        assert!(output.contains("some xdotool stderr text"));
    }
}
