//! W6: "While you were away." Port of `projects/bullpen-night/src/client/AwayCard.tsx`.

use crate::types::{AwayBotRow, AwayPayload};
use dioxus::prelude::*;

fn bot_detail(bot: &AwayBotRow) -> String {
    let mut parts: Vec<String> = Vec::new();
    if bot.unread > 0 {
        parts.push(format!("{} unread", bot.unread));
    }
    if bot.questions > 0 {
        let label = if bot.questions == 1 {
            "question"
        } else {
            "questions"
        };
        parts.push(format!("{} {label}", bot.questions));
    }
    if bot.approvals > 0 {
        let label = if bot.approvals == 1 {
            "approval"
        } else {
            "approvals"
        };
        parts.push(format!("{} {label}", bot.approvals));
    }
    if !bot.stopped_routines.is_empty() {
        let n = bot.stopped_routines.len();
        let label = if n == 1 { "routine" } else { "routines" };
        parts.push(format!("{n} {label} stopped"));
    }
    parts.join(" · ")
}

#[component]
pub fn AwayCard(
    away: Option<AwayPayload>,
    on_dismiss: EventHandler<()>,
    on_open_bot: EventHandler<String>,
) -> Element {
    let Some(away) = away else {
        return rsx! {};
    };
    if !away.show {
        return rsx! {};
    }

    let summary = away.summary.as_deref().unwrap_or("");
    let bots = away.bots.as_deref().unwrap_or(&[]);

    rsx! {
        div { class: "away-card",
            div { class: "away-card-head",
                span { class: "away-card-title", "While you were away" }
                button {
                    class: "away-card-dismiss",
                    r#type: "button",
                    onclick: move |_| on_dismiss.call(()),
                    "Dismiss"
                }
            }
            if !summary.is_empty() {
                p { class: "away-card-summary", "{summary}" }
            }
            if !bots.is_empty() {
                div { class: "away-card-bots",
                    for bot in bots {
                        button {
                            key: "{bot.bot_id}",
                            class: "away-card-bot",
                            r#type: "button",
                            onclick: {
                                let id = bot.bot_id.clone();
                                move |_| on_open_bot.call(id.clone())
                            },
                            span { class: "away-card-bot-name", "{bot.bot_name}" }
                            span { class: "away-card-bot-detail", "{bot_detail(bot)}" }
                        }
                    }
                }
            }
        }
    }
}
