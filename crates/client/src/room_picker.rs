//! Port of `projects/bullpen-night/src/client/GroupChatPicker.tsx`: pick
//! 2..N bots, name the group, `POST /api/rooms` to create or
//! `PATCH /api/rooms/:id` to edit membership. The ticket bumps the
//! original's cap of 5 to **6**.

use crate::api;
use crate::avatar::Avatar;
use crate::types::{Bot, RoomSummary};
use dioxus::prelude::*;
use std::collections::HashSet;

/// A group chat tops out here - the server's own `check_roster` enforces
/// the same number (`crates/store/src/rooms.rs`), so a client bug here is a
/// worse UX, never a way around the limit.
pub const MAX_ROOM_MEMBERS: usize = 6;

#[derive(Clone, PartialEq)]
pub enum PickerMode {
    Create,
    /// Editing an existing group's title/membership - `room_picker.rs`'s
    /// "PATCH for editing members" half of the ticket's Target.
    Edit(RoomSummary),
}

#[component]
pub fn RoomPicker(
    bots: Vec<Bot>,
    section_ids: Vec<String>,
    mode: PickerMode,
    on_close: EventHandler<()>,
    on_saved: EventHandler<RoomSummary>,
) -> Element {
    let initial_title = match &mode {
        PickerMode::Edit(room) => room.title.clone(),
        PickerMode::Create => String::new(),
    };
    let initial_picked: HashSet<String> = match &mode {
        PickerMode::Edit(room) => room.member_ids.iter().cloned().collect(),
        PickerMode::Create => HashSet::new(),
    };
    let edit_id = match &mode {
        PickerMode::Edit(room) => Some(room.id.clone()),
        PickerMode::Create => None,
    };
    let heading = match &mode {
        PickerMode::Edit(_) => "Add or remove bots",
        PickerMode::Create => "Create group chat",
    };
    let save_label = match &mode {
        PickerMode::Edit(_) => "Save",
        PickerMode::Create => "Create",
    };

    let mut title = use_signal(|| initial_title);
    let mut picked = use_signal(|| initial_picked);
    let mut saving = use_signal(|| false);
    let mut error = use_signal(|| None::<String>);

    // Precomputed per-row facts, same reason `rail.rs`/`thread.rs` compute
    // pairs ahead of their own `rsx!` loops: whether a row is checked or
    // capped-out is a plain fact about a bot and the current picked set,
    // not something to work out inside the template.
    let at_cap = picked.read().len() >= MAX_ROOM_MEMBERS;
    // The trailing `String` is a clone of `bot.id` kept as its own tuple
    // slot - not a field projected off `bot` - so `onchange` below can
    // `move` it into the closure without touching `bot`, which the same
    // row still needs for its `Avatar`/name after.
    let checklist: Vec<(Bot, bool, bool, String)> = bots
        .iter()
        .filter(|b| !b.hidden)
        .map(|b| {
            let checked = picked.read().contains(&b.id);
            (b.clone(), checked, !checked && at_cap, b.id.clone())
        })
        .collect();

    let submit = move |_| {
        if picked.read().len() < 2 {
            error.set(Some("Pick at least two bots.".to_string()));
            return;
        }
        let edit_id = edit_id.clone();
        let title_value = title.read().clone();
        let member_ids: Vec<String> = picked.read().iter().cloned().collect();
        saving.set(true);
        error.set(None);
        spawn(async move {
            let result = match &edit_id {
                Some(id) => api::update_room(id, &title_value, &member_ids).await,
                None => api::create_room(&title_value, &member_ids).await,
            };
            saving.set(false);
            match result {
                Ok(room) => on_saved.call(room),
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
                "aria-label": "{heading}",
                onclick: move |evt| evt.stop_propagation(),
                div { class: "modal-head",
                    h2 { "{heading}" }
                    button {
                        class: "modal-x",
                        "aria-label": "Close",
                        onclick: move |_| on_close.call(()),
                        "×"
                    }
                }
                div { class: "modal-body",
                    label { class: "room-picker-title",
                        "Title"
                        input {
                            value: "{title}",
                            oninput: move |evt| title.set(evt.value()),
                            placeholder: "Name this group chat",
                            maxlength: 120,
                        }
                    }
                    p { class: "room-picker-hint",
                        "Pick up to {MAX_ROOM_MEMBERS} bots. Everyone sees everyone's replies."
                    }
                    div { class: "room-picker-list",
                        for (bot , checked , disabled , checkbox_id) in checklist {
                            label { class: "room-picker-item", key: "{bot.id}",
                                input {
                                    r#type: "checkbox",
                                    checked,
                                    disabled,
                                    onchange: move |_| {
                                        let mut set = picked.write();
                                        if set.contains(&checkbox_id) {
                                            set.remove(&checkbox_id);
                                        } else if set.len() < MAX_ROOM_MEMBERS {
                                            set.insert(checkbox_id.clone());
                                        }
                                    },
                                }
                                Avatar {
                                    id: bot.id.clone(),
                                    name: bot.name.clone(),
                                    section_id: bot.section_id.clone(),
                                    section_ids: section_ids.clone(),
                                    avatar: bot.avatar.clone(),
                                    shape: bot.shape.clone(),
                                    size: 22.0,
                                }
                                "{bot.name}"
                            }
                        }
                    }
                    if let Some(msg) = error.read().clone() {
                        p { class: "notice-inline", "{msg}" }
                    }
                    div { class: "room-picker-actions",
                        button { class: "thread-new", onclick: move |_| on_close.call(()), "Cancel" }
                        button {
                            class: "thread-new room-picker-create",
                            disabled: picked.read().len() < 2 || *saving.read(),
                            onclick: submit,
                            "{save_label}"
                        }
                    }
                }
            }
        }
    }
}
