//! EDIT-01: editing an existing bot's name/purpose/instructions - the hole
//! `new_bot.rs`'s own doc names ("It starts on the platform default model.
//! Give it instructions and a model next." - there was no next). Reuses
//! `new_bot.rs`'s modal shell (`.modal-scrim`/`.modal`/`.modal-head`/
//! `.modal-x`/`.modal-body`/`.room-picker-*`) rather than inventing a
//! fourth modal shape, and IMPORT-02/DUP-01's own `.room-picker-textarea`
//! class for the Instructions field - EDITABLE here (no `readonly`), unlike
//! the read-only preview textarea `new_bot.rs` added that class for.
//!
//! `voice` stays out on purpose: it is S11's device-voice field, and
//! nothing in this port writes it anywhere except `store::duplicate_bot`
//! (which only ever carries an existing value ACROSS to a copy, never sets
//! one from scratch) - adding a writer here for a field with no feature
//! behind it yet would be a column edit pretending to be a feature.
//!
//! On success this hands the updated `Bot` back through `on_saved`.
//! `thread.rs`'s own call site both seeds its `local_bot` from it directly
//! (the same "seed a local copy from the server's response" posture every
//! other header control there already takes - pin/hide/move/avatar/shape/
//! `ModelChip`) AND fires `on_rail_changed` (a name change is visible on
//! the RAIL too, which `local_bot` alone does not reach - the rail's own
//! bot list comes from `app.rs`'s separate roster fetch). Not
//! `on_duplicated`: that prop exists specifically because duplicating
//! creates a NEW bot Josh needs navigated to; editing changes the bot
//! already open, so there is nothing new to select.

use crate::api;
use crate::types::Bot;
use dioxus::prelude::*;

#[component]
pub fn EditBotModal(bot: Bot, on_close: EventHandler<()>, on_saved: EventHandler<Bot>) -> Element {
    let mut name = use_signal(|| bot.name.clone());
    let mut purpose = use_signal(|| bot.purpose.clone());
    let mut instructions = use_signal(|| bot.instructions.clone());
    let mut saving = use_signal(|| false);
    let mut error = use_signal(|| None::<String>);

    // Same guard the server itself applies (`routes/bots.rs::patch_bot`'s
    // own doc on `name`): a blank/whitespace-only name means "no change",
    // not something to send. Disabling Save on it here just saves Josh a
    // round trip for a PATCH that would silently keep the old name anyway.
    let save_disabled = name.read().trim().is_empty() || *saving.read();

    let bot_id = bot.id.clone();
    let submit = move |_| {
        if *saving.read() {
            return;
        }
        let trimmed_name = name.read().trim().to_string();
        if trimmed_name.is_empty() {
            error.set(Some("name is required".to_string()));
            return;
        }
        let purpose_value = purpose.read().clone();
        let instructions_value = instructions.read().clone();
        let id = bot_id.clone();
        saving.set(true);
        error.set(None);
        spawn(async move {
            let result =
                api::update_bot_identity(&id, &trimmed_name, &purpose_value, &instructions_value)
                    .await;
            saving.set(false);
            match result {
                Ok(updated) => on_saved.call(updated),
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
                "aria-label": "Edit bot",
                onclick: move |evt| evt.stop_propagation(),
                div { class: "modal-head",
                    h2 { "Edit bot" }
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
                    label { class: "room-picker-title",
                        "Instructions"
                        textarea {
                            class: "room-picker-textarea",
                            value: "{instructions}",
                            "aria-label": "Instructions",
                            oninput: move |evt| instructions.set(evt.value()),
                        }
                    }
                    if let Some(msg) = error.read().clone() {
                        p { class: "notice-inline", "{msg}" }
                    }
                    div { class: "room-picker-actions",
                        button { class: "thread-new", onclick: move |_| on_close.call(()), "Cancel" }
                        button {
                            class: "thread-new room-picker-create",
                            disabled: save_disabled,
                            onclick: submit,
                            if *saving.read() { "Saving…" } else { "Save" }
                        }
                    }
                }
            }
        }
    }
}
