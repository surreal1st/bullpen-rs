//! Port of `projects/bullpen-night/src/client/Roster.tsx:600-720`
//! (`BotRow`/`GroupRow` markup) for the left rail, plus `:230-250` (the
//! GROUP CHATS section) and `GroupAvatar.tsx` (the stacked-faces cluster,
//! folded into this file rather than kept as its own since S1-07b's Target
//! only names `rail.rs`, not a separate module for it). Rename-in-place is
//! still out of scope (no rename wiring).

use crate::avatar::Avatar;
use crate::types::{Bot, RoomSummary, Section};
use dioxus::prelude::*;

#[component]
pub fn Rail(
    sections: Vec<Section>,
    bots: Vec<Bot>,
    #[props(default)] rooms: Vec<RoomSummary>,
    #[props(default)] selected: Option<String>,
    #[props(default)] selected_room: Option<String>,
    on_select: EventHandler<String>,
    on_select_room: EventHandler<RoomSummary>,
    on_new_room: EventHandler<()>,
    // F7b-01: the "New bot" affordance beside "New group chat" - see
    // `new_bot.rs`'s own doc for why it opens a modal rather than growing a
    // third shape here.
    on_new_bot: EventHandler<()>,
    on_edit_room: EventHandler<RoomSummary>,
    // S2-F-08 (D2): `app.rs`'s settings gear used to float as a
    // `position: fixed` button over the composer's Send button (`shots/
    // s2-settings.png` before this fix). It lives here now, beside "New
    // group chat", so it never overlaps the thread.
    on_settings: EventHandler<()>,
) -> Element {
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

    // Whether each row is the selected bot, computed here rather than
    // inside the `rsx!` loop below (which bot the row belongs to and
    // whether it matches `selected` is a plain fact about a pair of
    // values, same reason `thread.rs` precomputes its timemark divider).
    type MarkedGroups = Vec<(Option<String>, Vec<(Bot, bool)>)>;
    let groups: MarkedGroups = groups
        .into_iter()
        .map(|(label, bots)| {
            let marked = bots
                .into_iter()
                .map(|b| {
                    let is_selected = selected.as_deref() == Some(b.id.as_str());
                    (b, is_selected)
                })
                .collect();
            (label, marked)
        })
        .collect();

    // Same "is this row the open one" precompute as `groups` below, ported
    // from Roster.tsx's `open={selectedRoomId === room.id}`.
    let rooms: Vec<(RoomSummary, bool)> = rooms
        .into_iter()
        .map(|room| {
            let is_selected = selected_room.as_deref() == Some(room.id.as_str());
            (room, is_selected)
        })
        .collect();

    rsx! {
        div { class: "rail",
            div { class: "roster",
                div { class: "rail-group-head",
                    span { class: "rail-group-title", "Group chats" }
                    div { class: "rail-group-acts",
                        button {
                            class: "rail-new-room",
                            title: "New bot",
                            "aria-label": "New bot",
                            onclick: move |_| on_new_bot.call(()),
                            "+"
                        }
                        button {
                            class: "rail-new-room",
                            title: "New group chat",
                            "aria-label": "New group chat",
                            onclick: move |_| on_new_room.call(()),
                            "+"
                        }
                        button {
                            class: "rail-settings",
                            title: "Settings",
                            "aria-label": "Settings",
                            onclick: move |_| on_settings.call(()),
                            "⚙"
                        }
                    }
                }
                for (room , is_selected) in rooms {
                    GroupRow {
                        key: "{room.id}",
                        room: room.clone(),
                        bots: bots.clone(),
                        section_ids: section_ids.clone(),
                        selected: is_selected,
                        on_select: on_select_room,
                        on_edit: on_edit_room,
                    }
                }
                for (label , group_bots) in groups {
                    if let Some(name) = label {
                        div {
                            style: "text-transform: uppercase; font-size: 0.68rem; font-weight: 700; letter-spacing: 0.11em; padding: 0.7rem 0.35rem 0.3rem; color: var(--faint);",
                            "{name}"
                        }
                    }
                    for (bot , is_selected) in group_bots {
                        BotRow {
                            key: "{bot.id}",
                            bot,
                            section_ids: section_ids.clone(),
                            selected: is_selected,
                            on_select,
                        }
                    }
                }
            }
        }
    }
}

#[component]
pub fn BotRow(
    bot: Bot,
    section_ids: Vec<String>,
    #[props(default = false)] selected: bool,
    on_select: EventHandler<String>,
) -> Element {
    let mut class = if bot.unread > 0 {
        "bot is-unread".to_string()
    } else {
        "bot".to_string()
    };
    if selected {
        class.push_str(" is-on");
    }
    let preview = bot
        .preview
        .clone()
        .unwrap_or_else(|| "No messages yet".to_string());
    let select_id = bot.id.clone();

    rsx! {
        button {
            class: "{class}",
            onclick: move |_| on_select.call(select_id.clone()),
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
                        // RAIL-01: the ordering (`crate::store::roster::
                        // list_roster`'s `ORDER BY pinned_at IS NULL, name`)
                        // does most of the work of "shows a pinned bot in
                        // the rail" - this glyph is the small, quiet mark
                        // the ticket asks for on top of that, port of
                        // Roster.tsx's `PinGlyph` (same `.bot-pin` class,
                        // already in `assets/rail.css` from before this
                        // ticket - unused until now).
                        if bot.pinned {
                            PinGlyph {}
                        }
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

/// A group chat's row: same shape as a bot's row, its stacked faces
/// (`GroupAvatar`) standing in for one. Port of Roster.tsx:622-660.
/// Right-click opens the picker in edit mode - the TS `onContextMenu`'s
/// `onMenu`, folded straight into "add or remove bots" since that is the
/// only entry on the original's context menu this ticket ports.
#[component]
pub fn GroupRow(
    room: RoomSummary,
    bots: Vec<Bot>,
    section_ids: Vec<String>,
    #[props(default = false)] selected: bool,
    on_select: EventHandler<RoomSummary>,
    on_edit: EventHandler<RoomSummary>,
) -> Element {
    let members: Vec<Bot> = room
        .member_ids
        .iter()
        .filter_map(|id| bots.iter().find(|b| &b.id == id).cloned())
        .collect();

    let mut class = if room.unread > 0 {
        "bot group-row is-unread".to_string()
    } else {
        "bot group-row".to_string()
    };
    if selected {
        class.push_str(" is-on");
    }
    let preview = if room.preview.is_empty() {
        "No messages yet".to_string()
    } else {
        room.preview.clone()
    };
    let select_room = room.clone();
    let edit_room = room.clone();

    rsx! {
        button {
            class: "{class}",
            onclick: move |_| on_select.call(select_room.clone()),
            oncontextmenu: move |evt| {
                evt.prevent_default();
                on_edit.call(edit_room.clone());
            },
            GroupAvatar { members, section_ids: section_ids.clone() }
            span { class: "bot-main",
                span { class: "bot-line",
                    span { class: "bot-name", "{room.title}" }
                    span { class: "bot-when",
                        if room.unread > 0 {
                            i { class: "dot" }
                        }
                    }
                }
                span { class: "bot-preview", "{preview}" }
            }
        }
    }
}

/// A group chat's face: its members' own faces, stacked in a 2x2 cluster.
/// Port of `GroupAvatar.tsx` (whole, 42 lines): up to four faces are drawn;
/// a fifth member and beyond collapse into a "+n" pip rather than a cluster
/// that no longer reads at rail size.
#[component]
pub fn GroupAvatar(
    /// The room's own roster, in the order Josh picked them. Only the
    /// first four are drawn.
    members: Vec<Bot>,
    section_ids: Vec<String>,
    #[props(default = 30.0)] size: f64,
) -> Element {
    let shown: Vec<Bot> = members.iter().take(4).cloned().collect();
    let extra = members.len().saturating_sub(shown.len());
    let face_size = (size * 0.58).round().max(10.0);
    let style = format!("width: {size}px; height: {size}px;");
    let more_style = format!("font-size: {}px;", (size * 0.28).max(8.0));

    rsx! {
        span { class: "group-av", style: "{style}", "aria-hidden": "true",
            for (i , bot) in shown.into_iter().enumerate() {
                span { class: "group-av-face group-av-face-{i}", key: "{bot.id}",
                    Avatar {
                        id: bot.id.clone(),
                        name: bot.name.clone(),
                        section_id: bot.section_id.clone(),
                        section_ids: section_ids.clone(),
                        avatar: bot.avatar.clone(),
                        shape: bot.shape.clone(),
                        size: face_size,
                    }
                }
            }
            if extra > 0 {
                span { class: "group-av-more", style: "{more_style}", "+{extra}" }
            }
        }
    }
}

/// RAIL-01: the small mark a pinned row wears, next to its name - exact
/// port of Roster.tsx's `PinGlyph` (path and all), including its explicit
/// `width`/`height` rather than leaning on `.bot-pin`'s own CSS for sizing
/// (see that component's own doc comment on why: a static contact sheet
/// that pastes this markup outside its stylesheet needs the icon to size
/// itself either way).
#[component]
fn PinGlyph() -> Element {
    rsx! {
        svg {
            class: "bot-pin",
            view_box: "0 0 12 12",
            fill: "currentColor",
            "aria-label": "Pinned",
            role: "img",
            width: "10",
            height: "10",
            path { d: "M6 0.6 7.1 4l2.3.4-2 1.8.4 2.4L6 7.4 3.9 8.6l.4-2.4-2-1.8L4.6 4Z" }
        }
    }
}
