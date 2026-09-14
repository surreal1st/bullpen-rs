//! S3-05: a bot's memory pane, opened from `thread.rs`'s `pane-head` beside
//! "Permissions" (`permissions_editor.rs`'s `PermissionsModal` is the shape
//! this copies - same `.modal-scrim`/`.modal`/`.modal-head` shell). Core
//! editor (`PUT .../memory/core`), the recent log with a delete button per
//! entry, an "add note" row (`POST .../memory/notes`), a Projects section
//! (list/create/add-this-bot), and a Shared section (the shared log,
//! delete). Refetches on `ChangeKind::Memory`, same posture as
//! `approvals.rs`'s pane.
//!
//! 🔴 Server gap (S3-tickets.md S3-05's "if a server shape blocks you"):
//! `GET /api/bots/:id/memory` and `GET /api/memory/shared`
//! (`crates/server/src/routes/memory.rs`'s `LogEntryResponse`) send only
//! `id`/`content`/`source`/`createdAt` - no `kind` ("log" vs "note") and no
//! `expiresAt`, even though `memory_log` carries both columns
//! (`store::memory::note`'s INSERT) and `store::memory::recent_log`/
//! `search_log`/`recent_shared_log` simply never SELECT them back out. The
//! ticket's target ("log with kind badges", "a note's remaining TTL") is
//! built against `MemoryEntry`'s optional `kind`/`expires_at`
//! (`types.rs`) so it activates the moment a server ticket adds those two
//! columns to the SELECT and the two fields to the response - until then
//! every entry renders with no badge and no TTL text, which is the honest
//! state of what the API returns today.

use crate::api;
use crate::events::{ChangeKind, subscribe_events};
use crate::types::{MemoryEntry, MemoryView, ProjectSummary};
use dioxus::prelude::*;
use js_sys::Date;
use wasm_bindgen::JsValue;

/// Add-note TTL choices: label -> seconds. Default index is 1 ("1d"),
/// matching the `note` TOOL's own default (`tools/note.rs:25`) - S3-F-01c
/// (F1 in `S3-R.md`): the old default (index 0, "1h") meant a note typed in
/// the UI silently outlived its usefulness by the next prompt build's sweep
/// unless Josh caught the dropdown.
const TTL_CHOICES: &[(&str, u64)] = &[("1h", 3_600), ("1d", 86_400), ("1w", 604_800)];
const DEFAULT_TTL_INDEX: usize = 1;

fn parse_iso_ms(iso: &str) -> Option<f64> {
    let date = Date::new(&JsValue::from_str(iso));
    let t = date.get_time();
    if t.is_nan() { None } else { Some(t) }
}

/// "expires in 3h" for a note's `expires_at`. `None` when the entry carries
/// none - every entry today, see this module's doc on the server gap.
fn format_ttl(expires_at: &str) -> Option<String> {
    let target = parse_iso_ms(expires_at)?;
    let remaining_ms = target - Date::now();
    if remaining_ms <= 0.0 {
        return Some("expired".to_string());
    }
    let minutes = (remaining_ms / 60_000.0).round() as i64;
    if minutes < 60 {
        return Some(format!("expires in {}m", minutes.max(1)));
    }
    let hours = minutes / 60;
    if hours < 24 {
        return Some(format!("expires in {hours}h"));
    }
    Some(format!("expires in {}d", hours / 24))
}

/// `query` is the current search box value - empty means the plain recent
/// log, non-empty hits the server's `?q=` branch (S3-F-01c, F1: the search
/// box was missing entirely, so this branch of `GET .../memory` was dead
/// from the client's side even though the server already served it).
async fn load_view(
    bot_id: String,
    query: String,
    mut view: Signal<Option<MemoryView>>,
    mut core_text: Signal<String>,
    mut core_saved: Signal<String>,
) {
    if let Ok(v) = api::fetch_bot_memory_query(&bot_id, &query).await {
        // Never stomp an unsaved core edit sitting in the textarea when a
        // background change (another tab, a bot's own `remember`) fires
        // this refetch - only seed the textarea when it is not currently
        // dirty, same guard `settings.rs`'s `RulesSection` effectively gets
        // for free by only ever loading once.
        if *core_text.read() == *core_saved.read() {
            core_text.set(v.core.clone());
            core_saved.set(v.core.clone());
        }
        view.set(Some(v));
    }
}

async fn load_projects(mut projects: Signal<Vec<ProjectSummary>>) {
    if let Ok(list) = api::fetch_projects().await {
        projects.set(list);
    }
}

async fn load_shared(mut shared: Signal<Vec<MemoryEntry>>) {
    if let Ok(list) = api::fetch_shared_memory().await {
        shared.set(list);
    }
}

/// Plain functions, not closures, for the three per-row actions
/// (delete a log entry, add this bot to a project, delete a shared entry) -
/// each fires from inside a `for` loop that builds one button per row, and a
/// closure capturing owned state (a `String` id, here) is not `Copy`, so a
/// single shared closure cannot be handed to more than one row's button.
/// Same reasoning `approvals.rs`'s `fire_decide` gives for its own plain-fn
/// shape.
fn delete_memory_entry(
    view: Signal<Option<MemoryView>>,
    core_text: Signal<String>,
    core_saved: Signal<String>,
    search_query: Signal<String>,
    mut error: Signal<Option<String>>,
    bot_id: String,
    entry_id: String,
) {
    spawn(async move {
        if let Err(err) = api::delete_bot_memory_entry(&bot_id, &entry_id).await {
            error.set(Some(err));
        }
        let query = search_query.read().clone();
        load_view(bot_id, query, view, core_text, core_saved).await;
    });
}

fn add_bot_to_project(mut error: Signal<Option<String>>, project_id: String, bot_id: String) {
    spawn(async move {
        if let Err(err) = api::add_project_member(&project_id, &bot_id).await {
            error.set(Some(err));
        }
    });
}

fn delete_shared_entry(
    shared: Signal<Vec<MemoryEntry>>,
    mut error: Signal<Option<String>>,
    entry_id: String,
) {
    spawn(async move {
        if let Err(err) = api::delete_shared_memory_entry(&entry_id).await {
            error.set(Some(err));
        }
        load_shared(shared).await;
    });
}

/// The modal shell - `thread.rs`'s "Memory" button opens this over the
/// thread, same placement `permissions_editor.rs`'s `PermissionsModal`
/// already established for "Permissions" beside it.
#[component]
pub fn MemoryModal(bot_id: String, bot_name: String, on_close: EventHandler<()>) -> Element {
    rsx! {
        div {
            class: "modal-scrim",
            role: "presentation",
            onclick: move |_| on_close.call(()),
            div {
                class: "modal mem-modal",
                onclick: move |evt| evt.stop_propagation(),
                role: "dialog",
                "aria-modal": "true",
                "aria-label": "{bot_name} memory",
                div { class: "modal-head",
                    h2 { "{bot_name} \u{b7} Memory" }
                    button {
                        class: "modal-x",
                        "aria-label": "Close",
                        onclick: move |_| on_close.call(()),
                        "\u{d7}"
                    }
                }
                div { class: "modal-body",
                    MemoryEditor { bot_id: bot_id.clone() }
                }
            }
        }
    }
}

#[component]
fn MemoryEditor(bot_id: String) -> Element {
    let mut view = use_signal(|| None::<MemoryView>);
    let mut core_text = use_signal(String::new);
    let mut core_saved = use_signal(String::new);
    let mut core_busy = use_signal(|| false);
    let mut remember_content = use_signal(String::new);
    let mut remember_busy = use_signal(|| false);
    let mut note_content = use_signal(String::new);
    let mut note_ttl = use_signal(|| TTL_CHOICES[DEFAULT_TTL_INDEX].1);
    let mut note_busy = use_signal(|| false);
    let mut search_query = use_signal(String::new);
    let mut error = use_signal(|| None::<String>);
    let projects = use_signal(Vec::<ProjectSummary>::new);
    let mut new_project_name = use_signal(String::new);
    let mut project_busy = use_signal(|| false);
    let shared = use_signal(Vec::<MemoryEntry>::new);

    let load_bot_id = bot_id.clone();
    use_effect(move || {
        let bot_id = load_bot_id.clone();
        spawn(load_view(
            bot_id,
            String::new(),
            view,
            core_text,
            core_saved,
        ));
    });

    use_effect(move || {
        spawn(load_projects(projects));
    });

    use_effect(move || {
        spawn(load_shared(shared));
    });

    // 🔴 `wasm_bindgen_futures::spawn_local`, not `dioxus::prelude::spawn`:
    // this fires from `events.rs`'s bare `spawn_local(run())` loop, outside
    // any Dioxus scope - `approvals.rs`'s own subscription doc explains the
    // silent wasm abort `dioxus::prelude::spawn` hits there.
    let events_bot_id = bot_id.clone();
    let _events = use_signal(move || {
        let bot_id = events_bot_id.clone();
        subscribe_events(move |kind| {
            if kind == ChangeKind::Memory {
                let bot_id = bot_id.clone();
                let query = search_query.read().clone();
                wasm_bindgen_futures::spawn_local(load_view(
                    bot_id, query, view, core_text, core_saved,
                ));
                wasm_bindgen_futures::spawn_local(load_projects(projects));
                wasm_bindgen_futures::spawn_local(load_shared(shared));
            }
        })
    });

    let bot_id_core = bot_id.clone();
    let save_core = move |_| {
        let bot_id = bot_id_core.clone();
        core_busy.set(true);
        spawn(async move {
            let text = core_text.read().clone();
            match api::put_bot_memory_core(&bot_id, &text).await {
                Ok(status) => {
                    core_saved.set(status.core.clone());
                    core_text.set(status.core.clone());
                    // Refresh the token/budget line right away rather than
                    // waiting for the next `ChangeKind::Memory` refetch - the
                    // count just changed under Josh's own hands.
                    let current = view.read().clone();
                    if let Some(mut v) = current {
                        v.core = status.core;
                        v.tokens = status.tokens;
                        v.budget = status.budget;
                        v.over_budget = status.over_budget;
                        v.entries = status.entries;
                        view.set(Some(v));
                    }
                }
                Err(err) => error.set(Some(err)),
            }
            core_busy.set(false);
        });
    };

    // The durable path (S3-F-01c, F1) - `POST .../memory`, no TTL, sits
    // above the note form so "permanent" vs "expires" reads as a choice
    // made up front rather than a dropdown easy to miss (the TS original's
    // `.memory-add`, `MemoryEditor.tsx:161-172`).
    let bot_id_remember = bot_id.clone();
    let add_remember = move |evt: FormEvent| {
        evt.prevent_default();
        let content = remember_content.read().trim().to_string();
        if content.is_empty() {
            return;
        }
        let bot_id = bot_id_remember.clone();
        remember_busy.set(true);
        spawn(async move {
            match api::remember_entry(&bot_id, &content).await {
                Ok(_) => {
                    remember_content.set(String::new());
                    let query = search_query.read().clone();
                    load_view(bot_id, query, view, core_text, core_saved).await;
                }
                Err(err) => error.set(Some(err)),
            }
            remember_busy.set(false);
        });
    };

    let bot_id_search = bot_id.clone();
    let on_search_input = move |evt: FormEvent| {
        let query = evt.value();
        search_query.set(query.clone());
        let bot_id = bot_id_search.clone();
        spawn(load_view(bot_id, query, view, core_text, core_saved));
    };

    let bot_id_note = bot_id.clone();
    let add_note = move |evt: FormEvent| {
        evt.prevent_default();
        let content = note_content.read().trim().to_string();
        if content.is_empty() {
            return;
        }
        let bot_id = bot_id_note.clone();
        let ttl = *note_ttl.read();
        note_busy.set(true);
        spawn(async move {
            match api::post_bot_memory_note(&bot_id, &content, ttl).await {
                Ok(_) => {
                    note_content.set(String::new());
                    let query = search_query.read().clone();
                    load_view(bot_id, query, view, core_text, core_saved).await;
                }
                Err(err) => error.set(Some(err)),
            }
            note_busy.set(false);
        });
    };

    let create_project = move |evt: FormEvent| {
        evt.prevent_default();
        let name = new_project_name.read().trim().to_string();
        if name.is_empty() {
            return;
        }
        project_busy.set(true);
        spawn(async move {
            match api::create_project(&name).await {
                Ok(_) => {
                    new_project_name.set(String::new());
                    load_projects(projects).await;
                }
                Err(err) => error.set(Some(err)),
            }
            project_busy.set(false);
        });
    };

    let Some(status) = view.read().clone() else {
        return rsx! { p { class: "muted", "Loading memory\u{2026}" } };
    };
    let is_dirty = *core_text.read() != *core_saved.read();
    let is_core_busy = *core_busy.read();
    let is_remember_busy = *remember_busy.read();
    let is_note_busy = *note_busy.read();
    let is_project_busy = *project_busy.read();
    let is_searching = !search_query.read().trim().is_empty();

    // Precomputed per the codebase's own pattern for a `for` body that
    // needs more than one derived value per row (`permissions_editor.rs`'s
    // `rows`) - `rsx!`'s `for` body is a single element, not a block of
    // statements, so the badge label and TTL text are worked out here.
    let log_rows: Vec<(MemoryEntry, String, Option<String>)> = status
        .log
        .iter()
        .cloned()
        .map(|entry| {
            let kind = entry.kind.clone().unwrap_or_else(|| "log".to_string());
            let ttl = entry.expires_at.as_deref().and_then(format_ttl);
            (entry, kind, ttl)
        })
        .collect();

    rsx! {
        div { class: "mem",
            if let Some(err) = error.read().clone() {
                p { class: "composer-error", "{err}" }
            }

            div { class: "stg-sub",
                h4 { class: "stg-sub-h", "Core" }
                p { class: "set-note",
                    "Rides in every prompt to this bot, byte-identical between turns. "
                    "{status.tokens} / {status.budget} tokens"
                    if status.over_budget { " \u{2014} over budget" }
                }
                textarea {
                    class: "rules-box mem-core-box",
                    spellcheck: "false",
                    rows: "5",
                    "aria-label": "Memory core",
                    value: "{core_text}",
                    oninput: move |evt| core_text.set(evt.value()),
                }
                div { class: "rules-foot",
                    button {
                        class: "stg-btn",
                        disabled: is_core_busy || !is_dirty,
                        onclick: save_core,
                        if is_core_busy { "Saving\u{2026}" } else if is_dirty { "Save core" } else { "Saved" }
                    }
                }
            }

            div { class: "stg-sub",
                h4 { class: "stg-sub-h", "Log ({status.entries})" }
                div { class: "mem-search",
                    input {
                        value: "{search_query}",
                        placeholder: "Search this memory\u{2026}",
                        "aria-label": "Search memory",
                        oninput: on_search_input,
                    }
                }
                if log_rows.is_empty() {
                    p { class: "muted",
                        if is_searching { "Nothing matches." } else { "Nothing remembered yet." }
                    }
                } else {
                    ul { class: "mem-log",
                        for (entry , kind , ttl) in log_rows {
                            li { key: "{entry.id}", class: "mem-entry",
                                div { class: "mem-entry-top",
                                    span { class: "mem-badge mem-badge-{kind}", "{kind}" }
                                    if let Some(ttl) = ttl {
                                        span { class: "mem-ttl", "{ttl}" }
                                    }
                                    button {
                                        class: "mem-del",
                                        "aria-label": "Delete entry",
                                        onclick: {
                                            let entry_id = entry.id.clone();
                                            let bot_id = bot_id.clone();
                                            move |_| {
                                                delete_memory_entry(
                                                    view,
                                                    core_text,
                                                    core_saved,
                                                    search_query,
                                                    error,
                                                    bot_id.clone(),
                                                    entry_id.clone(),
                                                )
                                            }
                                        },
                                        "Delete"
                                    }
                                }
                                p { class: "mem-entry-content", "{entry.content}" }
                            }
                        }
                    }
                }

                div { class: "mem-add-group",
                    span { class: "mem-add-label", "Remember (permanent)" }
                    form {
                        class: "mem-add-note",
                        onsubmit: add_remember,
                        input {
                            value: "{remember_content}",
                            placeholder: "Tell it something to remember\u{2026}",
                            "aria-label": "Remember permanently",
                            oninput: move |evt| remember_content.set(evt.value()),
                        }
                        button {
                            r#type: "submit",
                            disabled: is_remember_busy || remember_content.read().trim().is_empty(),
                            if is_remember_busy { "Remembering\u{2026}" } else { "Remember" }
                        }
                    }
                }

                div { class: "mem-add-group",
                    span { class: "mem-add-label", "Note (expires)" }
                    form {
                        class: "mem-add-note",
                        onsubmit: add_note,
                        input {
                            value: "{note_content}",
                            placeholder: "Add a note\u{2026}",
                            "aria-label": "Note content",
                            oninput: move |evt| note_content.set(evt.value()),
                        }
                        select {
                            class: "mem-ttl-select",
                            "aria-label": "Note expiry",
                            value: "{note_ttl}",
                            onchange: move |evt| {
                                if let Ok(secs) = evt.value().parse::<u64>() {
                                    note_ttl.set(secs);
                                }
                            },
                            for (label , secs) in TTL_CHOICES.iter().copied() {
                                option { key: "{label}", value: "{secs}", "{label}" }
                            }
                        }
                        button {
                            r#type: "submit",
                            disabled: is_note_busy || note_content.read().trim().is_empty(),
                            "Add note"
                        }
                    }
                }
            }

            div { class: "stg-sub",
                h4 { class: "stg-sub-h", "Projects" }
                if projects.read().is_empty() {
                    p { class: "muted", "No projects yet." }
                } else {
                    ul { class: "mem-projects",
                        for project in projects.read().clone() {
                            li { key: "{project.id}", class: "mem-project-row",
                                span { "{project.name}" }
                                button {
                                    class: "mem-add-bot",
                                    onclick: {
                                        let project_id = project.id.clone();
                                        let bot_id = bot_id.clone();
                                        move |_| add_bot_to_project(error, project_id.clone(), bot_id.clone())
                                    },
                                    "Add this bot"
                                }
                            }
                        }
                    }
                }
                form {
                    class: "mem-add-note",
                    onsubmit: create_project,
                    input {
                        value: "{new_project_name}",
                        placeholder: "New project name\u{2026}",
                        "aria-label": "New project name",
                        oninput: move |evt| new_project_name.set(evt.value()),
                    }
                    button {
                        r#type: "submit",
                        disabled: is_project_busy || new_project_name.read().trim().is_empty(),
                        "Create"
                    }
                }
            }

            div { class: "stg-sub",
                h4 { class: "stg-sub-h", "Shared" }
                if shared.read().is_empty() {
                    p { class: "muted", "Nothing shared yet." }
                } else {
                    ul { class: "mem-log",
                        for entry in shared.read().clone() {
                            li { key: "{entry.id}", class: "mem-entry",
                                div { class: "mem-entry-top",
                                    span { class: "mem-badge mem-badge-shared", "shared" }
                                    button {
                                        class: "mem-del",
                                        "aria-label": "Delete shared entry",
                                        onclick: {
                                            let entry_id = entry.id.clone();
                                            move |_| delete_shared_entry(shared, error, entry_id.clone())
                                        },
                                        "Delete"
                                    }
                                }
                                p { class: "mem-entry-content", "{entry.content}" }
                            }
                        }
                    }
                }
            }
        }
    }
}
