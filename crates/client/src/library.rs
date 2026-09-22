//! W4b Library shelf — port of `projects/bullpen-night/src/client/Library.tsx`.

use crate::api::{self, LibraryItem};
use crate::transport;
use dioxus::prelude::*;

fn format_bytes(n: i64) -> String {
    if n < 1024 {
        format!("{n} B")
    } else if n < 1024 * 1024 {
        format!("{} KB", n / 1024)
    } else {
        format!("{:.1} MB", n as f64 / (1024.0 * 1024.0))
    }
}

fn glyph_for(kind: &str) -> &'static str {
    match kind {
        "pdf" => "📄",
        "spreadsheet" => "📊",
        "deck" => "📽",
        "clip" => "🎬",
        "text" => "📝",
        "image" => "🖼",
        _ => "📁",
    }
}

#[component]
pub fn LibraryModal(on_close: EventHandler<()>) -> Element {
    let mut items = use_signal(Vec::<LibraryItem>::new);
    let mut loading = use_signal(|| true);
    let mut error = use_signal(|| None::<String>);
    let mut query = use_signal(String::new);
    let mut bot_filter = use_signal(|| "All".to_string());
    let mut kind_filter = use_signal(|| "All".to_string());
    let mut confirm_id = use_signal(|| None::<String>);
    let mut refresh = use_signal(|| 0u32);

    use_effect(move || {
        let _ = refresh();
        let q = query.read().clone();
        let bot = bot_filter.read().clone();
        let kind = kind_filter.read().clone();
        spawn(async move {
            loading.set(true);
            error.set(None);
            if !q.is_empty() {
                transport::sleep(200).await;
            }
            match api::fetch_library(&q, &bot, &kind).await {
                Ok(list) => items.set(list),
                Err(e) => error.set(Some(e)),
            }
            loading.set(false);
        });
    });

    let bot_options: Vec<(String, String)> = {
        let mut names = std::collections::BTreeMap::new();
        for item in items.read().iter() {
            if let (Some(id), Some(name)) = (&item.bot_id, &item.bot_name) {
                names.insert(id.clone(), name.clone());
            }
        }
        names.into_iter().collect()
    };

    let kind_options: Vec<String> = {
        let mut set = std::collections::BTreeSet::new();
        for item in items.read().iter() {
            set.insert(item.kind.clone());
        }
        set.into_iter().collect()
    };

    rsx! {
        div {
            class: "modal-scrim",
            onclick: move |_| on_close.call(()),
            div {
                class: "modal library-modal",
                onclick: move |evt| evt.stop_propagation(),
                "aria-label": "Library",
                header { class: "modal-head",
                    h2 { "Library" }
                    button {
                        r#type: "button",
                        class: "modal-close",
                        onclick: move |_| on_close.call(()),
                        "×"
                    }
                }
                div { class: "library-toolbar",
                    input {
                        r#type: "search",
                        placeholder: "Search files",
                        value: "{query}",
                        oninput: move |evt| {
                            query.set(evt.value());
                            refresh.set(refresh() + 1);
                        },
                    }
                    select {
                        value: "{bot_filter}",
                        onchange: move |evt| {
                            bot_filter.set(evt.value());
                            refresh.set(refresh() + 1);
                        },
                        option { value: "All", "All bots" }
                        for (id , name) in bot_options {
                            option { value: "{id}", "{name}" }
                        }
                    }
                    select {
                        value: "{kind_filter}",
                        onchange: move |evt| {
                            kind_filter.set(evt.value());
                            refresh.set(refresh() + 1);
                        },
                        option { value: "All", "All kinds" }
                        for k in kind_options {
                            option { value: "{k}", "{k}" }
                        }
                    }
                }
                if loading() {
                    p { class: "muted", "Loading…" }
                } else if let Some(err) = error.read().clone() {
                    p { class: "composer-error", "{err}" }
                } else if items.read().is_empty() {
                    p { class: "muted", "Nothing on the shelf yet." }
                } else {
                    div { class: "library-grid",
                        for item in items.read().clone() {
                            {
                                let row_id = item.id.clone();
                                let open_href = format!("/api/attachments/{}", row_id);
                                let confirm_target = row_id.clone();
                                let delete_id = row_id.clone();
                                let ask_delete_id = row_id.clone();
                                rsx! {
                                    div { class: "library-card", key: "{row_id}",
                                        div { class: "library-card-glyph", "{glyph_for(&item.kind)}" }
                                        div { class: "library-card-main",
                                            strong { "{item.name}" }
                                            span { class: "muted",
                                                "{format_bytes(item.bytes)} · {item.kind}"
                                            }
                                            if let Some(bot) = item.bot_name.clone() {
                                                span { class: "muted", "{bot}" }
                                            }
                                        }
                                        div { class: "library-card-actions",
                                            a {
                                                href: "{open_href}",
                                                target: "_blank",
                                                rel: "noopener",
                                                "Open"
                                            }
                                            if confirm_id.read().as_deref() == Some(confirm_target.as_str()) {
                                                button {
                                                    r#type: "button",
                                                    class: "stg-btn danger",
                                                    onclick: move |_| {
                                                        let id = delete_id.clone();
                                                        spawn(async move {
                                                            if api::delete_attachment(&id).await.is_ok() {
                                                                confirm_id.set(None);
                                                                refresh.set(refresh() + 1);
                                                            }
                                                        });
                                                    },
                                                    "Delete"
                                                }
                                                button {
                                                    r#type: "button",
                                                    class: "stg-btn",
                                                    onclick: move |_| confirm_id.set(None),
                                                    "Cancel"
                                                }
                                            } else {
                                                button {
                                                    r#type: "button",
                                                    class: "stg-btn danger",
                                                    onclick: move |_| confirm_id.set(Some(ask_delete_id.clone())),
                                                    "Delete…"
                                                }
                                            }
                                        }
                                    }
                                }
                            }
                        }
                    }
                }
            }
        }
    }
}
