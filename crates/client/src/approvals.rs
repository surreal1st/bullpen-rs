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

/// Renders a tool call's arguments for the card. A single-key object shows
/// its one value bare (what matters when approving `shell` is the command,
/// not its JSON encoding); anything else is pretty-printed at one-space
/// indent, matching the TS `JSON.stringify(parsed, null, 1)` exactly rather
/// than `serde_json`'s two-space default. Malformed JSON is shown verbatim -
/// same "never throw" posture as the rest of this pane.
fn pretty(args: &str) -> String {
    let Ok(parsed) = serde_json::from_str::<serde_json::Value>(args) else {
        return args.to_string();
    };
    if let serde_json::Value::Object(ref map) = parsed
        && map.len() == 1
        && let Some(s) = map.values().next().and_then(|v| v.as_str())
    {
        return s.to_string();
    }
    let mut buf = Vec::new();
    let formatter = serde_json::ser::PrettyFormatter::with_indent(b" ");
    let mut ser = serde_json::Serializer::with_formatter(&mut buf, formatter);
    match serde::Serialize::serialize(&parsed, &mut ser) {
        Ok(()) => String::from_utf8(buf).unwrap_or_else(|_| args.to_string()),
        Err(_) => args.to_string(),
    }
}

#[component]
pub fn Approvals() -> Element {
    let items = use_signal(Vec::<PendingApproval>::new);

    use_effect(move || {
        wasm_bindgen_futures::spawn_local(reload(items));
    });

    // 🔴 `wasm_bindgen_futures::spawn_local`, not `dioxus::prelude::spawn`:
    // this runs from `events.rs`'s bare `spawn_local(run())` loop, which
    // Dioxus never considers a "current scope" - see `working_bar.rs`'s
    // `reload` for the exact failure this avoids (a silent wasm abort on
    // the first "approvals" change).
    let _events = use_signal(move || {
        subscribe_events(move |kind| {
            if kind == ChangeKind::Approvals {
                wasm_bindgen_futures::spawn_local(reload(items));
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
                p { class: "approval-question", "{q.question}" }
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
                                    "{option}"
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
    use super::judge_line;

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
