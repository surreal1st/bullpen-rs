//! Port of `projects/bullpen-night/src/client/Approvals.tsx`: pending tool
//! calls a run parked on, rendered as cards below the thread and above the
//! composer - `App.tsx:1278-1300`'s placement ("it belongs beside the
//! conversation it came from - never folded into a popover... just above
//! the composer because that is where every editor puts the thing asking
//! for a decision"). Grouped on bot + tool + arguments
//! (`shared::approval_groups::group_approvals`) so a routine firing every 30
//! minutes does not grow this pane without bound.
//!
//! Not ported: `isClientFulfilled`/`approveRead`'s desktop file-read flow
//! (nothing in bullpen-rs's client has a desktop bridge yet) and the
//! `propose_tool` card (`GET /api/approvals/:id/proposal` is explicitly
//! SKIPPED by S2-03's own ticket - nothing proposes tools yet). The
//! is-question branch (`isQuestion`/`QuestionActions`) IS ported even though
//! `ask_josh` is `allow` by default in S2 (`crates/server/src/tools/mod.rs`)
//! and so never actually parks a run here - a bot with that default
//! tightened to `ask` would reach it, and the TS original treats that as
//! ordinary, not exceptional.

use crate::api;
use crate::events::{ChangeKind, subscribe_events};
use crate::types::PendingApproval;
use dioxus::prelude::*;
use shared::approval_groups::{Groupable, group_approvals};
use shared::ask_josh::{is_answerable, parse_ask_josh};

impl Groupable for PendingApproval {
    fn id(&self) -> &str {
        &self.id
    }
    fn bot_id(&self) -> &str {
        &self.bot_id
    }
    fn tool_name(&self) -> &str {
        &self.tool_name
    }
    fn tool_args(&self) -> &str {
        &self.tool_args
    }
}

async fn reload(mut items: Signal<Vec<PendingApproval>>) {
    if let Ok(list) = api::fetch_approvals().await {
        items.set(list);
    }
}

/// Fires every decision in a group (sequential - each one resumes a run,
/// and firing them at once against one SQLite file is a needless way to
/// find out what happens under contention, same reasoning the TS `decide`
/// gives), then reloads. A plain function rather than a closure stored
/// alongside the card's other state: every button that can decide a card
/// (Approve/Reject/Always/Never, an option button, the typed-answer form,
/// Skip) needs its own copy of "decide and reload", and a `Vec<String>`
/// capture is not `Copy`, so a single shared closure cannot be handed to
/// more than one of them.
fn fire_decide(
    items: Signal<Vec<PendingApproval>>,
    mut busy: Signal<bool>,
    ids: Vec<String>,
    approved: bool,
    result: Option<String>,
    remember: Option<&'static str>,
) {
    busy.set(true);
    spawn(async move {
        for id in ids {
            let _ = api::decide_approval(&id, approved, result.as_deref(), remember).await;
        }
        busy.set(false);
        reload(items).await;
    });
}

/// Where this request came from, in words Josh would use. A trimmed port of
/// the TS `causeOf`: no `routineName` branch - bullpen-rs's `trigger` column
/// never carries a routine's own name (see `types.rs`'s doc on
/// `PendingApproval`), only its bare kind.
fn cause_of(trigger: Option<&str>) -> &'static str {
    match trigger {
        Some(t) if t != "chat" => "from an unattended run",
        _ => "from your conversation",
    }
}

/// S4-05: what the "Why it's asking" line shows, derived from the two
/// judge columns `GET /api/approvals` now carries. `None` whenever either
/// half is missing - a plain grid "ask" (never judged) sends both `null`,
/// and a lone verdict with no reason (or vice versa) is not a state the
/// server ever produces, so treated the same as absent rather than guessed
/// at. Kept as a free function, not inlined into the component, so the
/// "renders nothing when both fields are null" bite is a plain `#[test]`
/// rather than a browser shot.
fn judge_line(verdict: Option<&str>, reason: Option<&str>) -> Option<(&'static str, String)> {
    let (verdict, reason) = (verdict?, reason?);
    let class = match verdict {
        "dangerous" => "dangerous",
        _ => "risky",
    };
    Some((class, format!("Why it's asking: {reason}")))
}

/// True for a C0 control (U+0000-U+001F) or DEL (U+007F) - the characters
/// that let a bot-chosen argument break the `<pre>`'s layout with a literal
/// newline/carriage-return rather than reorder its glyphs (design §5.5).
fn is_c0_or_del(c: char) -> bool {
    matches!(c, '\u{0000}'..='\u{001F}' | '\u{007F}')
}

/// True for a Unicode bidi-formatting character: LRM/RLM, the five
/// embedding/override controls, and the four isolate controls. This is the
/// exact set design §5.5 names, not "non-ASCII" in general - an accented or
/// CJK filename is legitimate content and must render unchanged.
fn is_bidi_format_char(c: char) -> bool {
    matches!(c, '\u{200E}' | '\u{200F}' | '\u{202A}'..='\u{202E}' | '\u{2066}'..='\u{2069}')
}

/// The approval card is the only thing Josh sees before deciding (§5.5): a
/// bot-chosen argument that embeds U+202E (RIGHT-TO-LEFT OVERRIDE) can make
/// the card display one filename while the bytes mean another, and a raw
/// `\r`/`\n` does the same thing more crudely by splitting or reflowing the
/// `<pre>`. Every C0 control, DEL, and bidi-formatting character is replaced
/// with its Rust `\u{..}` escape - **visibly, not silently stripped**:
/// removing the override would still leave the glyphs in honest left-to-right
/// order, but it throws away the fact that something was there, and a card
/// that looks merely odd is safer than one that looks clean. This never
/// touches the underlying value used elsewhere (the actual tool argument, or
/// what gets posted back as an answer) - only what gets painted.
fn sanitize_for_card(input: &str) -> String {
    let mut out = String::with_capacity(input.len());
    for c in input.chars() {
        if is_c0_or_del(c) || is_bidi_format_char(c) {
            out.push_str(&format!("\\u{{{:x}}}", c as u32));
        } else {
            out.push(c);
        }
    }
    out
}

/// Renders a tool call's arguments for the card. A single-key object shows
/// its one value bare (what matters when approving `shell` is the command,
/// not its JSON encoding); anything else is pretty-printed at one-space
/// indent, matching the TS `JSON.stringify(parsed, null, 1)` exactly rather
/// than `serde_json`'s two-space default. Malformed JSON is shown verbatim -
/// same "never throw" posture as the rest of this pane. **Every return path
/// goes through `sanitize_for_card`** (§5.5): this is the only place a
/// bot-chosen path or other tool argument reaches the `<pre>`, and it does
/// so with no filesystem or network call of any kind - the card shows the
/// typed value, sanitised, and nothing else (§5.4's divergence check, not
/// this function, is what makes the typed value trustworthy at read time,
/// and that check has no seam here - it runs later, inside the read).
fn pretty(args: &str) -> String {
    let Ok(parsed) = serde_json::from_str::<serde_json::Value>(args) else {
        return sanitize_for_card(args);
    };
    if let serde_json::Value::Object(ref map) = parsed
        && map.len() == 1
        && let Some(s) = map.values().next().and_then(|v| v.as_str())
    {
        return sanitize_for_card(s);
    }
    let mut buf = Vec::new();
    let formatter = serde_json::ser::PrettyFormatter::with_indent(b" ");
    let mut ser = serde_json::Serializer::with_formatter(&mut buf, formatter);
    let rendered = match serde::Serialize::serialize(&parsed, &mut ser) {
        Ok(()) => String::from_utf8(buf).unwrap_or_else(|_| args.to_string()),
        Err(_) => args.to_string(),
    };
    sanitize_for_card(&rendered)
}

#[component]
pub fn Approvals() -> Element {
    let items = use_signal(Vec::<PendingApproval>::new);

    use_effect(move || {
        crate::transport::spawn_task(reload(items));
    });

    // 🔴 `crate::transport::spawn_task`, not `dioxus::prelude::spawn`: this
    // runs from `events.rs`'s bare `spawn_task(run())` loop, which Dioxus
    // never considers a "current scope" - see `working_bar.rs`'s `reload`
    // for the exact failure this avoids (a silent wasm abort on the first
    // "approvals" change), and `transport/mod.rs`'s doc on `spawn_task` for
    // why native needs a different escape hatch than wasm's `spawn_local`.
    let _events = use_signal(move || {
        subscribe_events(move |kind| {
            if kind == ChangeKind::Approvals {
                crate::transport::spawn_task(reload(items));
            }
        })
    });

    let list = items.read().clone();
    if list.is_empty() {
        return rsx! {};
    }
    let total = list.len();
    let groups = group_approvals(list);

    rsx! {
        section { class: "approvals",
            h3 { "Needs you " i { "{total}" } }
            for group in groups {
                ApprovalCard {
                    key: "{group.head.id}",
                    bot_name: group.head.bot_name.clone(),
                    tool_name: group.head.tool_name.clone(),
                    tool_args: group.head.tool_args.clone(),
                    trigger: group.head.trigger.clone(),
                    judge_verdict: group.head.judge_verdict.clone(),
                    judge_reason: group.head.judge_reason.clone(),
                    group_ids: group.ids.clone(),
                    count: group.count,
                    items,
                }
            }
        }
    }
}

#[component]
fn ApprovalCard(
    bot_name: String,
    tool_name: String,
    tool_args: String,
    trigger: Option<String>,
    judge_verdict: Option<String>,
    judge_reason: Option<String>,
    group_ids: Vec<String>,
    count: usize,
    items: Signal<Vec<PendingApproval>>,
) -> Element {
    let busy = use_signal(|| false);
    let mut typed = use_signal(String::new);

    let question = if is_answerable(&tool_name) {
        let parsed = parse_ask_josh(&tool_args);
        if parsed.question.is_empty() {
            None
        } else {
            Some(parsed)
        }
    } else {
        None
    };
    let card_class = if question.is_some() {
        "approval is-question"
    } else {
        "approval"
    };
    let cause = cause_of(trigger.as_deref());
    let judge = judge_line(judge_verdict.as_deref(), judge_reason.as_deref());

    rsx! {
        article { class: "{card_class}",
            div { class: "approval-top",
                b { "{bot_name}" }
                if question.is_none() {
                    code { class: "approval-tool", "{tool_name}" }
                }
                if count > 1 {
                    span { class: "approval-count", "×{count}" }
                }
                span { class: "approval-cause", "{cause}" }
            }
            if let Some(q) = question.clone() {
                p { class: "approval-question", "{sanitize_for_card(&q.question)}" }
            } else {
                pre { class: "mono approval-args", "{pretty(&tool_args)}" }
            }
            if let Some((badge_class, text)) = judge {
                p { class: "approval-judge",
                    span { class: "judge-badge judge-{badge_class}", "{badge_class}" }
                    " {text}"
                }
            }
            if let Some(q) = question {
                div { class: "approval-answer",
                    if !q.options.is_empty() {
                        div { class: "approval-options",
                            for option in q.options.clone() {
                                button {
                                    key: "{option}",
                                    class: "ok",
                                    disabled: *busy.read(),
                                    onclick: {
                                        let ids = group_ids.clone();
                                        let option = option.clone();
                                        move |_| {
                                            fire_decide(items, busy, ids.clone(), true, Some(option.clone()), None);
                                        }
                                    },
                                    "{sanitize_for_card(&option)}"
                                }
                            }
                        }
                    }
                    form {
                        class: "approval-typed",
                        onsubmit: {
                            let ids = group_ids.clone();
                            move |evt: FormEvent| {
                                evt.prevent_default();
                                let text = typed.read().trim().to_string();
                                if text.is_empty() {
                                    return;
                                }
                                typed.set(String::new());
                                fire_decide(items, busy, ids.clone(), true, Some(text), None);
                            }
                        },
                        input {
                            value: "{typed}",
                            disabled: *busy.read(),
                            placeholder: if q.options.is_empty() { "your answer" } else { "or say something else" },
                            oninput: move |evt| typed.set(evt.value()),
                            "aria-label": "Your answer",
                        }
                        button {
                            r#type: "submit",
                            disabled: *busy.read() || typed.read().trim().is_empty(),
                            "Send"
                        }
                    }
                    // Dismissing a question is not rejecting a command; it
                    // tells the bot to carry on without an answer.
                    button {
                        class: "approval-dismiss",
                        disabled: *busy.read(),
                        onclick: {
                            let ids = group_ids.clone();
                            move |_| fire_decide(items, busy, ids.clone(), false, None, None)
                        },
                        "Skip this"
                    }
                }
            } else {
                div { class: "approval-acts",
                    button {
                        class: "ok",
                        disabled: *busy.read(),
                        onclick: {
                            let ids = group_ids.clone();
                            move |_| fire_decide(items, busy, ids.clone(), true, None, None)
                        },
                        if count > 1 { "Approve all {count}" } else { "Approve" }
                    }
                    button {
                        disabled: *busy.read(),
                        onclick: {
                            let ids = group_ids.clone();
                            move |_| fire_decide(items, busy, ids.clone(), false, None, None)
                        },
                        "Reject"
                    }
                    // A standing decision on the TOOL, not a one-off verdict
                    // on this call - S2-07 (blocked on S2-03) is what turns
                    // `remember` into an auto-review rule server-side.
                    button {
                        class: "approval-remember",
                        disabled: *busy.read(),
                        title: "For chats. Scheduled runs will still ask.",
                        onclick: {
                            let ids = group_ids.clone();
                            move |_| fire_decide(items, busy, ids.clone(), true, None, Some("allow"))
                        },
                        "Always allow"
                    }
                    button {
                        class: "approval-remember",
                        disabled: *busy.read(),
                        title: "For chats. Scheduled runs will still ask.",
                        onclick: {
                            let ids = group_ids.clone();
                            move |_| fire_decide(items, busy, ids.clone(), false, None, Some("deny"))
                        },
                        "Never"
                    }
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{judge_line, pretty, sanitize_for_card};

    /// design §5.5, §7 bite 6 (rendering half): a bidi override must never
    /// reach the rendered card as a live formatting character, only as a
    /// visible escape - proven by removing `sanitize_for_card`'s call sites
    /// in `pretty` and watching this go red (see ticket Results for the
    /// literal red output).
    #[test]
    fn bidi_override_never_reaches_the_rendered_card() {
        let args = "{\"path\":\"C:\\\\Users\\\\rain\\\\Documents\\\\\u{202E}gnp.yek_retuorneponom\\\\selif\\\\.txt\"}";
        let rendered = pretty(args);
        assert!(
            !rendered.contains('\u{202E}'),
            "raw RLO override must never reach the card: {rendered:?}"
        );
        assert!(
            rendered.contains("\\u{202e}"),
            "override must be visibly escaped, not silently dropped: {rendered:?}"
        );
    }

    /// Same bite, the crude form: a raw `\r`/`\n` in a tool argument must
    /// not become a real line break in the `<pre>`.
    #[test]
    fn crlf_never_reaches_the_rendered_card_as_a_real_newline() {
        let args = "{\"path\":\"notes.txt\\r\\nDELETE ALL FILES\"}";
        let rendered = pretty(args);
        assert!(
            !rendered.contains('\r'),
            "raw CR must never reach the card: {rendered:?}"
        );
        assert!(
            !rendered.contains('\n'),
            "raw LF must never reach the card: {rendered:?}"
        );
        assert!(rendered.contains("\\u{d}"));
        assert!(rendered.contains("\\u{a}"));
    }

    /// The sanitiser targets exactly the two character classes design §5.5
    /// names - not "non-ASCII" in general. An accented or CJK path is
    /// legitimate content the bidi-fix must not mangle.
    #[test]
    fn ordinary_non_ascii_path_renders_unchanged() {
        let args = "{\"path\":\"C:\\\\Users\\\\Jos\u{e9}\\\\\u{7b14}\u{8bb0}.txt\"}";
        let rendered = pretty(args);
        assert_eq!(rendered, "C:\\Users\\Jos\u{e9}\\\u{7b14}\u{8bb0}.txt");
    }

    /// The malformed-JSON fallback branch (`pretty` returns the raw string
    /// verbatim when parsing fails) must still be sanitised - it is a
    /// second return path, easy to add a sanitiser to only the happy path.
    #[test]
    fn malformed_json_fallback_is_still_sanitised() {
        let rendered = pretty("not json \u{202E}here");
        assert!(!rendered.contains('\u{202E}'));
        assert!(rendered.contains("\\u{202e}"));
    }

    /// The multi-key pretty-printed branch must also be sanitised - it is a
    /// third return path, distinct from the single-key bare-string branch.
    #[test]
    fn multi_key_pretty_printed_branch_is_still_sanitised() {
        let args = "{\"path\":\"\u{202E}x\",\"recursive\":true}";
        let rendered = pretty(args);
        assert!(!rendered.contains('\u{202E}'));
        assert!(rendered.contains("\\u{202e}"));
    }

    #[test]
    fn sanitize_for_card_escapes_every_c0_and_bidi_char_directly() {
        let input = "a\u{0}\u{1F}\u{7F}\u{200E}\u{200F}\u{202A}\u{202B}\u{202C}\u{202D}\u{202E}\u{2066}\u{2067}\u{2068}\u{2069}b";
        let out = sanitize_for_card(input);
        assert_eq!(
            out,
            "a\\u{0}\\u{1f}\\u{7f}\\u{200e}\\u{200f}\\u{202a}\\u{202b}\\u{202c}\\u{202d}\\u{202e}\\u{2066}\\u{2067}\\u{2068}\\u{2069}b"
        );
    }

    #[test]
    fn renders_nothing_when_both_fields_are_null() {
        assert_eq!(judge_line(None, None), None);
    }

    #[test]
    fn renders_nothing_with_only_a_verdict_or_only_a_reason() {
        assert_eq!(judge_line(Some("risky"), None), None);
        assert_eq!(judge_line(None, Some("could delete data")), None);
    }

    #[test]
    fn risky_gets_the_amber_badge() {
        let (class, text) = judge_line(Some("risky"), Some("touches the repo")).unwrap();
        assert_eq!(class, "risky");
        assert_eq!(text, "Why it's asking: touches the repo");
    }

    #[test]
    fn dangerous_gets_the_red_badge() {
        let (class, _) = judge_line(Some("dangerous"), Some("could wipe the drive")).unwrap();
        assert_eq!(class, "dangerous");
    }
}
