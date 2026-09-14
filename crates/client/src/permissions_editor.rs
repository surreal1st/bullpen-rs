//! Port of `PermissionsEditor.tsx`: one bot's tool grid - Always/Ask me/Never
//! per tool, `GET`/`PUT /api/bots/:id/permissions`
//! (`crates/server/src/routes/permissions.rs`, already built). In the TS
//! original this lives inside `BotEditor.tsx`, which nothing in bullpen-rs
//! has built yet - S2-09b mounts it directly under the thread header
//! instead (see `thread.rs`'s doc comment on `PermissionsPanel` for why),
//! a scoped placement decision rather than a port of BotEditor's own shell.

use crate::api;
use crate::types::MadeTool;
use dioxus::prelude::*;
use std::collections::HashMap;

struct ToolInfo {
    name: &'static str,
    label: &'static str,
    blurb: &'static str,
}

const TOOLS: &[ToolInfo] = &[
    ToolInfo {
        name: "shell",
        label: "Run commands",
        blurb: "Shell access inside its own sandbox. The one that can delete something.",
    },
    ToolInfo {
        name: "fetch_url",
        label: "Fetch the web",
        blurb: "Any public site, minus the blocklist. Never anything internal.",
    },
    ToolInfo {
        name: "search_memory",
        label: "Search its memory",
        blurb: "Reads its own log. Cannot reach anything else.",
    },
    ToolInfo {
        name: "remember",
        label: "Write to its memory",
        blurb: "Saves a fact for later. Affects only this bot.",
    },
    ToolInfo {
        name: "message_bot",
        label: "Ask another bot",
        blurb: "Puts a question to a colleague on the roster and uses the answer.",
    },
    ToolInfo {
        name: "escalate",
        label: "Ask for a better model",
        blurb: "Hands a stuck conversation to the premium model. Never available on a scheduled run.",
    },
];

const CHOICES: &[(&str, &str)] = &[("allow", "Always"), ("ask", "Ask me"), ("deny", "Never")];

async fn reload(bot_id: String, mut permissions: Signal<Option<HashMap<String, String>>>) {
    if let Ok(perms) = api::fetch_permissions(&bot_id).await {
        permissions.set(Some(perms));
    }
}

/// Fires the PUT for one tool, optimistically - ported from the TS `set`:
/// the click updates the signal immediately (so a slow network never leaves
/// the button looking unpressed) and posts in the background. A plain
/// function, not a closure, so each of the 6+ buttons on the grid can hold
/// its own copy without fighting `Signal`'s `Copy` bound against a captured
/// `String`.
fn set_decision(
    bot_id: String,
    mut permissions: Signal<Option<HashMap<String, String>>>,
    tool: String,
    decision: String,
) {
    let Some(mut next) = permissions.read().clone() else {
        return;
    };
    next.insert(tool, decision);
    permissions.set(Some(next.clone()));
    spawn(async move {
        let _ = api::put_permissions(&bot_id, &next).await;
    });
}

#[component]
pub fn PermissionsEditor(bot_id: String) -> Element {
    let permissions = use_signal(|| None::<HashMap<String, String>>);
    let mut made = use_signal(Vec::<MadeTool>::new);

    let load_bot_id = bot_id.clone();
    use_effect(move || {
        let bot_id = load_bot_id.clone();
        spawn(reload(bot_id, permissions));
    });

    use_effect(move || {
        spawn(async move {
            made.set(api::fetch_bot_tools().await);
        });
    });

    let Some(perms) = permissions.read().clone() else {
        return rsx! { p { class: "muted", "Loading permissions…" } };
    };

    // One combined list - a bot-written tool is not a different KIND of
    // permission (W5's own framing, carried over from the TS doc comment):
    // it defaults to "ask" like anything unrecognised, in the same grid.
    let rows: Vec<(String, String, String, String)> = TOOLS
        .iter()
        .map(|t| {
            (
                t.name.to_string(),
                t.label.to_string(),
                t.blurb.to_string(),
                perms
                    .get(t.name)
                    .cloned()
                    .unwrap_or_else(|| "ask".to_string()),
            )
        })
        .chain(made.read().iter().map(|t| {
            (
                t.name.clone(),
                t.name.clone(),
                format!("{} Written by {}.", t.description, t.bot_name),
                perms
                    .get(&t.name)
                    .cloned()
                    .unwrap_or_else(|| "ask".to_string()),
            )
        }))
        .collect();

    rsx! {
        div { class: "perms",
            for (name , label , blurb , current) in rows {
                div { key: "{name}", class: "perm-row",
                    div { class: "perm-what",
                        span { class: "perm-label", "{label}" }
                        span { class: "perm-blurb", "{blurb}" }
                    }
                    div { class: "perm-choices", role: "group", "aria-label": "{label}",
                        for (value , choice_label) in CHOICES.iter() {
                            button {
                                key: "{value}",
                                class: if current == *value { "is-on" } else { "" },
                                onclick: {
                                    let bot_id = bot_id.clone();
                                    let name = name.clone();
                                    let value = value.to_string();
                                    move |_| set_decision(bot_id.clone(), permissions, name.clone(), value.clone())
                                },
                                "{choice_label}"
                            }
                        }
                    }
                }
            }
        }
    }
}
