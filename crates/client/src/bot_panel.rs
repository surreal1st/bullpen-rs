//! Grok-style right column: live VM screen + routines list. Mounted beside
//! the chat pane in `app.rs`, not inside `ChatPane` — the center column is
//! messages-only.

use crate::api;
use crate::types::Routine;
use crate::vm_card::VmCard;
use dioxus::prelude::*;

#[component]
pub fn BotPanel(
    bot_id: String,
    bot_name: String,
    #[props(default)] visible: bool,
    #[props(default)] refresh_key: u32,
    on_manage_routines: EventHandler<()>,
) -> Element {
    let mut routines = use_signal(|| None::<Vec<Routine>>);
    let load_id = bot_id.clone();

    use_effect(move || {
        let _ = refresh_key;
        if !visible {
            routines.set(None);
            return;
        }
        let bot_id = load_id.clone();
        spawn(async move {
            match api::fetch_routines(&bot_id).await {
                Ok(list) => routines.set(Some(list)),
                Err(_) => routines.set(Some(Vec::new())),
            }
        });
    });

    if !visible {
        return rsx! {
            aside { class: "bot-panel bot-panel-empty",
                p { class: "bot-panel-empty-line", "Pick a bot to see its screen and routines." }
            }
        };
    }

    let list = routines.read().clone();

    rsx! {
        aside { class: "bot-panel",
            div { class: "bot-panel-scroll",
                VmCard { bot_id: bot_id.clone(), bot_name: bot_name.clone() }
                section { class: "panel-sect bot-panel-routines",
                    div { class: "bot-panel-routines-head",
                        h2 { class: "bot-panel-routines-title", "Routines" }
                        button {
                            class: "bot-panel-routines-manage",
                            type: "button",
                            onclick: move |_| on_manage_routines.call(()),
                            title: "Add or edit routines",
                            "+"
                        }
                    }
                    if list.is_none() {
                        p { class: "muted bot-panel-routines-loading", "Loading routines…" }
                    } else if list.as_ref().is_some_and(|l| l.is_empty()) {
                        p { class: "muted bot-panel-routines-empty", "Nothing scheduled yet." }
                    } else if let Some(list) = list.as_ref() {
                        div { class: "bot-panel-routine-list",
                            for routine in list.iter().cloned() {
                                article {
                                    key: "{routine.id}",
                                    class: if routine.active { "bot-panel-routine is-on" } else { "bot-panel-routine" },
                                    div { class: "bot-panel-routine-top",
                                        span {
                                            class: if routine.active { "routine-dot is-on" } else { "routine-dot" },
                                            "aria-hidden": "true"
                                        }
                                        b { "{routine.name}" }
                                    }
                                    p { class: "bot-panel-routine-when", "{routine.schedule_text}" }
                                    if !routine.prompt.is_empty() {
                                        p { class: "bot-panel-routine-prompt", "{routine.prompt}" }
                                    }
                                }
                            }
                        }
                    }
                    button {
                        class: "bot-panel-routines-link",
                        type: "button",
                        onclick: move |_| on_manage_routines.call(()),
                        "Manage routines…"
                    }
                }
            }
        }
    }
}
