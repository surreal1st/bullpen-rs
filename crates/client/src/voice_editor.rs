//! S12-04b: per-bot `speechSynthesis` voice picker - port of
//! `projects/bullpen-night/src/client/VoiceEditor.tsx`.

use crate::api;
use crate::types::Bot;
use crate::voice::{can_speak, list_voice_names, speak, subscribe_voices};
use dioxus::prelude::*;

const PREVIEW_LINE: &str = "This is how I sound.";

#[component]
pub fn VoiceEditor(bot: Bot, on_saved: EventHandler<Bot>) -> Element {
    let mut voices = use_signal(list_voice_names);
    let mut value = use_signal(|| bot.voice.clone());
    let mut saving = use_signal(|| false);
    let mut error = use_signal(|| None::<String>);

    use_effect({
        let bot_voice = bot.voice.clone();
        move || {
            value.set(bot_voice.clone());
        }
    });

    use_effect(move || {
        subscribe_voices(move || voices.set(list_voice_names()));
    });

    if !can_speak() {
        return rsx! {
            p { class: "muted voice-editor-unavailable",
                "This browser cannot read text aloud, so there is no voice to choose."
            }
        };
    }

    let bot_id = bot.id.clone();
    let preview_voice = value.read().clone();

    rsx! {
        div { class: "voice-editor",
            span { class: "voice-editor-label muted", "Voice" }
            div { class: "voice-editor-row",
                select {
                    class: "pane-perms-btn voice-editor-select",
                    "aria-label": "Voice",
                    disabled: *saving.read(),
                    value: "{value.read().clone().unwrap_or_default()}",
                    onchange: {
                        let bot_id = bot_id.clone();
                        move |evt| {
                            if *saving.read() {
                                return;
                            }
                            let picked = evt.value();
                            let next = if picked.is_empty() {
                                None
                            } else {
                                Some(picked)
                            };
                            saving.set(true);
                            error.set(None);
                            let id = bot_id.clone();
                            spawn(async move {
                                let body = match &next {
                                    Some(name) => serde_json::json!({ "voice": name }),
                                    None => serde_json::json!({ "voice": null }),
                                };
                                let result = api::patch_bot(&id, body).await;
                                saving.set(false);
                                match result {
                                    Ok(updated) => {
                                        value.set(updated.voice.clone());
                                        on_saved.call(updated);
                                    }
                                    Err(err) => error.set(Some(err)),
                                }
                            });
                        }
                    },
                    option { value: "", "Device default" }
                    for name in voices.read().iter().cloned() {
                        option { key: "{name}", value: "{name}", "{name}" }
                    }
                }
                button {
                    class: "pane-perms-btn",
                    r#type: "button",
                    disabled: *saving.read(),
                    onclick: move |_| {
                        speak(
                            PREVIEW_LINE,
                            None,
                            preview_voice.as_deref(),
                        );
                    },
                    "Preview"
                }
            }
            if voices.read().is_empty() {
                small {
                    "No voices reported yet — some browsers only list them a moment after the page loads."
                }
            }
            if let Some(err) = error.read().clone() {
                p { class: "composer-error", role: "alert", "{err}" }
            }
        }
    }
}
