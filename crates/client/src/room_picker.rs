//! Port of `projects/bullpen-night/src/client/GroupChatPicker.tsx`: pick
//! 2..N bots, name the group, `POST /api/rooms` to create or
//! `PATCH /api/rooms/:id` to edit membership. The ticket bumps the
//! original's cap of 5 to **6**.

use crate::api;
use crate::avatar::Avatar;
use crate::types::{Bot, RoomSummary};
use dioxus::prelude::*;

/// A group chat tops out here - the server's own `check_roster` enforces
/// the same number (`crates/store/src/rooms.rs`), so a client bug here is a
/// worse UX, never a way around the limit.
pub const MAX_ROOM_MEMBERS: usize = 6;

/// F8 (S1-F-11): toggles `id` in an ORDERED pick list - removes it if
/// present, appends it if not (unless already at `cap`). A free function so
/// the ordering claim (first picked stays first, `member_ids[0]` is what the
/// server makes the room's owner - `crates/store/src/rooms.rs`'s `to_room`)
/// is provable by a plain `cargo test`, not just by eye in the browser.
fn toggle_picked(list: &mut Vec<String>, id: &str, cap: usize) {
    if let Some(pos) = list.iter().position(|existing| existing == id) {
        list.remove(pos);
    } else if list.len() < cap {
        list.push(id.to_string());
    }
}

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
    // F8 (S1-F-11): an ORDERED `Vec`, not a `HashSet` - `member_ids[0]` is
    // what the server makes the room's owner and the first speaker of every
    // round (`crates/store/src/rooms.rs`'s `to_room`), so a nondeterministic
    // iteration order here used to hand a random member the owner slot every
    // time "Growth" (say) was recreated with the same three bots, and
    // rewrote `conversations.bot_id` to a random pick on every membership
    // edit. Editing an existing room starts from its own `member_ids`, which
    // is already in "Josh picked them" order - the owner it already has
    // stays first unless he unchecks it.
    let initial_picked: Vec<String> = match &mode {
        PickerMode::Edit(room) => room.member_ids.clone(),
        PickerMode::Create => Vec::new(),
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
            let checked = picked.read().iter().any(|id| id == &b.id);
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
        // Sent in pick order - `picked` is already that order (see its doc
        // above), so this is a plain clone, not a re-sort.
        let member_ids: Vec<String> = picked.read().clone();
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
                                        toggle_picked(&mut picked.write(), &checkbox_id, MAX_ROOM_MEMBERS);
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

#[cfg(test)]
mod tests {
    use super::*;

    /// F8's bite check: change `push` to `insert(0, ...)` (or collect through
    /// a `HashSet` again) and this goes red - whichever bot was picked FIRST
    /// must stay first, since that is the id the server makes the room's
    /// owner.
    #[test]
    fn toggle_picked_keeps_first_picked_first() {
        let mut list = Vec::new();
        toggle_picked(&mut list, "arthur", 6);
        toggle_picked(&mut list, "grok", 6);
        toggle_picked(&mut list, "elly", 6);
        assert_eq!(list, vec!["arthur", "grok", "elly"]);
    }

    #[test]
    fn toggle_picked_removes_without_disturbing_order() {
        let mut list = vec!["arthur".to_string(), "grok".to_string(), "elly".to_string()];
        toggle_picked(&mut list, "grok", 6);
        assert_eq!(list, vec!["arthur", "elly"]);
    }

    #[test]
    fn toggle_picked_refuses_past_the_cap() {
        let mut list = vec!["a".to_string(), "b".to_string()];
        toggle_picked(&mut list, "c", 2);
        assert_eq!(list, vec!["a", "b"]);
    }
}
