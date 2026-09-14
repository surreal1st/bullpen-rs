//! Port of `projects/bullpen-night/src/client/Roster.tsx:600-720`
//! (`BotRow`/`GroupRow` markup) for the left rail. Group chats (`GroupRow`)
//! and the row's own context menu / rename-in-place are out of scope for
//! S0-05 (no `/api/rooms` yet, no rename wiring); this renders sections as
//! headers and bots as rows with the face, name, purpose, unread dot, and
//! the `is-busy` animation.

use crate::avatar::Avatar;
use crate::types::{Bot, Section};
use dioxus::prelude::*;

#[component]
pub fn Rail(sections: Vec<Section>, bots: Vec<Bot>) -> Element {
    let section_ids: Vec<String> = sections.iter().map(|s| s.id.clone()).collect();

    // Group visible bots by section, in section order; anything that does
    // not match a real section falls into an implicit trailing bucket, the
    // same "Unassigned" shape Roster.tsx gives the synthetic group.
    let mut groups: Vec<(Option<String>, Vec<Bot>)> = sections
        .iter()
        .map(|s| (Some(s.name.clone()), Vec::new()))
        .collect();
    let mut unassigned: Vec<Bot> = Vec::new();

    for bot in bots.iter().filter(|b| !b.hidden) {
        match bot
            .section_id
            .as_deref()
            .and_then(|id| section_ids.iter().position(|s| s == id))
        {
            Some(i) => groups[i].1.push(bot.clone()),
            None => unassigned.push(bot.clone()),
        }
    }
    if !unassigned.is_empty() {
        groups.push((
            if sections.is_empty() {
                None
            } else {
                Some("Unassigned".to_string())
            },
            unassigned,
        ));
    }

    rsx! {
        div { class: "rail",
            div { class: "roster",
                for (label, group_bots) in groups {
                    if let Some(name) = label {
                        div {
                            style: "text-transform: uppercase; font-size: 0.68rem; font-weight: 700; letter-spacing: 0.11em; padding: 0.7rem 0.35rem 0.3rem; color: var(--faint);",
                            "{name}"
                        }
                    }
                    for bot in group_bots {
                        BotRow { key: "{bot.id}", bot, section_ids: section_ids.clone() }
                    }
                }
            }
        }
    }
}

#[component]
pub fn BotRow(bot: Bot, section_ids: Vec<String>) -> Element {
    let class = if bot.unread > 0 {
        "bot is-unread"
    } else {
        "bot"
    };
    let preview = bot
        .preview
        .clone()
        .unwrap_or_else(|| "No messages yet".to_string());

    rsx! {
        button { class: "{class}",
            Avatar {
                id: bot.id.clone(),
                name: bot.name.clone(),
                section_id: bot.section_id.clone(),
                section_ids,
                busy: bot.busy,
                avatar: bot.avatar.clone(),
                shape: bot.shape.clone(),
            }
            span { class: "bot-main",
                span { class: "bot-line",
                    span { class: "bot-name",
                        "{bot.name}"
                        if !bot.purpose.is_empty() {
                            span { class: "bot-role", " · {bot.purpose}" }
                        }
                    }
                    span { class: "bot-when",
                        if bot.unread > 0 {
                            i { class: "dot" }
                        }
                    }
                }
                span { class: "bot-preview", "{preview}" }
            }
        }
    }
}
