//! F7b-01: the "New bot" affordance beside `rail.rs`'s `.rail-new-room` +.
//! Ported from `projects/bullpen-night/src/client/BotEditor.tsx:351-410`'s
//! `NewBotForm` - Name and "What it is for" only; instructions/model are set
//! later from the bot's own editor, same as the TS original's own note ("It
//! starts on the platform default model. Give it instructions and a model
//! next.").
//!
//! Reuses `room_picker.rs`'s `.modal-scrim`/`.modal`/`.modal-head`/
//! `.modal-x`/`.modal-body`/`.room-picker-*` shell rather than inventing a
//! third modal pattern (`rail.css`'s own doc names `RoomPicker` and
//! `GoalsModal`/`RoutinesModal` as the two existing shapes; this is closer
//! to `RoomPicker`'s plain create form than to the goals/routines list+edit
//! shape).

use crate::api;
use crate::types::Bot;
use dioxus::prelude::*;

#[component]
pub fn NewBotModal(on_close: EventHandler<()>, on_created: EventHandler<Bot>) -> Element {
    let mut name = use_signal(String::new);
    let mut purpose = use_signal(String::new);
    let mut saving = use_signal(|| false);
    let mut error = use_signal(|| None::<String>);

    let create_disabled = name.read().trim().is_empty() || *saving.read();

    let submit = move |_| {
        let trimmed = name.read().trim().to_string();
        if trimmed.is_empty() {
            error.set(Some("name is required".to_string()));
            return;
        }
        let purpose_value = purpose.read().clone();
        saving.set(true);
        error.set(None);
        spawn(async move {
            let result = api::create_bot(&trimmed, &purpose_value).await;
            saving.set(false);
            match result {
                Ok(bot) => on_created.call(bot),
                Err(err) => error.set(Some(err)),
            }
        });
    };

    rsx! {
        div {
            class: "modal-scrim",
            role: "presentation",
            onclick: move |_| on_close.call(()),
            div {
                class: "modal room-picker",
                role: "dialog",
                "aria-modal": "true",
                "aria-label": "New bot",
                onclick: move |evt| evt.stop_propagation(),
                div { class: "modal-head",
                    h2 { "New bot" }
                    button {
                        class: "modal-x",
                        "aria-label": "Close",
                        onclick: move |_| on_close.call(()),
                        "×"
                    }
                }
                div { class: "modal-body",
                    label { class: "room-picker-title",
                        "Name"
                        input {
                            value: "{name}",
                            oninput: move |evt| name.set(evt.value()),
                            placeholder: "Trinity",
                            autofocus: true,
                            maxlength: 120,
                        }
                    }
                    label { class: "room-picker-title",
                        "What it is for"
                        input {
                            value: "{purpose}",
                            oninput: move |evt| purpose.set(evt.value()),
                            placeholder: "Watches the error log",
                            maxlength: 200,
                        }
                    }
                    if let Some(msg) = error.read().clone() {
                        p { class: "notice-inline", "{msg}" }
                    }
                    div { class: "room-picker-actions",
                        button { class: "thread-new", onclick: move |_| on_close.call(()), "Cancel" }
                        button {
                            class: "thread-new room-picker-create",
                            disabled: create_disabled,
                            onclick: submit,
                            "Create"
                        }
                    }
                }
            }
        }
    }
}
