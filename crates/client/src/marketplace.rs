//! Minimal marketplace UI — templates tab first (S10-06).

use crate::transport::Request;
use dioxus::prelude::*;
use serde::Deserialize;

#[derive(Clone, Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct MarketplaceCard {
    name: String,
    purpose: String,
    #[serde(default)]
    category: Option<String>,
    installed: bool,
}

#[derive(Clone, Debug, Deserialize)]
struct TemplatesResponse {
    cards: Vec<MarketplaceCard>,
}

#[component]
pub fn MarketplaceModal(
    on_close: EventHandler<()>,
    #[props(default)] on_installed: Option<EventHandler<()>>,
) -> Element {
    let mut cards = use_signal(Vec::<MarketplaceCard>::new);
    let mut loading = use_signal(|| true);
    let mut error = use_signal(|| None::<String>);
    let mut busy = use_signal(|| None::<String>);
    let mut note = use_signal(|| None::<String>);

    use_effect(move || {
        spawn(async move {
            loading.set(true);
            error.set(None);
            match Request::get("/api/marketplace/templates").send().await {
                Ok(resp) if resp.ok() => match resp.json::<TemplatesResponse>().await {
                    Ok(body) => cards.set(body.cards),
                    Err(e) => error.set(Some(e.to_string())),
                },
                Ok(resp) => error.set(Some(format!("templates -> {}", resp.status()))),
                Err(e) => error.set(Some(e.to_string())),
            }
            loading.set(false);
        });
    });

    let install = move |name: String| {
        spawn(async move {
            busy.set(Some(name.clone()));
            note.set(None);
            let body = serde_json::json!({ "kind": "bot", "name": name });
            let req = match Request::post("/api/marketplace/install").json(&body) {
                Ok(r) => r,
                Err(e) => {
                    note.set(Some(e));
                    busy.set(None);
                    return;
                }
            };
            match req.send().await {
                Ok(resp) if resp.status() == 201 => {
                    note.set(Some("Added. Find it on the rail.".to_string()));
                    if let Some(cb) = on_installed {
                        cb.call(());
                    }
                    if let Ok(list) = Request::get("/api/marketplace/templates").send().await
                        && list.ok()
                        && let Ok(body) = list.json::<TemplatesResponse>().await
                    {
                        cards.set(body.cards);
                    }
                }
                Ok(resp) => {
                    if let Ok(v) = resp.json::<serde_json::Value>().await {
                        note.set(Some(
                            v.get("error")
                                .and_then(|e| e.as_str())
                                .unwrap_or("could not install")
                                .to_string(),
                        ));
                    }
                }
                Err(e) => note.set(Some(e.to_string())),
            }
            busy.set(None);
        });
    };

    rsx! {
        div {
            class: "modal-scrim",
            role: "presentation",
            onclick: move |_| on_close.call(()),
            div {
                class: "modal stg-modal",
                onclick: move |evt| evt.stop_propagation(),
                role: "dialog",
                "aria-modal": "true",
                "aria-label": "Marketplace",
                div { class: "modal-head",
                    h2 { "Marketplace" }
                    button {
                        class: "modal-x",
                        "aria-label": "Close",
                        onclick: move |_| on_close.call(()),
                        "×"
                    }
                }
                div { class: "modal-body stg-body",
                    p { class: "stg-hint", "Starter templates ship with Bullpen. Plugins and the public bot directory arrive in the next slices." }
                    if loading() {
                        p { "Loading templates…" }
                    } else if let Some(err) = error() {
                        p { class: "stg-error", "{err}" }
                    } else if cards.read().is_empty() {
                        p { "No templates found. Set BULLPEN_TEMPLATES_DIR on the server." }
                    } else {
                        ul { class: "stg-list",
                            for card in cards.read().iter() {
                                li { class: "stg-list-row",
                                    div { class: "stg-list-main",
                                        strong { "{card.name}" }
                                        span { class: "stg-list-sub", "{card.purpose}" }
                                        if let Some(cat) = &card.category {
                                            span { class: "stg-chip", "{cat}" }
                                        }
                                    }
                                    if card.installed {
                                        span { class: "stg-muted", "On your rail" }
                                    } else {
                                        button {
                                            class: "stg-btn",
                                            disabled: busy.read().as_ref() == Some(&card.name),
                                            onclick: {
                                                let n = card.name.clone();
                                                move |_| install(n.clone())
                                            },
                                            if busy.read().as_ref() == Some(&card.name) { "Adding…" } else { "Add" }
                                        }
                                    }
                                }
                            }
                        }
                    }
                    if let Some(n) = note() {
                        p { class: "stg-note", "{n}" }
                    }
                }
            }
        }
    }
}
