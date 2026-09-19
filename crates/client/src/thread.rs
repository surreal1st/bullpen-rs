//! Port of the `<section className="thread">` markup in
//! `projects/bullpen-night/src/client/App.tsx:1120-1276`: message rows, the
//! date/pause divider between groups, the streaming row, and the bottom
//! anchor that keeps the pane scrolled to the newest message. Also owns the
//! send path from `App.tsx:470-601` (`send()`: POST messages, the SSE
//! loop), since that state (the message list, the in-flight streaming
//! text) has to live somewhere above both the read-only list and the
//! composer that triggers it.

use crate::api;
use crate::approvals::Approvals;
use crate::bubble::Bubble;
use crate::composer::Composer;
use crate::edit_bot::EditBotModal;
use crate::goals_editor::GoalsModal;
use crate::memory_editor::MemoryModal;
use crate::message_time::{day_key, format_day, format_time, now_iso, parse_epoch_ms};
use crate::model_chip::ModelChip;
use crate::permissions_editor::PermissionsModal;
use crate::questions::Questions;
use crate::routines_editor::RoutinesModal;
use crate::types::{Bot, Message, Role, Section};
use crate::vm_card::VmCard;
use crate::working_bar::WorkingBar;
use dioxus::prelude::*;
use shared::faces::SHAPES;
use std::rc::Rc;

/// Owns one bot's (or room's) conversation: fetches it, renders it, and
/// drives sends. Give this a `key` unique to the open conversation at the
/// call site (see `app.rs`) - a fresh key should be a fresh component
/// instance, not a signal update, so switching what is open resets every
/// signal here for free instead of needing its own "is this a switch or a
/// refresh" logic (`isConversationSwitch` in the original - out of scope
/// while there is only ever one thread open).
///
/// `thread_id` is `Some(room.id)` when this pane is a group chat opened
/// through its owner bot (`rail.rs`'s `GroupRow`) - S1-07b's addition over
/// S1-07a, which only ever talked to a bot's default conversation.
///
/// `on_seen` (F14, S1-F-11) fires once the `/seen` call below resolves, so
/// `app.rs` can refetch the roster/room list the dot is drawn from - ported
/// from `App.tsx:758,769`'s `fetch(.../seen).then(() => loadBots())`. This
/// component has no access to those lists itself (they live above it, in
/// `app.rs`), so it can only ask for a refresh, not perform one.
#[component]
pub fn ChatPane(
    bot_id: String,
    bot_name: String,
    #[props(default)] thread_id: Option<String>,
    #[props(default)] section_ids: Vec<String>,
    // RAIL-02: the full section list (id + name) for the header's "Move to"
    // picker - `section_ids` above only ever carries bare ids (`Avatar`'s
    // colour hashing, `WorkingBar`'s own use), never names a `<select>`
    // could show. `#[props(default)]` so the room branch of `app.rs`'s
    // `ChatPane` call (which never renders `pane-head` at all - see this
    // component's own doc on `bot: Option<Bot>`) does not have to pass one.
    #[props(default)] sections: Vec<Section>,
    // S2-09b: the full roster row for the open bot, when this pane is a
    // bot's own conversation (not a room - `app.rs` only ever has one bot
    // object to hand over there). Carries the model pin/effort the header's
    // chip and the permissions grid below it both need. `None` for a room:
    // neither the chip nor the grid is a single bot's own setting in that
    // case, so this ticket narrows both to the non-room path (see this
    // component's own `## Result` note on the gap).
    #[props(default)] bot: Option<Bot>,
    on_seen: EventHandler<()>,
    // ARCH-01: fires once the confirm modal's archive call resolves, so
    // `app.rs` can refresh the roster the same way `on_seen` already does -
    // this component has no roster/selection state of its own (see this
    // component's own doc on `on_seen`), only the ability to ask for a
    // refresh. Once the roster no longer carries this bot, `app.rs`'s own
    // `selected_bot` lookup (`data.bots.iter().find(...)`) comes back
    // `None` on its own, which is what actually moves the pane off the
    // archived bot - nothing here has to clear a selection directly.
    on_archived: EventHandler<()>,
    // RAIL-01: fires once a pin/hide toggle below resolves, same posture as
    // `on_archived` above - this component asks for a roster refresh, it
    // does not hold the roster itself. A hide moves the bot off the rail
    // the same indirect way archiving does (`app.rs`'s `selected_bot`
    // lookup comes back `None` once the roster no longer shows it there -
    // except a hidden bot's roster row does NOT disappear, only its
    // `hidden` flag flips, so this pane stays open on a bot Josh just hid,
    // exactly as clicking "Hide" while reading it should feel: reversible,
    // not a surprise eviction. See `crate::store::roster::list_roster`'s
    // own doc on why `hidden` never leaves the roster response the way
    // `archived` does).
    on_rail_changed: EventHandler<()>,
    // DUP-01: fires once "Duplicate" below resolves, carrying the NEW bot -
    // deliberately not reusing `on_rail_changed` (`EventHandler<()>`, no
    // payload): a plain roster refresh alone would leave Josh looking at
    // the SOURCE bot while the copy sits somewhere else on the rail, and
    // "the copy should be what you are looking at" is the ticket's own
    // requirement. This carries the same `Bot` payload `new_bot.rs`'s own
    // `on_created` does, and `app.rs` wires it to the identical "merge into
    // the roster, then select it" logic that prop already uses - the
    // pattern this ticket asked to reuse, just under its own name since
    // `ChatPane` (not `NewBotModal`) is where this fires from.
    on_duplicated: EventHandler<Bot>,
) -> Element {
    let mut messages = use_signal(Vec::<Message>::new);
    let mut streaming = use_signal(|| None::<String>);
    let mut sending = use_signal(|| false);
    let mut load_error = use_signal(|| None::<String>);
    // The working bar needs the REAL conversation id (a bot's default
    // thread is created lazily server-side, so `thread_id` alone is not
    // always it) - a `Signal` rather than a plain field so `working_bar.rs`'s
    // `use_effect` re-runs once the fetch below resolves it, the same
    // reason `messages`/`streaming` below are `Signal`s the read-only
    // `Thread` takes rather than owned values.
    let mut conversation_id = use_signal(|| None::<String>);

    // F14: opening this pane is what makes it read. `thread_id` distinguishes
    // a room (owns its own `seen_at` on the `conversations` row) from a bot's
    // own default conversation (owns `last_seen_at` on the `bots` row) - see
    // `crates/server/src/routes/mod.rs`'s `mark_bot_seen` and
    // `crates/server/src/routes/rooms.rs`'s `mark_seen` for the two different
    // columns this clears. Runs once per mount (this component is remounted
    // by a fresh `key` on every switch, never patched in place - see the doc
    // above), which is exactly "on open".
    let seen_bot_id = bot_id.clone();
    let seen_thread_id = thread_id.clone();
    use_effect(move || {
        let bot_id = seen_bot_id.clone();
        let thread_id = seen_thread_id.clone();
        spawn(async move {
            let result = match &thread_id {
                Some(room_id) => api::mark_room_seen(room_id).await,
                None => api::mark_bot_seen(&bot_id).await,
            };
            if result.is_ok() {
                on_seen.call(());
            }
        });
    });

    let fetch_bot_id = bot_id.clone();
    let fetch_thread_id = thread_id.clone();
    use_effect(move || {
        let bot_id = fetch_bot_id.clone();
        let thread_id = fetch_thread_id.clone();
        spawn(async move {
            match api::fetch_conversation(&bot_id, thread_id.as_deref()).await {
                // Ported behaviour from 0.4.8 (the ticket's own callout):
                // nothing re-renders when the fetched data is identical.
                // `ConversationView` derives `PartialEq`, so this is a
                // direct compare rather than the original's
                // serialize-and-compare `sameData`.
                Ok(view) => {
                    conversation_id.set(Some(view.conversation_id.clone()));
                    if *messages.peek() != view.messages {
                        messages.set(view.messages);
                    }
                }
                Err(err) => load_error.set(Some(err)),
            }
        });
    });

    let send_bot_id = bot_id.clone();
    let send_thread_id = thread_id.clone();
    let on_send = move |text: String| {
        let bot_id = send_bot_id.clone();
        let thread_id = send_thread_id.clone();
        messages.write().push(Message {
            id: format!("local-{}", now_iso()),
            role: Role::User,
            content: text.clone(),
            model: None,
            error: None,
            created_at: now_iso(),
        });
        streaming.set(Some(String::new()));
        sending.set(true);
        spawn(async move {
            let mut assembled = String::new();
            let result =
                api::send_message(&bot_id, &text, thread_id.as_deref(), |event| match event {
                    api::StreamEvent::Delta { text } => {
                        assembled.push_str(&text);
                        streaming.set(Some(assembled.clone()));
                    }
                    api::StreamEvent::Done { model } => {
                        messages.write().push(Message {
                            id: format!("local-{}", now_iso()),
                            role: Role::Assistant,
                            content: assembled.clone(),
                            model,
                            error: None,
                            created_at: now_iso(),
                        });
                        streaming.set(None);
                    }
                    // F7 (S1-F-11): a run that failed used to be an `Ignored`
                    // frame - the streaming bubble went blank, the composer
                    // re-enabled, and nothing on screen said why. Whatever
                    // text arrived before the failure is kept (a partial
                    // answer is still evidence), with `error` set so
                    // `bubble.rs` renders the `.upstream-error` box under it.
                    api::StreamEvent::Error { message } => {
                        messages.write().push(Message {
                            id: format!("local-{}", now_iso()),
                            role: Role::Assistant,
                            content: assembled.clone(),
                            model: None,
                            error: Some(message),
                            created_at: now_iso(),
                        });
                        streaming.set(None);
                    }
                    api::StreamEvent::Run { .. } | api::StreamEvent::Ignored => {}
                })
                .await;
            if let Err(err) = result {
                streaming.set(None);
                load_error.set(Some(err));
            }
            sending.set(false);
        });
    };

    // S2-09b: a local echo of the open bot's pin/effort, seeded from the
    // `bot` prop and updated by `ModelChip`'s own `on_saved` - this pane has
    // no roster of its own to write a PATCH's result back into (that lives
    // above it, in `app.rs`), so the chip and the permissions grid read
    // their own copy rather than going stale until the next full roster
    // fetch. A known, scoped gap: the RAIL's copy (e.g. a "default" tag on
    // the bot row, if one is ever added there) still only refreshes on the
    // next roster load.
    let mut local_bot = use_signal(|| bot.clone());
    use_effect(move || {
        local_bot.set(bot.clone());
    });

    // EDIT-01: the identity editor (name/purpose/instructions) - first in
    // this control group, since it is the most-needed one of all of them
    // (see `edit_bot.rs`'s own top doc on the hole this closes). Same
    // modal-over-the-thread placement as `PermissionsModal` etc. below.
    let mut edit_open = use_signal(|| false);
    // S2-F-08 (D1): the permissions grid used to sit inline under the
    // header (`section.pane-perms`), eating the top half of the pane on
    // every bot. It now opens over the thread instead, behind this button -
    // see `permissions_editor.rs`'s `PermissionsModal`.
    let mut perms_open = use_signal(|| false);
    // S3-05: the memory pane, same placement as "Permissions" beside it in
    // `pane-head-meta` - see `memory_editor.rs`'s `MemoryModal`.
    let mut mem_open = use_signal(|| false);
    // S5-05: the routines pane, same placement again - see
    // `routines_editor.rs`'s `RoutinesModal`.
    let mut routines_open = use_signal(|| false);
    // S5b-07: the goals pane, same placement again - see
    // `goals_editor.rs`'s `GoalsModal`.
    let mut goals_open = use_signal(|| false);
    // ARCH-01: the archive confirm, same placement again - see
    // `ArchiveConfirmModal` below. A confirm rather than a one-click action
    // (unlike e.g. `routines_editor.rs`'s "Delete" button): archiving hides
    // a bot from the roster, and a mis-click that makes a bot vanish is
    // exactly the kind of thing Josh would have to come asking about.
    let mut archive_open = use_signal(|| false);
    // RAIL-01: pin/hide are direct actions, not modals - the ticket's own
    // "No confirm for either. Both are trivially reversible and a confirm
    // on a pin would be noise." `rail_error` is shared by both buttons
    // (only one of them is ever busy at a time - each disables only itself
    // via its own `_busy` signal below).
    let mut rail_error = use_signal(|| None::<String>);
    let mut pin_busy = use_signal(|| false);
    let mut hide_busy = use_signal(|| false);
    // RAIL-02: the "Move to" picker - a direct action, not a modal, same
    // "no confirm" posture `toggle_pinned`/`toggle_hidden` already take
    // (`rail_error` above is shared by all three, only one of them is ever
    // busy at a time).
    let mut move_busy = use_signal(|| false);
    // RAIL-03: the avatar field and shape picker - same direct-action, no
    // confirm posture as the three above (`rail_error` is shared by all
    // five now, still only one busy at a time).
    let mut avatar_busy = use_signal(|| false);
    let mut shape_busy = use_signal(|| false);
    // EXPORT-01: the "Export" action - same direct-action, no-confirm
    // posture, `rail_error` shared with the five above. `export_ready`
    // holds the `data:` URI once a fetch completes; see the closure below
    // that fills it for why this is two signals rather than one `Result`
    // (busy and "there is a ready download" are not mutually exclusive
    // with error - a fresh click clears `export_ready` immediately so a
    // stale download never sits next to a new in-flight fetch).
    let mut export_busy = use_signal(|| false);
    let mut export_ready = use_signal(|| None::<String>);
    // DUP-01: "Duplicate" - same direct-action, no-confirm posture as
    // Export above (`rail_error` shared with all seven controls in this
    // header now). Unlike every other action here, success does not patch
    // `local_bot` at all - the new bot is a DIFFERENT row, handed straight
    // up through `on_duplicated` instead (see that prop's own doc).
    let mut duplicate_busy = use_signal(|| false);

    rsx! {
        div { class: "pane",
            if let Some(current) = local_bot.read().clone() {
                {
                    // RAIL-01: `toggle_pinned`/`toggle_hidden` below both
                    // close over one clone of `current` each - `current`
                    // itself is still needed unmoved for `ModelChip`'s
                    // `bot: current` further down, and each closure needs
                    // its own copy to read `.pinned`/`.hidden` from and to
                    // write the toggled value back into after a successful
                    // call (`local_bot.set(Some(updated))`, the same
                    // "seed a local copy, patch it on success" posture
                    // `ModelChip`'s own `on_saved` already established for
                    // this pane).
                    let pin_current = current.clone();
                    let pin_bot_id = bot_id.clone();
                    let pin_next = !current.pinned;
                    let toggle_pinned = move |_| {
                        if *pin_busy.read() {
                            return;
                        }
                        pin_busy.set(true);
                        rail_error.set(None);
                        let bot_id = pin_bot_id.clone();
                        let mut updated = pin_current.clone();
                        spawn(async move {
                            match api::set_bot_pinned(&bot_id, pin_next).await {
                                Ok(()) => {
                                    pin_busy.set(false);
                                    updated.pinned = pin_next;
                                    local_bot.set(Some(updated));
                                    on_rail_changed.call(());
                                }
                                Err(err) => {
                                    pin_busy.set(false);
                                    rail_error.set(Some(err));
                                }
                            }
                        });
                    };

                    let hide_current = current.clone();
                    let hide_bot_id = bot_id.clone();
                    let hide_next = !current.hidden;
                    let toggle_hidden = move |_| {
                        if *hide_busy.read() {
                            return;
                        }
                        hide_busy.set(true);
                        rail_error.set(None);
                        let bot_id = hide_bot_id.clone();
                        let mut updated = hide_current.clone();
                        spawn(async move {
                            match api::set_bot_hidden(&bot_id, hide_next).await {
                                Ok(()) => {
                                    hide_busy.set(false);
                                    updated.hidden = hide_next;
                                    local_bot.set(Some(updated));
                                    on_rail_changed.call(());
                                }
                                Err(err) => {
                                    hide_busy.set(false);
                                    rail_error.set(Some(err));
                                }
                            }
                        });
                    };

                    // RAIL-02: moves the bot to the picked section, or to
                    // Unassigned (`""`, the picker's own first option) -
                    // same "seed a local copy, patch it on success" posture
                    // as `toggle_pinned`/`toggle_hidden` above, except the
                    // new value comes from the change event itself (a
                    // `<select>`'s current choice) rather than a fixed
                    // "next" computed ahead of time.
                    let move_current = current.clone();
                    let move_bot_id = bot_id.clone();
                    let on_move_section = move |evt: FormEvent| {
                        if *move_busy.read() {
                            return;
                        }
                        let value = evt.value();
                        let target = if value.is_empty() { None } else { Some(value) };
                        move_busy.set(true);
                        rail_error.set(None);
                        let bot_id = move_bot_id.clone();
                        let mut updated = move_current.clone();
                        spawn(async move {
                            match api::set_bot_section(&bot_id, target.as_deref()).await {
                                Ok(()) => {
                                    move_busy.set(false);
                                    updated.section_id = target;
                                    local_bot.set(Some(updated));
                                    on_rail_changed.call(());
                                }
                                Err(err) => {
                                    move_busy.set(false);
                                    rail_error.set(Some(err));
                                }
                            }
                        });
                    };
                    let current_section = current.section_id.clone().unwrap_or_default();

                    // RAIL-03: sets or clears the avatar override - the
                    // same "seed a local copy, patch it on success" posture
                    // as `toggle_pinned`/`on_move_section` above. An empty
                    // (or whitespace-only) field clears it; the two-code-
                    // point cap is enforced server-side
                    // (`store::set_avatar`'s own doc), not duplicated here.
                    let avatar_current = current.clone();
                    let avatar_bot_id = bot_id.clone();
                    let on_avatar_change = move |evt: FormEvent| {
                        if *avatar_busy.read() {
                            return;
                        }
                        let value = evt.value();
                        let target = if value.trim().is_empty() {
                            None
                        } else {
                            Some(value)
                        };
                        avatar_busy.set(true);
                        rail_error.set(None);
                        let bot_id = avatar_bot_id.clone();
                        let mut updated = avatar_current.clone();
                        spawn(async move {
                            match api::set_bot_avatar(&bot_id, target.as_deref()).await {
                                Ok(()) => {
                                    avatar_busy.set(false);
                                    updated.avatar = target;
                                    local_bot.set(Some(updated));
                                    on_rail_changed.call(());
                                }
                                Err(err) => {
                                    avatar_busy.set(false);
                                    rail_error.set(Some(err));
                                }
                            }
                        });
                    };
                    let current_avatar = current.avatar.clone().unwrap_or_default();

                    // RAIL-03: the shape picker - options come from
                    // `shared::faces::SHAPES`, the same table the client's
                    // own `avatar.rs` renders the generated face from, so
                    // this picker can never offer a name the server would
                    // refuse (it never refuses one anyway - see
                    // `store::set_shape`'s own doc). The first option
                    // ("Auto") clears the override back to the hashed
                    // default, same as an empty `sectionId` unassigning.
                    let shape_current = current.clone();
                    let shape_bot_id = bot_id.clone();
                    let on_shape_change = move |evt: FormEvent| {
                        if *shape_busy.read() {
                            return;
                        }
                        let value = evt.value();
                        let target = if value.is_empty() { None } else { Some(value) };
                        shape_busy.set(true);
                        rail_error.set(None);
                        let bot_id = shape_bot_id.clone();
                        let mut updated = shape_current.clone();
                        spawn(async move {
                            match api::set_bot_shape(&bot_id, target.as_deref()).await {
                                Ok(()) => {
                                    shape_busy.set(false);
                                    updated.shape = target;
                                    local_bot.set(Some(updated));
                                    on_rail_changed.call(());
                                }
                                Err(err) => {
                                    shape_busy.set(false);
                                    rail_error.set(Some(err));
                                }
                            }
                        });
                    };
                    let current_shape = current.shape.clone().unwrap_or_default();

                    // EXPORT-01: fetches the bot's markdown export through
                    // `api::export_bot_markdown` (the transport layer that
                    // carries the desktop build's Bearer token - a bare
                    // `<a href>` to the raw route would 401 there, see that
                    // function's own doc) and hands the bytes to a `data:`
                    // URI once they arrive - the same "authenticated fetch,
                    // then a fully declarative element" split `vm_card.rs`
                    // already uses for the thumbnail. `export_ready` holds
                    // that URI; the `<a download>` below only appears once
                    // it is set, and clicking it is the platform's own
                    // save-file UI, not anything this app drives itself.
                    // `rail_error` is shared by every control in this
                    // header, same posture as pin/hide/move/avatar/shape
                    // above.
                    let export_bot_id = bot_id.clone();
                    let export_filename = format!("{bot_id}.md");
                    let on_export_click = move |_| {
                        if *export_busy.read() {
                            return;
                        }
                        export_busy.set(true);
                        rail_error.set(None);
                        export_ready.set(None);
                        let bot_id = export_bot_id.clone();
                        spawn(async move {
                            match api::export_bot_markdown(&bot_id).await {
                                Ok(bytes) => {
                                    export_busy.set(false);
                                    export_ready.set(Some(format!(
                                        "data:text/markdown;charset=utf-8;base64,{}",
                                        crate::vm_card::base64_encode(&bytes)
                                    )));
                                }
                                Err(err) => {
                                    export_busy.set(false);
                                    rail_error.set(Some(err));
                                }
                            }
                        });
                    };

                    // DUP-01: same direct-action posture as
                    // `on_export_click` above, `rail_error` shared. On
                    // success this hands the NEW bot straight up through
                    // `on_duplicated` (see that prop's own doc for why -
                    // there is no local row to patch, unlike every pin/
                    // hide/move/avatar/shape toggle above).
                    let duplicate_bot_id = bot_id.clone();
                    let on_duplicate_click = move |_| {
                        if *duplicate_busy.read() {
                            return;
                        }
                        duplicate_busy.set(true);
                        rail_error.set(None);
                        let bot_id = duplicate_bot_id.clone();
                        spawn(async move {
                            match api::duplicate_bot(&bot_id).await {
                                Ok(bot) => {
                                    duplicate_busy.set(false);
                                    on_duplicated.call(bot);
                                }
                                Err(err) => {
                                    duplicate_busy.set(false);
                                    rail_error.set(Some(err));
                                }
                            }
                        });
                    };

                    rsx! {
                        div { class: "pane-head",
                            div { class: "pane-head-who",
                                b { "{bot_name}" }
                            }
                            div { class: "pane-head-meta",
                                button {
                                    class: "pane-perms-btn",
                                    onclick: move |_| edit_open.set(true),
                                    "Edit"
                                }
                                button {
                                    class: "pane-perms-btn",
                                    onclick: move |_| perms_open.set(true),
                                    "Permissions"
                                }
                                button {
                                    class: "pane-perms-btn",
                                    onclick: move |_| mem_open.set(true),
                                    "Memory"
                                }
                                button {
                                    class: "pane-perms-btn",
                                    onclick: move |_| routines_open.set(true),
                                    "Routines"
                                }
                                button {
                                    class: "pane-perms-btn",
                                    onclick: move |_| goals_open.set(true),
                                    "Goals"
                                }
                                select {
                                    class: "pane-perms-btn",
                                    "aria-label": "Move to section",
                                    disabled: *move_busy.read(),
                                    value: "{current_section}",
                                    onchange: on_move_section,
                                    option { value: "", "Unassigned" }
                                    for section in sections.iter() {
                                        option { key: "{section.id}", value: "{section.id}", "{section.name}" }
                                    }
                                }
                                button {
                                    class: "pane-perms-btn",
                                    disabled: *pin_busy.read(),
                                    onclick: toggle_pinned,
                                    if current.pinned { "Unpin" } else { "Pin" }
                                }
                                button {
                                    class: "pane-perms-btn",
                                    disabled: *hide_busy.read(),
                                    onclick: toggle_hidden,
                                    if current.hidden { "Unhide" } else { "Hide" }
                                }
                                input {
                                    class: "pane-perms-btn pane-avatar-input",
                                    "aria-label": "Avatar",
                                    title: "Avatar",
                                    r#type: "text",
                                    maxlength: "8",
                                    placeholder: "🙂",
                                    disabled: *avatar_busy.read(),
                                    value: "{current_avatar}",
                                    onchange: on_avatar_change,
                                }
                                select {
                                    class: "pane-perms-btn",
                                    "aria-label": "Shape",
                                    disabled: *shape_busy.read(),
                                    value: "{current_shape}",
                                    onchange: on_shape_change,
                                    option { value: "", "Auto shape" }
                                    for (key , shape) in SHAPES.iter() {
                                        option { key: "{key}", value: "{key}", "{shape.label}" }
                                    }
                                }
                                button {
                                    class: "pane-perms-btn",
                                    disabled: *export_busy.read(),
                                    onclick: on_export_click,
                                    if *export_busy.read() { "Exporting…" } else { "Export" }
                                }
                                if let Some(href) = export_ready.read().clone() {
                                    a {
                                        class: "pane-perms-btn",
                                        href: "{href}",
                                        download: "{export_filename}",
                                        onclick: move |_| export_ready.set(None),
                                        "Download .md"
                                    }
                                }
                                button {
                                    class: "pane-perms-btn",
                                    disabled: *duplicate_busy.read(),
                                    onclick: on_duplicate_click,
                                    if *duplicate_busy.read() { "Duplicating…" } else { "Duplicate" }
                                }
                                button {
                                    class: "pane-perms-btn",
                                    onclick: move |_| archive_open.set(true),
                                    "Archive"
                                }
                                ModelChip {
                                    bot: current,
                                    on_saved: move |updated: Bot| local_bot.set(Some(updated)),
                                }
                            }
                        }
                        if let Some(err) = rail_error.read().clone() {
                            p { class: "composer-error", "{err}" }
                        }
                    }
                }
                // S6-VM-01: the bot's own computer, live, at the top of the
                // panel - same placement the TS `BotPanel.tsx` gives it,
                // first thing under the head. See `vm_card.rs`'s own doc
                // for why this file (not a separate panel file) is where
                // "the top of the bot panel" lands in this client. Inside
                // this same `if let Some(current) = ...` as `pane-head`
                // (not a sibling of it) so a room's conversation - `bot` is
                // `None` there, see this component's own doc - stays
                // narrowed to the non-room path exactly like the
                // permissions grid and `ModelChip` just above it.
                VmCard { bot_id: bot_id.clone(), bot_name: bot_name.clone() }
            }
            if *edit_open.read() {
                if let Some(current) = local_bot.read().clone() {
                    EditBotModal {
                        bot: current,
                        on_close: move |_| edit_open.set(false),
                        on_saved: move |updated: Bot| {
                            edit_open.set(false);
                            // Same "seed a local copy from the server's
                            // response" posture pin/hide/move/avatar/shape/
                            // `ModelChip` above already take, so the header
                            // shows the new name immediately - PLUS
                            // `on_rail_changed`, which none of those need
                            // (a pin/hide/avatar/shape/model change is not
                            // visible on the rail's own bot row the way a
                            // name is).
                            local_bot.set(Some(updated));
                            on_rail_changed.call(());
                        },
                    }
                }
            }
            if *perms_open.read() {
                PermissionsModal {
                    bot_id: bot_id.clone(),
                    bot_name: bot_name.clone(),
                    on_close: move |_| perms_open.set(false),
                }
            }
            if *mem_open.read() {
                MemoryModal {
                    bot_id: bot_id.clone(),
                    bot_name: bot_name.clone(),
                    on_close: move |_| mem_open.set(false),
                }
            }
            if *routines_open.read() {
                RoutinesModal {
                    bot_id: bot_id.clone(),
                    bot_name: bot_name.clone(),
                    on_close: move |_| routines_open.set(false),
                }
            }
            if *goals_open.read() {
                GoalsModal {
                    bot_id: bot_id.clone(),
                    bot_name: bot_name.clone(),
                    on_close: move |_| goals_open.set(false),
                }
            }
            if *archive_open.read() {
                ArchiveConfirmModal {
                    bot_id: bot_id.clone(),
                    bot_name: bot_name.clone(),
                    on_close: move |_| archive_open.set(false),
                    on_archived: move |_| {
                        archive_open.set(false);
                        on_archived.call(());
                    },
                }
            }
            if let Some(err) = load_error.read().clone() {
                p { class: "composer-error", "{err}" }
            }
            Thread {
                messages,
                streaming,
                bot_name: bot_name.clone(),
                conversation_id,
                section_ids: section_ids.clone(),
            }
            // A paused run is the most urgent thing on screen - never folded
            // into a popover, and just above the composer, same placement
            // `App.tsx:1278-1300` gives it ("that is where every editor puts
            // the thing asking for a decision"). Approvals before questions:
            // "the blocking thing goes first" - an approval is a run frozen
            // waiting for Josh, a question is not.
            Approvals {}
            Questions { bot_id: bot_id.clone(), bot_name: bot_name.clone() }
            Composer { bot_name: bot_name.clone(), disabled: *sending.read(), on_send }
        }
    }
}

/// The read-only pane: empty state, message rows with a divider whenever
/// the calendar day changes or the conversation paused 15+ minutes
/// (`App.tsx:1135-1161`), the streaming row, and a bottom anchor that
/// scrolls into view on growth.
///
/// Takes `Signal`s rather than owned values on purpose: the auto-scroll
/// effect below has to re-run every time the message list or the streaming
/// text grows, and Dioxus's `use_effect` only tracks dependencies it
/// actually reads through a `Signal` - a plain `Vec<Message>` prop changing
/// between renders would not re-trigger it.
#[component]
fn Thread(
    messages: Signal<Vec<Message>>,
    streaming: Signal<Option<String>>,
    bot_name: String,
    conversation_id: Signal<Option<String>>,
    #[props(default)] section_ids: Vec<String>,
) -> Element {
    let mut anchor = use_signal(|| None::<Rc<MountedData>>);

    use_effect(move || {
        // Reading both through their signals is what makes this effect
        // re-run on every append, not just on first mount.
        let _ = messages.read().len();
        let _ = streaming.read().as_ref().map(|s| s.len());
        if let Some(el) = anchor.read().clone() {
            spawn(async move {
                let _ = el.scroll_to(ScrollBehavior::Instant).await;
            });
        }
    });

    let msgs = messages.read();
    let live_text = streaming.read().clone();
    let now = now_iso();

    // A date/pause divider is a property of a PAIR of messages, so this is
    // computed once here rather than inside the `rsx!` loop below - same
    // reason the original computes `newDay`/`paused` as plain variables
    // before returning JSX.
    let rows: Vec<(bool, Message)> = msgs
        .iter()
        .enumerate()
        .map(|(i, m)| {
            let mark = match i.checked_sub(1).map(|p| &msgs[p]) {
                None => true,
                Some(prev) => {
                    day_key(&prev.created_at) != day_key(&m.created_at) || paused(prev, m)
                }
            };
            (mark, m.clone())
        })
        .collect();

    rsx! {
        div { class: "thread",
            if msgs.is_empty() && live_text.is_none() {
                p { class: "empty", "Ask {bot_name} something." }
            }
            for (mark , m) in rows {
                // `key` must sit on the first node of the block; some rows
                // have a divider ahead of the bubble and some do not, so
                // this wraps both in one keyed node. `display: contents`
                // takes the wrapper back out of layout, so `.thread`'s flex
                // algorithm still sees the divider and the row as its own
                // direct children (`.timemark`'s `align-self: center` and
                // `.row`'s `min-width: 0` both depend on that).
                div { key: "{m.id}", style: "display: contents;",
                    if mark {
                        div { class: "timemark", role: "separator",
                            "{format_day(&m.created_at, &now)} {format_time(&m.created_at)}"
                        }
                    }
                    Bubble { message: m, bot_name: bot_name.clone() }
                }
            }
            if let Some(text) = live_text {
                Bubble {
                    message: Message {
                        id: "local-stream".to_string(),
                        role: Role::Assistant,
                        content: text,
                        model: None,
                        error: None,
                        created_at: now_iso(),
                    },
                    bot_name: bot_name.clone(),
                    live: true,
                }
            }
            // Last thing inside `.thread`, above the scroll anchor, so it
            // rides the bottom the way a typing indicator does - ported
            // placement from `App.tsx:1276-1286`.
            WorkingBar { conversation_id, section_ids: section_ids.clone() }
            div { onmounted: move |evt| anchor.set(Some(evt.data())) }
        }
    }
}

/// ARCH-01: the confirm step in front of `api::archive_bot`. Reuses the same
/// `.modal-scrim`/`.modal`/`.modal-head`/`.modal-x`/`.rules-foot`/`.stg-btn`/
/// `.refusal` shell every other modal in this client already does (see
/// `goals_editor.rs`'s `GoalsModal` doc on why that reuse is deliberate) -
/// the one new class is `.danger` on the confirm button itself
/// (`assets/settings.css`'s `.stg-btn.danger`, the same red `.routine-acts
/// button.danger` already uses elsewhere, just not scoped to that
/// container).
#[component]
fn ArchiveConfirmModal(
    bot_id: String,
    bot_name: String,
    on_close: EventHandler<()>,
    on_archived: EventHandler<()>,
) -> Element {
    let mut busy = use_signal(|| false);
    let mut error = use_signal(|| None::<String>);

    let confirm_bot_id = bot_id.clone();
    let confirm = move |_| {
        if *busy.read() {
            return;
        }
        busy.set(true);
        error.set(None);
        let bot_id = confirm_bot_id.clone();
        spawn(async move {
            match api::archive_bot(&bot_id, true).await {
                Ok(_) => {
                    busy.set(false);
                    on_archived.call(());
                }
                Err(err) => {
                    busy.set(false);
                    error.set(Some(err));
                }
            }
        });
    };

    let is_busy = *busy.read();

    rsx! {
        div {
            class: "modal-scrim",
            role: "presentation",
            onclick: move |_| on_close.call(()),
            div {
                class: "modal archive-confirm-modal",
                onclick: move |evt| evt.stop_propagation(),
                role: "dialog",
                "aria-modal": "true",
                "aria-label": "Archive {bot_name}",
                div { class: "modal-head",
                    h2 { "Archive {bot_name}?" }
                    button {
                        class: "modal-x",
                        "aria-label": "Close",
                        onclick: move |_| on_close.call(()),
                        "×"
                    }
                }
                div { class: "modal-body",
                    p { class: "set-note",
                        "Leaves the roster, but nothing is deleted - conversations, memory and spend history stay put. Restore it later from Settings."
                    }
                    if let Some(err) = error.read().clone() {
                        div { class: "refusal",
                            b { "Could not archive." }
                            p { "{err}" }
                        }
                    }
                    div { class: "rules-foot",
                        button {
                            class: "stg-btn",
                            disabled: is_busy,
                            onclick: move |_| on_close.call(()),
                            "Cancel"
                        }
                        button {
                            class: "stg-btn danger",
                            disabled: is_busy,
                            onclick: confirm,
                            if is_busy { "Archiving…" } else { "Archive" }
                        }
                    }
                }
            }
        }
    }
}

/// 15+ minutes between two messages reads as "came back later" rather than
/// "still talking" - ported from `App.tsx:1151-1153`. S13a-01b: was
/// `js_sys::Date` directly; now `message_time::parse_epoch_ms`, the same
/// portable (wasm/native) parse this file's other date handling already
/// goes through, since a ms diff has no locale dependency to split on.
fn paused(prev: &Message, current: &Message) -> bool {
    let (Some(prev_ms), Some(current_ms)) = (
        parse_epoch_ms(&prev.created_at),
        parse_epoch_ms(&current.created_at),
    ) else {
        return false;
    };
    current_ms - prev_ms >= 15.0 * 60_000.0
}
