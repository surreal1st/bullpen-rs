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
//!
//! S1-F-11's added item: `App` (the component `main.rs` launches) is now the
//! sign-in gate, ported from `Gate.tsx`. It owns nothing about bots/rooms
//! itself - once a session is confirmed it renders `AppShell`, which is
//! everything this file used to be before this ticket. F-05 (`S1-F-fixes.md`)
//! made every `/api/*` route except `auth/*`/`health`/`version`/`invites/*`
//! answer 401/503 with no session; without a door in front of it, the app
//! rendered but never fetched anything real (`/api/roster` 401, a spinner
//! forever) - "nobody can open the app now that F-05 landed" is this
//! ticket's own framing for why this exists.

use crate::api;
use crate::attention;
use crate::events::{ChangeKind, subscribe_events};
use crate::library::LibraryModal;
use crate::new_bot::NewBotModal;
use crate::rail::Rail;
use crate::room_picker::{PickerMode, RoomPicker};
use crate::settings::SettingsModal;
use crate::thread::ChatPane;
use crate::transport::Request;
use crate::types::{AwayPayload, Bot, RoomSummary, Roster};
use dioxus::prelude::*;

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

/// What the gate is showing. Mirrors `Gate.tsx`'s `State` union.
#[derive(Clone, PartialEq)]
enum GateState {
    Checking,
    Setup,
    /// `problem` is the last sign-in attempt's error, if any - `None` on
    /// first paint, same as `Gate.tsx`'s `{ kind: "locked" }` (no `problem`
    /// key) vs `{ kind: "locked", problem }`.
    Locked(Option<String>),
    Invited {
        token: String,
        problem: Option<String>,
    },
    Open,
}

fn invited_token_from_hash() -> Option<String> {
    let window = web_sys::window()?;
    let hash = window.location().hash().ok()?;
    let hash = hash.strip_prefix('#')?;
    let token = hash.strip_prefix("invite=")?;
    if token.is_empty()
        || !token
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
    {
        return None;
    }
    Some(token.to_string())
}

fn clear_invite_hash() {
    if let Some(window) = web_sys::window() {
        let path = window
            .location()
            .pathname()
            .unwrap_or_else(|_| "/".to_string());
        if let Ok(history) = window.history() {
            let _ = history.replace_state_with_url(&wasm_bindgen::JsValue::NULL, "", Some(&path));
        }
    }
}

/// The sign-in gate - everything between the open internet and the app
/// shell. Ported from `Gate.tsx:41-254`, trimmed to the two states this
/// ticket asks for: a password form when a password is set but no session is
/// presented, and a "no password set" notice when none is set yet. The
/// server is what actually enforces this (`crate::auth::require_session` on
/// the Rust side) - this is only the door, same disclaimer `Gate.tsx`'s own
/// doc comment makes about itself.
#[component]
pub fn App() -> Element {
    let mut gate = use_signal(|| GateState::Checking);

    use_effect(move || {
        spawn(async move {
            if let Some(token) = invited_token_from_hash() {
                match api::invite_link_valid(&token).await {
                    Ok(true) => {
                        gate.set(GateState::Invited {
                            token,
                            problem: None,
                        });
                        return;
                    }
                    Ok(false) => {
                        gate.set(GateState::Locked(Some(
                            "That invite link is not valid any more.".to_string(),
                        )));
                        return;
                    }
                    Err(_) => {
                        gate.set(GateState::Locked(Some("Cannot reach Bullpen.".to_string())));
                        return;
                    }
                }
            }

            match api::auth_status().await {
                Ok(status) if !status.configured => gate.set(GateState::Setup),
                Ok(status) if status.signed_in => gate.set(GateState::Open),
                Ok(_) => gate.set(GateState::Locked(None)),
                Err(_) => gate.set(GateState::Locked(Some("Cannot reach Bullpen.".to_string()))),
            }
        });
    });

    let mut password = use_signal(String::new);
    let mut busy = use_signal(|| false);

    let mut sign_in = move || {
        if *busy.read() || password.read().is_empty() {
            return;
        }
        busy.set(true);
        spawn(async move {
            let attempt = password.read().clone();
            let result = api::login(&attempt).await;
            busy.set(false);
            match result {
                Ok(()) => {
                    password.set(String::new());
                    gate.set(GateState::Open);
                }
                Err(err) => gate.set(GateState::Locked(Some(err))),
            }
        });
    };

    let mut invite_name = use_signal(String::new);
    let mut accept_invite = move || {
        if *busy.read() {
            return;
        }
        let GateState::Invited { token, .. } = gate.read().clone() else {
            return;
        };
        if invite_name.read().trim().is_empty() || password.read().len() < 8 {
            gate.set(GateState::Invited {
                token,
                problem: Some("Give a name and a password of at least 8 characters.".to_string()),
            });
            return;
        }
        busy.set(true);
        let name = invite_name.read().clone();
        let pass = password.read().clone();
        spawn(async move {
            let result = api::claim_invite(&token, name.trim(), &pass).await;
            busy.set(false);
            match result {
                Ok(()) => {
                    password.set(String::new());
                    invite_name.set(String::new());
                    clear_invite_hash();
                    gate.set(GateState::Open);
                }
                Err(err) => {
                    gate.set(GateState::Invited {
                        token,
                        problem: Some(err),
                    });
                }
            }
        });
    };

    let body = match gate.read().clone() {
        GateState::Checking => rsx! {
            div { class: "gate", "aria-busy": "true" }
        },
        GateState::Setup => rsx! {
            div { class: "gate",
                div { class: "gate-card",
                    h1 { "Bullpen" }
                    p { class: "gate-note",
                        "No password is set on this server yet, so it is refusing everything. Set one, then reload."
                    }
                }
            }
        },
        GateState::Invited { problem, .. } => rsx! {
            div { class: "gate",
                form {
                    class: "gate-card",
                    onsubmit: move |evt| {
                        evt.prevent_default();
                        accept_invite();
                    },
                    h1 { "Join this Bullpen" }
                    p { class: "gate-note", "Pick a name and a password. You will be signed in when this succeeds." }
                    label { class: "gate-label", r#for: "gate-name", "Your name" }
                    input {
                        id: "gate-name",
                        r#type: "text",
                        autocomplete: "name",
                        autofocus: true,
                        value: "{invite_name}",
                        oninput: move |evt| invite_name.set(evt.value()),
                        class: "gate-input",
                    }
                    label { class: "gate-label", r#for: "gate-password", "Password" }
                    input {
                        id: "gate-password",
                        r#type: "password",
                        autocomplete: "new-password",
                        value: "{password}",
                        oninput: move |evt| password.set(evt.value()),
                        class: "gate-input",
                    }
                    button {
                        r#type: "submit",
                        class: "gate-button",
                        disabled: *busy.read(),
                        if *busy.read() { "Joining…" } else { "Join" }
                    }
                    if let Some(problem) = problem {
                        p { class: "gate-problem", "{problem}" }
                    }
                }
            }
        },
        GateState::Locked(problem) => rsx! {
            div { class: "gate",
                form {
                    class: "gate-card",
                    onsubmit: move |evt| {
                        evt.prevent_default();
                        sign_in();
                    },
                    h1 { "Bullpen" }
                    label { class: "gate-label", r#for: "gate-password", "Password" }
                    input {
                        id: "gate-password",
                        r#type: "password",
                        autocomplete: "current-password",
                        autofocus: true,
                        value: "{password}",
                        oninput: move |evt| password.set(evt.value()),
                        class: "gate-input",
                    }
                    button {
                        r#type: "submit",
                        class: "gate-button",
                        disabled: *busy.read() || password.read().is_empty(),
                        if *busy.read() { "Signing in…" } else { "Sign in" }
                    }
                    if let Some(problem) = problem {
                        p { class: "gate-problem", "{problem}" }
                    }
                }
            }
        },
        GateState::Open => rsx! {
            AppShell {}
        },
    };

    rsx! {
        document::Stylesheet { href: asset!("/assets/rail.css") }
        document::Stylesheet { href: asset!("/assets/thread.css") }
        document::Stylesheet { href: asset!("/assets/settings.css") }
        {body}
    }
}

/// Everything this file was before S1-F-11's gate: fetches the roster and
/// rooms, subscribes to server-side changes, and renders the rail beside
/// whichever `ChatPane` is open. Only reached once `App`'s gate confirms a
/// session - see this file's top doc comment.
#[component]
fn AppShell() -> Element {
    let mut roster = use_signal::<Option<Result<Roster, String>>>(|| None);
    let mut rooms = use_signal(Vec::<RoomSummary>::new);
    let mut selected = use_signal::<Option<Selection>>(|| None);
    let mut picker = use_signal::<Option<PickerMode>>(|| None);
    // F7b-01: whether the "New bot" modal is open - same one-signal-per-
    // modal posture as `picker`/`settings_open` below.
    let mut new_bot_open = use_signal(|| false);
    // S2-09b opened this from a floating `position: fixed` button, which sat
    // directly over the composer's Send button (S2-F-08, D2). The trigger
    // now lives in `rail.rs`'s `.rail-group-head` (`on_settings` below);
    // this signal still just tracks whether the modal itself is open - see
    // `settings.rs`'s own doc comment for what it renders.
    let mut settings_open = use_signal(|| false);
    let mut library_open = use_signal(|| false);
    let mut is_owner = use_signal(|| true);
    // Bumped whenever a "roster" change lands while a ROOM is open, to
    // force that `ChatPane` to remount and re-fetch - ported from
    // `App.tsx`'s `roomOpen.current` branch: a room's second (and later)
    // reply lands on a run this tab never opened a stream for, since the
    // first member's own SSE stream already closed by the time the next
    // one starts.
    let room_refresh = use_signal(|| 0u32);
    // S11-07: one fetch for the whole session — `AwayCard.tsx`'s `useAway`.
    let mut away = use_signal(|| None::<AwayPayload>);

    use_effect(move || {
        spawn(async move {
            if let Ok(Some(payload)) = api::fetch_away().await {
                away.set(Some(payload));
            }
        });
    });

    // S11-08: tab title from `/api/attention` (8s poll).
    const ATTENTION_MS: u32 = 8_000;
    use_effect(move || {
        spawn(async move {
            loop {
                if let Ok(counts) = api::fetch_attention().await {
                    attention::apply_document_title(counts.total);
                }
                crate::transport::sleep(ATTENTION_MS).await;
            }
        });
    });

    use_effect(move || {
        spawn(async move {
            if let Ok(status) = api::auth_status().await {
                is_owner.set(status.role.as_deref() != Some("member"));
            }
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
    // 🔴 `crate::transport::spawn_task`, not `dioxus::prelude::spawn`, for
    // the two fetches below: this callback runs from `events.rs`'s bare
    // `spawn_task(run())` loop, which Dioxus never considers a "current
    // scope" - `spawn()` there `.unwrap()`s an empty scope stack and aborts
    // the whole wasm instance, silently, on the first "roster" change (see
    // `working_bar.rs`'s `reload`, which hit the exact same failure and is
    // documented there in more depth; `transport/mod.rs` documents why
    // native's `spawn_task` needs a different escape hatch again).
    let _events = use_signal(|| {
        subscribe_events(move |kind| {
            if kind == ChangeKind::Roster {
                crate::transport::spawn_task(async move {
                    roster.set(Some(fetch_roster().await));
                });
                crate::transport::spawn_task(async move {
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
            let away_snapshot = away.read().clone();
            let mut away_signal = away;
            let dismiss_away = move |_| {
                away_signal.with_mut(|state| {
                    if let Some(payload) = state.as_mut() {
                        payload.show = false;
                    }
                });
                crate::transport::spawn_task(async move {
                    api::dismiss_away().await;
                });
            };
            let open_bot_from_away = move |id: String| {
                selected.set(Some(Selection::Bot(id)));
            };

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
                        on_new_bot: move |_| new_bot_open.set(true),
                        on_edit_room: move |room: RoomSummary| picker.set(Some(PickerMode::Edit(room))),
                        on_settings: move |_| settings_open.set(true),
                        on_library: move |_| library_open.set(true),
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
                            // F14: the dot only clears once the roster/room
                            // list this rail reads from is refetched - `/seen`
                            // itself does not push a "roster" change (see
                            // `routes/rooms.rs::mark_seen`, which touches no
                            // change bus), so this pane asks for that refresh
                            // explicitly rather than waiting on SSE.
                            on_seen: move |_| {
                                spawn(async move {
                                    if let Ok(list) = api::fetch_rooms().await {
                                        rooms.set(list);
                                    }
                                });
                            },
                            // ARCH-01: `thread.rs` never renders the Archive
                            // button/modal for a room (`bot` is `None`
                            // there - see `ChatPane`'s own doc), so this
                            // never actually fires; still a required prop,
                            // wired the same as the bot branch below for
                            // when it ever could.
                            on_archived: move |_| {
                                spawn(async move {
                                    roster.set(Some(fetch_roster().await));
                                });
                            },
                            // RAIL-01: `thread.rs` never renders the
                            // pin/hide buttons for a room either (same
                            // `bot` is `None` gap `on_archived` above
                            // already notes) - still a required prop, wired
                            // the same way for when it ever could.
                            on_rail_changed: move |_| {
                                spawn(async move {
                                    roster.set(Some(fetch_roster().await));
                                });
                            },
                            // DUP-01: `thread.rs` never renders "Duplicate"
                            // for a room either (same `bot` is `None` gap
                            // the two props above already note) - still a
                            // required prop, wired to the same harmless
                            // refresh for when it ever could.
                            on_duplicated: move |_bot: Bot| {
                                spawn(async move {
                                    roster.set(Some(fetch_roster().await));
                                });
                            },
                            away: away_snapshot.clone(),
                            on_dismiss_away: dismiss_away,
                            on_open_bot_away: open_bot_from_away,
                        }
                    } else if let Some(bot) = selected_bot {
                        ChatPane {
                            key: "bot:{bot.id}",
                            bot_id: bot.id.clone(),
                            bot_name: bot.name.clone(),
                            section_ids: section_ids.clone(),
                            // RAIL-02: the full section list (id + name), for
                            // the header's "Move to" picker - `section_ids`
                            // above only ever carried ids (avatar hashing),
                            // never names a picker could show.
                            sections: data.sections.clone(),
                            bot: Some(bot.clone()),
                            on_seen: move |_| {
                                spawn(async move {
                                    roster.set(Some(fetch_roster().await));
                                });
                            },
                            // ARCH-01: same refresh `on_seen` already
                            // triggers - once the roster no longer carries
                            // this bot, the `selected_bot` lookup above
                            // comes back `None` on the next render, which is
                            // what actually moves the pane off it (see
                            // `thread.rs`'s `on_archived` doc).
                            on_archived: move |_| {
                                spawn(async move {
                                    roster.set(Some(fetch_roster().await));
                                });
                            },
                            // RAIL-01: same refresh `on_seen`/`on_archived`
                            // already trigger - a pin moves the row inside
                            // the rail's own ordering, and a hide flips the
                            // flag `rail.rs`'s filter reads, so both need
                            // the roster re-fetched for the RAIL to notice;
                            // this pane's own `local_bot` (see `thread.rs`'s
                            // doc on it) already updates itself immediately
                            // for the header buttons' own label.
                            on_rail_changed: move |_| {
                                spawn(async move {
                                    roster.set(Some(fetch_roster().await));
                                });
                            },
                            // DUP-01: hands the new bot up exactly the way
                            // `NewBotModal`'s own `on_created` below does -
                            // merge it into the roster (or push it, if this
                            // fetch races the roster's own eventual
                            // refresh), then select it, so "the copy should
                            // be what you are looking at" (the ticket's own
                            // requirement) holds without a second refetch.
                            // `on_rail_changed` above cannot do this itself:
                            // it carries no payload, so it can refresh the
                            // roster but has nothing to select.
                            on_duplicated: move |bot: Bot| {
                                let mut current = roster.write();
                                if let Some(Ok(data)) = current.as_mut() {
                                    match data.bots.iter_mut().find(|b| b.id == bot.id) {
                                        Some(existing) => *existing = bot.clone(),
                                        None => data.bots.push(bot.clone()),
                                    }
                                }
                                drop(current);
                                selected.set(Some(Selection::Bot(bot.id.clone())));
                            },
                            away: away_snapshot,
                            on_dismiss_away: dismiss_away,
                            on_open_bot_away: open_bot_from_away,
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
                if *new_bot_open.read() {
                    NewBotModal {
                        on_close: move |_| new_bot_open.set(false),
                        on_created: move |bot: Bot| {
                            new_bot_open.set(false);
                            // Optimistic, same posture as `RoomPicker`'s
                            // `on_saved` above: land it in `roster` right
                            // away rather than waiting on a refetch, so the
                            // next thing Josh sees is its empty thread.
                            let mut current = roster.write();
                            if let Some(Ok(data)) = current.as_mut() {
                                match data.bots.iter_mut().find(|b| b.id == bot.id) {
                                    Some(existing) => *existing = bot.clone(),
                                    None => data.bots.push(bot.clone()),
                                }
                            }
                            drop(current);
                            selected.set(Some(Selection::Bot(bot.id.clone())));
                        },
                    }
                }
            }
        }
        Some(Err(err)) => rsx! {
            div { class: "roster", style: "padding: 1rem; color: var(--danger);", "Roster failed to load: {err}" }
        },
    };

    // Stylesheets are loaded once, by `App` itself (this component is only
    // ever reached through its gate) - see that component's own rsx!.
    rsx! {
        {body}
        if library_open() {
            LibraryModal {
                on_close: move |_| library_open.set(false),
            }
        }
        if *settings_open.read() {
            SettingsModal {
                is_owner: *is_owner.read(),
                on_close: move |_| settings_open.set(false),
                // ARCH-01: a restore from `ArchivedBotsSection` should bring
                // the bot back into the rail right away, same refresh
                // `ChatPane`'s own `on_seen`/`on_archived` already trigger.
                // RAIL-02's `SectionsManagerSection` reuses this same
                // handler after a create/rename/delete.
                on_restored: move |_| {
                    spawn(async move {
                        roster.set(Some(fetch_roster().await));
                    });
                },
                // RAIL-02: same source `Rail` itself reads sections from
                // (`data.sections`, above) - the modal has no roster fetch
                // of its own, see `SettingsModal`'s own doc on `sections`.
                sections: match roster.read().as_ref() {
                    Some(Ok(data)) => data.sections.clone(),
                    _ => Vec::new(),
                },
            }
        }
    }
}
