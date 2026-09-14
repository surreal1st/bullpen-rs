//! App shell: fetches `/api/roster` on mount and renders the rail from it.
//! S1-07a wiring: selecting a bot in the rail opens its conversation
//! (`ChatPane`) beside the rail.
//! S1-07b: fetches `/api/rooms` too, so the rail's GROUP CHATS section has
//! something to show; selecting a room opens the same `ChatPane` through
//! its owner bot with `thread_id` set; the room picker (`room_picker.rs`)
//! creates and edits membership. Ported from `App.tsx:241-275`'s "roster"
//! event handling (refetch roster + rooms; bump the open room's key so a
//! reply chained onto member two or later - no SSE stream for this tab -
//! still shows up) and `:986-1000`/`:1266-1276` for where a room's own
//! title/members and the working bar sit.

use crate::api;
use crate::events::{ChangeKind, subscribe_events};
use crate::rail::Rail;
use crate::room_picker::{PickerMode, RoomPicker};
use crate::thread::ChatPane;
use crate::types::{RoomSummary, Roster};
use dioxus::prelude::*;
use gloo_net::http::Request;

async fn fetch_roster() -> Result<Roster, String> {
    let resp = Request::get("/api/roster")
        .send()
        .await
        .map_err(|e| e.to_string())?;
    if !resp.ok() {
        return Err(format!("/api/roster -> {}", resp.status()));
    }
    resp.json::<Roster>().await.map_err(|e| e.to_string())
}

/// What the right-hand pane shows: a bot's own conversation, or a group
/// chat's. `Room` carries the whole `RoomSummary` rather than just an id so
/// `ChatPane` can read its `member_ids`/`title` without a second fetch.
#[derive(Clone, PartialEq)]
enum Selection {
    Bot(String),
    Room(RoomSummary),
}

#[component]
pub fn App() -> Element {
    let mut roster = use_signal::<Option<Result<Roster, String>>>(|| None);
    let mut rooms = use_signal(Vec::<RoomSummary>::new);
    let mut selected = use_signal::<Option<Selection>>(|| None);
    let mut picker = use_signal::<Option<PickerMode>>(|| None);
    // Bumped whenever a "roster" change lands while a ROOM is open, to
    // force that `ChatPane` to remount and re-fetch - ported from
    // `App.tsx`'s `roomOpen.current` branch: a room's second (and later)
    // reply lands on a run this tab never opened a stream for, since the
    // first member's own SSE stream already closed by the time the next
    // one starts.
    let room_refresh = use_signal(|| 0u32);

    use_effect(move || {
        spawn(async move {
            let result = fetch_roster().await;
            roster.set(Some(result));
        });
        spawn(async move {
            if let Ok(list) = api::fetch_rooms().await {
                rooms.set(list);
            }
        });
    });

    // Subscribed once for the app's whole life - same shared-stream posture
    // as every other `subscribe_events` caller (`working_bar.rs`), and the
    // same `use_signal`-not-`use_hook` reasoning: `EventsHandle` is not
    // `Clone`.
    //
    // 🔴 `wasm_bindgen_futures::spawn_local`, not `dioxus::prelude::spawn`,
    // for the two fetches below: this callback runs from `events.rs`'s bare
    // `wasm_bindgen_futures::spawn_local(run())` loop, which Dioxus never
    // considers a "current scope" - `spawn()` there `.unwrap()`s an empty
    // scope stack and aborts the whole wasm instance, silently, on the
    // first "roster" change (see `working_bar.rs`'s `reload`, which hit the
    // exact same failure and is documented there in more depth).
    let _events = use_signal(|| {
        subscribe_events(move |kind| {
            if kind == ChangeKind::Roster {
                wasm_bindgen_futures::spawn_local(async move {
                    roster.set(Some(fetch_roster().await));
                });
                wasm_bindgen_futures::spawn_local(async move {
                    if let Ok(list) = api::fetch_rooms().await {
                        rooms.set(list);
                    }
                });
                if matches!(selected.peek().as_ref(), Some(Selection::Room(_))) {
                    // `Signal` is `Copy`; shadowing with a fresh local `mut`
                    // binding gets a mutable handle to the SAME underlying
                    // storage without needing `subscribe_events`'s `Fn`
                    // closure to become `FnMut` (its `.set()` takes `&mut
                    // self`, and the field captured by the outer closure is
                    // reached only through `&self`).
                    let mut room_refresh = room_refresh;
                    let next = *room_refresh.peek() + 1;
                    room_refresh.set(next);
                }
            }
        })
    });

    let body = match roster.read().as_ref() {
        None => rsx! {
            div { class: "roster", style: "padding: 1rem; color: var(--muted);", "Loading roster…" }
        },
        Some(Ok(data)) => {
            let section_ids: Vec<String> = data.sections.iter().map(|s| s.id.clone()).collect();

            let selected_bot = match selected.read().as_ref() {
                Some(Selection::Bot(id)) => data.bots.iter().find(|b| &b.id == id).cloned(),
                _ => None,
            };
            let selected_bot_id = match selected.read().as_ref() {
                Some(Selection::Bot(id)) => Some(id.clone()),
                _ => None,
            };
            let selected_room = match selected.read().as_ref() {
                Some(Selection::Room(room)) => Some(room.clone()),
                _ => None,
            };
            let selected_room_id = selected_room.as_ref().map(|r| r.id.clone());

            rsx! {
                div { class: "shell",
                    Rail {
                        sections: data.sections.clone(),
                        bots: data.bots.clone(),
                        rooms: rooms.read().clone(),
                        selected: selected_bot_id,
                        selected_room: selected_room_id,
                        on_select: move |id| selected.set(Some(Selection::Bot(id))),
                        on_select_room: move |room: RoomSummary| selected.set(Some(Selection::Room(room))),
                        on_new_room: move |_| picker.set(Some(PickerMode::Create)),
                        on_edit_room: move |room: RoomSummary| picker.set(Some(PickerMode::Edit(room))),
                    }
                    if let Some(room) = selected_room {
                        ChatPane {
                            // A fresh room (or a "roster" bump while this one is
                            // open) is a fresh instance - same "remount, don't
                            // patch" posture `bot.id` already uses below.
                            key: "room:{room.id}:{room_refresh}",
                            bot_id: room.member_ids.first().cloned().unwrap_or_default(),
                            bot_name: room.title.clone(),
                            thread_id: Some(room.id.clone()),
                            section_ids: section_ids.clone(),
                        }
                    } else if let Some(bot) = selected_bot {
                        ChatPane {
                            key: "bot:{bot.id}",
                            bot_id: bot.id.clone(),
                            bot_name: bot.name.clone(),
                            section_ids: section_ids.clone(),
                        }
                    } else {
                        div { class: "pane pane-empty", "Pick a bot to start talking." }
                    }
                }
                if let Some(mode) = picker.read().clone() {
                    RoomPicker {
                        bots: data.bots.clone(),
                        section_ids: section_ids.clone(),
                        mode,
                        on_close: move |_| picker.set(None),
                        on_saved: move |room: RoomSummary| {
                            picker.set(None);
                            // Optimistic: the server's own "roster" change
                            // will refresh `rooms` for real shortly, but
                            // showing the save immediately means Josh does
                            // not have to wait on it to see what he just did.
                            let mut list = rooms.write();
                            match list.iter_mut().find(|r| r.id == room.id) {
                                Some(existing) => *existing = room.clone(),
                                None => list.push(room.clone()),
                            }
                            drop(list);
                            selected.set(Some(Selection::Room(room)));
                        },
                    }
                }
            }
        }
        Some(Err(err)) => rsx! {
            div { class: "roster", style: "padding: 1rem; color: var(--danger);", "Roster failed to load: {err}" }
        },
    };

    rsx! {
        document::Stylesheet { href: asset!("/assets/rail.css") }
        document::Stylesheet { href: asset!("/assets/thread.css") }
        {body}
    }
}
