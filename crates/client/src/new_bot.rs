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
//!
//! IMPORT-02 adds a second mode INSIDE this same modal ("New bot" / "Import
//! from a file") rather than a third `+` in the rail: the rail already has
//! two identical `+` buttons (group chat, new bot) separable only by
//! tooltip, and that is an open complaint from Josh, not a pattern to
//! extend. Both modes end the same way - a real `Bot` handed up through
//! `on_created` - so the roster refreshes and the modal closes exactly as
//! creating a bot already does; there is no second "a bot now exists" path.
//!
//! **Deliberately no file picker.** There is no `<input type="file">` /
//! `FileEngine` precedent anywhere in this crate, and a picker that has to
//! work identically across web-wasm, desktop-webview and mobile is its own
//! research task, not part of this slice - pasting into a textarea is the
//! whole interaction for now, so its absence here is a scope cut, not an
//! oversight. The `fileName` field still matters even with no real file
//! behind it: it is what the server's own format detection reads
//! (`AGENTS.md`, `SKILL.md`, or anything else - `crates/server/src/
//! import_open.rs::detect_format`), so it stays a plain text field Josh
//! fills in by hand.
//!
//! Import is a three-step flow, collapsed to two rendered states
//! (`ImportStage`): **Paste** (filename + textarea, "Preview") and
//! **Preview** (the server's own parse - name, purpose, format, model pin,
//! every warning - then "Import" or "Back"). The third step, "Done," is not
//! a rendered state at all: a successful `POST /api/import/open` calls
//! `on_created` directly, same as the "New bot" form's own `submit` does,
//! and the parent closes the modal from there. Nothing about the parse
//! happens client-side - see `api::preview_open_import`/`api::
//! import_open_bot`'s own doc comments for why (the server owns the format
//! rules, and the create route's response has no full bot row to read a
//! `Bot` back from directly).

use crate::api;
use crate::types::{Bot, OpenPreview};
use dioxus::prelude::*;

/// Which of `NewBotModal`'s two ways to make a bot is showing.
#[derive(Clone, Copy, PartialEq)]
enum Mode {
    New,
    Import,
}

/// IMPORT-02's two rendered states inside `Mode::Import` - see this
/// module's own top doc comment for why there is no third "Done" state.
/// `Preview` carries the server's own parse so "Back" (and a 400 on
/// Import) never loses it - the pasted text and filename live in their own
/// signals below and are untouched by moving between these two states.
#[derive(Clone, PartialEq)]
enum ImportStage {
    Paste,
    Preview(OpenPreview),
}

#[component]
pub fn NewBotModal(on_close: EventHandler<()>, on_created: EventHandler<Bot>) -> Element {
    let mut mode = use_signal(|| Mode::New);

    // ---- "New bot" mode - unchanged by IMPORT-02. ----
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

    // ---- IMPORT-02: "Import from a file" mode. ----
    let mut file_name = use_signal(|| "bot.md".to_string());
    let mut import_text = use_signal(String::new);
    let mut import_stage = use_signal(|| ImportStage::Paste);
    let mut import_busy = use_signal(|| false);
    let mut import_error = use_signal(|| None::<String>);

    let preview_disabled = import_text.read().trim().is_empty() || *import_busy.read();

    // Blank filename falls back to the server's own default rather than
    // sending an empty string - matches `crates/server/src/routes/
    // import.rs::read_import_body`'s own "absent or non-string -> bot.md",
    // so a client that clears the field sees the same behaviour a client
    // that never set it at all would.
    let effective_file_name = move || {
        let raw = file_name.read();
        if raw.trim().is_empty() {
            "bot.md".to_string()
        } else {
            raw.clone()
        }
    };

    let do_preview = move |_| {
        let fname = effective_file_name();
        let text = import_text.read().clone();
        import_busy.set(true);
        import_error.set(None);
        spawn(async move {
            let result = api::preview_open_import(&fname, &text).await;
            import_busy.set(false);
            match result {
                Ok(preview) => import_stage.set(ImportStage::Preview(preview)),
                Err(err) => import_error.set(Some(err)),
            }
        });
    };

    let do_import = move |_| {
        let fname = effective_file_name();
        let text = import_text.read().clone();
        import_busy.set(true);
        import_error.set(None);
        spawn(async move {
            let result = api::import_open_bot(&fname, &text).await;
            import_busy.set(false);
            match result {
                Ok(bot) => on_created.call(bot),
                // Deliberately does NOT reset `import_stage` - a 400 (a
                // duplicate name, a refused model pin) stays on Preview so
                // the pasted text is not lost, the ticket's own
                // requirement.
                Err(err) => import_error.set(Some(err)),
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
                    div { class: "room-picker-modebar",
                        button {
                            class: if *mode.read() == Mode::New { "thread-new is-on" } else { "thread-new" },
                            onclick: move |_| mode.set(Mode::New),
                            "New bot"
                        }
                        button {
                            class: if *mode.read() == Mode::Import { "thread-new is-on" } else { "thread-new" },
                            onclick: move |_| mode.set(Mode::Import),
                            "Import from a file"
                        }
                    }
                    if *mode.read() == Mode::New {
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
                    } else if matches!(*import_stage.read(), ImportStage::Paste) {
                        label { class: "room-picker-title",
                            "Filename"
                            input {
                                value: "{file_name}",
                                oninput: move |evt| file_name.set(evt.value()),
                                placeholder: "bot.md",
                                maxlength: 120,
                            }
                        }
                        p { class: "room-picker-hint",
                            "The filename decides the format \u{2014} AGENTS.md, SKILL.md, or anything else."
                        }
                        label { class: "room-picker-title",
                            "File contents"
                            textarea {
                                class: "room-picker-textarea",
                                value: "{import_text}",
                                "aria-label": "File contents",
                                oninput: move |evt| import_text.set(evt.value()),
                            }
                        }
                        if let Some(msg) = import_error.read().clone() {
                            for line in msg.split('\n') {
                                p { class: "notice-inline", "{line}" }
                            }
                        }
                        div { class: "room-picker-actions",
                            button { class: "thread-new", onclick: move |_| on_close.call(()), "Cancel" }
                            button {
                                class: "thread-new room-picker-create",
                                disabled: preview_disabled,
                                onclick: do_preview,
                                "Preview"
                            }
                        }
                    } else if let ImportStage::Preview(preview) = import_stage.read().clone() {
                        div { class: "room-picker-title",
                            "Name"
                            p { class: "room-picker-hint", "{preview.name}" }
                        }
                        div { class: "room-picker-title",
                            "What it is for"
                            p { class: "room-picker-hint", "{preview.purpose}" }
                        }
                        div { class: "room-picker-title",
                            "Format"
                            p { class: "room-picker-hint", "{preview.format}" }
                        }
                        if let Some(model) = preview.model.clone() {
                            div { class: "room-picker-title",
                                "Model"
                                p { class: "room-picker-hint", "{model}" }
                            }
                        }
                        for warning in &preview.warnings {
                            p { class: "room-picker-hint", "\u{26A0} {warning}" }
                        }
                        if let Some(msg) = import_error.read().clone() {
                            for line in msg.split('\n') {
                                p { class: "notice-inline", "{line}" }
                            }
                        }
                        div { class: "room-picker-actions",
                            button {
                                class: "thread-new",
                                disabled: *import_busy.read(),
                                onclick: move |_| {
                                    import_error.set(None);
                                    import_stage.set(ImportStage::Paste);
                                },
                                "Back"
                            }
                            button {
                                class: "thread-new room-picker-create",
                                disabled: *import_busy.read(),
                                onclick: do_import,
                                "Import"
                            }
                        }
                    }
                }
            }
        }
    }
}
