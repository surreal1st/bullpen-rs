//! App shell: fetches `/api/roster` on mount and renders the rail from it.

use crate::rail::Rail;
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
        Some(Ok(data)) => rsx! {
            Rail { sections: data.sections.clone(), bots: data.bots.clone() }
        },
        Some(Err(err)) => rsx! {
            div { class: "roster", style: "padding: 1rem; color: var(--danger);", "Roster failed to load: {err}" }
        },
    };

    rsx! {
        document::Stylesheet { href: asset!("/assets/rail.css") }
        {body}
    }
}
