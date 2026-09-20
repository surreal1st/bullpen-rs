//! Marketplace modal — plugins shelf, bot directory, and starter templates.

use crate::transport::Request;
use dioxus::prelude::*;
use serde::Deserialize;

#[derive(Clone, Copy, PartialEq, Eq)]
enum MarketplaceTab {
    Plugins,
    Bots,
    Templates,
}

impl MarketplaceTab {
    fn path(self) -> &'static str {
        match self {
            Self::Plugins => "/api/marketplace/plugins",
            Self::Bots => "/api/marketplace/bots",
            Self::Templates => "/api/marketplace/templates",
        }
    }

    fn label(self) -> &'static str {
        match self {
            Self::Plugins => "Plugins",
            Self::Bots => "Bots",
            Self::Templates => "Templates",
        }
    }

    fn search_placeholder(self) -> &'static str {
        match self {
            Self::Plugins => "Search plugins",
            Self::Bots => "Search bots",
            Self::Templates => "Search templates",
        }
    }
}

#[derive(Clone, Debug, PartialEq, Deserialize)]
#[serde(rename_all = "camelCase")]
struct MarketplaceCard {
    kind: String,
    name: String,
    purpose: String,
    #[serde(default)]
    category: Option<String>,
    #[serde(default)]
    integrations: Vec<String>,
    installed: bool,
    #[serde(default)]
    unavailable: Option<String>,
    #[serde(default)]
    open_access: Option<bool>,
    #[serde(default)]
    detail_url: Option<String>,
}

#[derive(Clone, Debug, Deserialize)]
struct ListResponse {
    #[serde(default)]
    ok: bool,
    cards: Vec<MarketplaceCard>,
    #[serde(default)]
    error: Option<String>,
}

#[component]
pub fn MarketplaceModal(
    on_close: EventHandler<()>,
    #[props(default)] on_installed: Option<EventHandler<()>>,
) -> Element {
    let mut tab = use_signal(|| MarketplaceTab::Plugins);
    let mut cards = use_signal(Vec::<MarketplaceCard>::new);
    let mut loading = use_signal(|| true);
    let mut error = use_signal(|| None::<String>);
    let mut busy = use_signal(|| None::<String>);
    let mut note = use_signal(|| None::<String>);
    let mut query = use_signal(String::new);
    let mut category = use_signal(|| "All".to_string());

    let reload =
        move |active: MarketplaceTab| {
            spawn(async move {
                loading.set(true);
                error.set(None);
                match Request::get(active.path()).send().await {
                    Ok(resp) if resp.ok() => match resp.json::<ListResponse>().await {
                        Ok(body) => {
                            if !body.ok {
                                error.set(body.error.or_else(|| {
                                    Some("that catalogue could not be read".to_string())
                                }));
                            }
                            cards.set(body.cards);
                        }
                        Err(e) => error.set(Some(e.to_string())),
                    },
                    Ok(resp) => error.set(Some(format!("{} -> {}", active.path(), resp.status()))),
                    Err(e) => error.set(Some(e.to_string())),
                }
                loading.set(false);
            });
        };

    use_effect(move || {
        let active = tab();
        reload(active);
    });

    let categories = use_memo(move || {
        let mut counts = std::collections::BTreeMap::<String, usize>::new();
        for card in cards.read().iter() {
            let cat = card.category.clone().unwrap_or_else(|| "Other".to_string());
            *counts.entry(cat).or_default() += 1;
        }
        let mut ordered: Vec<(String, usize)> = counts.into_iter().collect();
        ordered.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
        std::iter::once("All".to_string())
            .chain(ordered.into_iter().map(|(name, _)| name))
            .collect::<Vec<_>>()
    });

    let shown = use_memo(move || {
        let needle = query.read().trim().to_lowercase();
        let cat = category.read().clone();
        cards
            .read()
            .iter()
            .filter(|card| {
                cat == "All" || card.category.as_deref().unwrap_or("Other") == cat.as_str()
            })
            .filter(|card| {
                needle.is_empty()
                    || card.name.to_lowercase().contains(&needle)
                    || card.purpose.to_lowercase().contains(&needle)
                    || card
                        .integrations
                        .iter()
                        .any(|i| i.to_lowercase().contains(&needle))
            })
            .take(300)
            .cloned()
            .collect::<Vec<_>>()
    });

    let install = move |card: MarketplaceCard| {
        spawn(async move {
            busy.set(Some(card.name.clone()));
            note.set(None);
            let body = serde_json::json!({ "kind": card.kind, "name": card.name });
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
                    note.set(Some(format!("{} added. Find it on the rail.", card.name)));
                    if let Some(cb) = on_installed {
                        cb.call(());
                    }
                    reload(tab());
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

    let category_names = categories.read().clone();
    let visible_cards = shown.read().clone();

    rsx! {
        div {
            class: "modal-scrim",
            role: "presentation",
            onclick: move |_| on_close.call(()),
            div {
                class: "modal stg-modal mkt-modal",
                onclick: move |evt| evt.stop_propagation(),
                role: "dialog",
                "aria-modal": "true",
                "aria-label": "Marketplace",
                div { class: "modal-head mkt-head",
                    h2 { "Marketplace" }
                    div { class: "modal-tabs", role: "tablist",
                        for t in [MarketplaceTab::Plugins, MarketplaceTab::Bots, MarketplaceTab::Templates] {
                            button {
                                key: "{t.label()}",
                                role: "tab",
                                "aria-selected": tab() == t,
                                class: if tab() == t { "is-on" } else { "" },
                                onclick: move |_| {
                                    tab.set(t);
                                    category.set("All".to_string());
                                },
                                "{t.label()}"
                            }
                        }
                    }
                    button {
                        class: "modal-x",
                        "aria-label": "Close",
                        onclick: move |_| on_close.call(()),
                        "×"
                    }
                }
                div { class: "modal-body mkt-body",
                    input {
                        class: "modal-search",
                        value: "{query()}",
                        placeholder: tab().search_placeholder(),
                        "aria-label": "Search the marketplace",
                        oninput: move |evt| query.set(evt.value()),
                    }
                    div { class: "chips",
                        for name in category_names {
                            button {
                                key: "{name}",
                                class: if category() == name { "chip-filter is-on" } else { "chip-filter" },
                                onclick: move |_| category.set(name.clone()),
                                "{name}"
                            }
                        }
                    }
                    if let Some(n) = note() {
                        div { class: "modal-note",
                            span { "{n}" }
                            button { onclick: move |_| note.set(None), "Dismiss" }
                        }
                    }
                    if let Some(err) = error() {
                        div { class: "caution",
                            b { "The catalogue could not be read." }
                            p { "{err}" }
                        }
                    }
                    if loading() {
                        p { class: "muted", "Loading…" }
                    } else if visible_cards.is_empty() {
                        p { class: "muted", "Nothing matches that." }
                    } else {
                        div { class: "cards",
                            for card in visible_cards.iter() {
                                article { key: "{card.kind}-{card.name}",
                                    class: "card",
                                    div { class: "card-top",
                                        div { class: "card-what",
                                            b { "{card.name}" }
                                            span { "{card.purpose}" }
                                        }
                                        if card.installed {
                                            span { class: "card-added", "✓ Added" }
                                        } else if card.unavailable.is_some() {
                                            a {
                                                class: "card-out",
                                                href: card.detail_url.clone().unwrap_or_else(|| "#".to_string()),
                                                target: "_blank",
                                                rel: "noopener noreferrer",
                                                title: card.unavailable.clone().unwrap_or_default(),
                                                "View"
                                            }
                                        } else {
                                            button {
                                                class: "card-add",
                                                disabled: busy.read().is_some(),
                                                onclick: {
                                                    let c = card.clone();
                                                    move |_| install(c.clone())
                                                },
                                                if busy() == Some(card.name.clone()) { "…" } else if tab() == MarketplaceTab::Templates { "Use this" } else { "Add" }
                                            }
                                        }
                                    }
                                    if !card.integrations.is_empty() || card.open_access == Some(true) {
                                        div { class: "card-tags",
                                            if card.open_access == Some(true) {
                                                span { class: "tag-open", "No sign-in needed" }
                                            }
                                            for tag in card.integrations.iter().take(4) {
                                                span { key: "{tag}", "{tag}" }
                                            }
                                        }
                                    }
                                }
                            }
                        }
                    }
                    if tab() == MarketplaceTab::Bots {
                        p { class: "modal-foot muted",
                            "Bots come from botdirectory.ai. Entries marked View are share-only."
                        }
                    }
                    if tab() == MarketplaceTab::Plugins {
                        p { class: "modal-foot muted",
                            "Adding a plugin does not switch it on. Authorize once in Settings, then enable per bot."
                        }
                    }
                    if tab() == MarketplaceTab::Templates {
                        p { class: "modal-foot muted",
                            "Starter templates create a copy on your rail with the cheap default model."
                        }
                    }
                }
            }
        }
    }
}
