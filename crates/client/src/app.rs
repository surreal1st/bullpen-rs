//! App shell: fetches `/api/roster` on mount and renders the rail from it.
//! S1-07a wiring: selecting a bot in the rail opens its conversation
//! (`ChatPane`) beside the rail.

use crate::rail::Rail;
use crate::thread::ChatPane;
use crate::types::Roster;
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

#[component]
pub fn App() -> Element {
    let mut roster = use_signal::<Option<Result<Roster, String>>>(|| None);
    let mut selected = use_signal::<Option<String>>(|| None);

    use_effect(move || {
        spawn(async move {
            let result = fetch_roster().await;
            roster.set(Some(result));
        });
    });

    let body = match roster.read().as_ref() {
        None => rsx! {
            div { class: "roster", style: "padding: 1rem; color: var(--muted);", "Loading roster…" }
        },
        Some(Ok(data)) => {
            let selected_id = selected.read().clone();
            let selected_bot = selected_id
                .as_ref()
                .and_then(|id| data.bots.iter().find(|b| &b.id == id).cloned());
            rsx! {
                div { class: "shell",
                    Rail {
                        sections: data.sections.clone(),
                        bots: data.bots.clone(),
                        selected: selected_id.clone(),
                        on_select: move |id| selected.set(Some(id)),
                    }
                    if let Some(bot) = selected_bot {
                        ChatPane { key: "{bot.id}", bot_id: bot.id.clone(), bot_name: bot.name.clone() }
                    } else {
                        div { class: "pane pane-empty", "Pick a bot to start talking." }
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
