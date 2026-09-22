//! Port of `SettingsModal.tsx` + the slice of `General.tsx` this ticket asks
//! for: the Models section (default/mid/tier1/premium pickers), the Routing
//! card (enabled toggle, rule text, the "Last 20 routings" log) and the
//! house-rules textarea. `Account`/`Appearance`/`System`/`Bot > ModelsShown`/
//! `AutoReviewSlot`/`SecondOpinion`/`Alerts` are explicitly SKIPPED per the
//! ticket - only a single "General" nav item is rendered (the TS original's
//! Computer/Billing/Updates tabs, and its owner-vs-member `isOwner()` gate,
//! have no bullpen-rs equivalent yet).

use crate::api;
use crate::connectors_card::ConnectorsCard;
use crate::marketplace::MarketplaceModal;
use crate::message_time::format_time;
use crate::model_chip::short_model;
use crate::slack_card::SlackCard;
use crate::transport::sleep;
use crate::types::{
    AutoReviewLogEntry, AutoReviewState, CatalogEntry, InviteSummary, PeopleUser, RoutingState,
    Section, Skill, SkillSummary, SpendView,
};
use dioxus::prelude::*;
use std::cell::Cell;
use std::collections::HashMap;
use std::rc::Rc;

#[component]
pub fn SettingsModal(
    #[props(default = true)] is_owner: bool,
    on_close: EventHandler<()>,
    // ARCH-01: fires once a restore in `ArchivedBotsSection` below succeeds,
    // so `app.rs` can refresh the roster the same way `thread.rs`'s
    // `on_archived` already does for the other direction - this modal has
    // no roster of its own either, only the ability to ask for a refresh.
    // RAIL-02's `SectionsSection` below reuses the same handler after a
    // create/rename/delete, for the same reason.
    #[props(default)] on_restored: Option<EventHandler<()>>,
    // RAIL-02: the live section list, same source `rail.rs` itself draws
    // from (`app.rs`'s `data.sections`) - this modal has no roster fetch of
    // its own (see `on_restored`'s doc above), so the list has to arrive as
    // a prop rather than a second `/api/roster` call.
    #[props(default)] sections: Vec<Section>,
) -> Element {
    let mut marketplace_open = use_signal(|| false);
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
                "aria-label": "Settings",
                div { class: "modal-head",
                    h2 { "Settings" }
                    button {
                        class: "modal-x",
                        "aria-label": "Close",
                        onclick: move |_| on_close.call(()),
                        "×"
                    }
                }
                div { class: "modal-body stg-body",
                    nav { class: "stg-nav", "aria-label": "Settings sections",
                        button { class: "stg-nav-item is-on", "aria-current": "true", "General" }
                    }
                    div { class: "stg-panel",
                        GeneralSettings {
                            is_owner,
                            on_restored,
                            sections,
                            on_open_marketplace: move |_| marketplace_open.set(true),
                        }
                    }
                }
            }
        }
        if marketplace_open() {
            MarketplaceModal {
                on_close: move |_| marketplace_open.set(false),
                on_installed: on_restored,
            }
        }
    }
}

#[component]
fn GeneralSettings(
    #[props(default = true)] is_owner: bool,
    #[props(default)] on_restored: Option<EventHandler<()>>,
    #[props(default)] sections: Vec<Section>,
    #[props(default)] on_open_marketplace: Option<EventHandler<()>>,
) -> Element {
    rsx! {
        if is_owner {
            section { class: "stg-group",
                h3 { class: "stg-group-h", "People" }
                div { class: "stg-card",
                    PeopleSection {}
                }
            }
        }
        section { class: "stg-group",
            h3 { class: "stg-group-h", "Bot" }
            div { class: "stg-card stg-card-loose",
                ModelsSection {}
                RulesSection {}
                RoutingSection {}
                AutoReviewSection {}
                SkillsSection {}
                SectionsManagerSection { sections, on_changed: on_restored }
                ArchivedBotsSection { on_restored }
                HiddenBotsSection { on_restored }
            }
        }
        section { class: "stg-group",
            h3 { class: "stg-group-h", "Memory" }
            div { class: "stg-card stg-card-loose",
                SharedMemorySection {}
            }
        }
        if is_owner {
            section { class: "stg-group",
                h3 { class: "stg-group-h", "Spend" }
                div { class: "stg-card stg-card-loose",
                    SpendSection {}
                }
            }
        }
        section { class: "stg-group",
            h3 { class: "stg-group-h", "Connectors" }
            div { class: "stg-card stg-card-loose",
                ConnectorsCard {}
                SlackCard {}
                div { class: "stg-row",
                    p { class: "stg-hint", "Install starter bots and connector packs from the marketplace." }
                    button {
                        class: "stg-btn",
                        onclick: move |_| {
                            if let Some(cb) = on_open_marketplace {
                                cb.call(());
                            }
                        },
                        "Open marketplace"
                    }
                }
            }
        }
    }
}

/* -------------------------------------------------------------------- Models */

struct Rung {
    kind: &'static str,
    title: &'static str,
    blurb: &'static str,
}

const RUNGS: &[Rung] = &[
    Rung {
        kind: "code",
        title: "Code",
        blurb: "Writing, running or debugging software. Reached when a bot says the problem is code.",
    },
    Rung {
        kind: "reason",
        title: "Reason",
        blurb: "Stuck on logic or a plan, not code. Reached when a bot says the problem is reasoning.",
    },
    Rung {
        kind: "vision",
        title: "Vision",
        blurb: "An image the current model cannot read. Reached when a bot says the problem is vision.",
    },
];

/// Port of `General.tsx::Models`: the default/tier1/mid/premium pickers.
/// Every value here is a concrete model id, never a pin's "unset" state -
/// `ModelPickerField` below is written to match (see its own doc comment).
#[component]
fn ModelsSection() -> Element {
    let mut fallback = use_signal(|| None::<String>);
    let mut premium = use_signal(|| None::<String>);
    let mut mid = use_signal(|| None::<String>);
    let mut tier1 = use_signal(|| None::<crate::types::Tier1Models>);
    let mut error = use_signal(|| None::<String>);

    use_effect(move || {
        spawn(async move {
            if let Ok(m) = api::fetch_default_model().await {
                fallback.set(Some(m));
            }
        });
        spawn(async move {
            if let Ok(m) = api::fetch_premium_model().await {
                premium.set(Some(m));
            }
        });
        spawn(async move {
            if let Ok(m) = api::fetch_mid_model().await {
                mid.set(Some(m));
            }
        });
        spawn(async move {
            if let Ok(m) = api::fetch_tier1_models().await {
                tier1.set(Some(m));
            }
        });
    });

    let on_default = move |m: String| {
        spawn(async move {
            match api::put_default_model(&m).await {
                Ok(saved) => fallback.set(Some(saved)),
                Err(err) => error.set(Some(err)),
            }
        });
    };
    let on_mid = move |m: String| {
        spawn(async move {
            match api::put_mid_model(&m).await {
                Ok(saved) => mid.set(Some(saved)),
                Err(err) => error.set(Some(err)),
            }
        });
    };
    let on_premium = move |m: String| {
        spawn(async move {
            match api::put_premium_model(&m).await {
                Ok(saved) => premium.set(Some(saved)),
                Err(err) => error.set(Some(err)),
            }
        });
    };

    rsx! {
        div { class: "stg-sub",
            h4 { class: "stg-sub-h", "Models" }

            if let Some(err) = error.read().clone() {
                div { class: "refusal",
                    b { "Refused." }
                    p { "{err}" }
                }
            }

            div { class: "field",
                span { "Default, for any bot without a pin" }
                small { "What every bot and every routine actually run, absent a pin." }
                if let Some(m) = fallback.read().clone() {
                    ModelPickerField { value: m, on_change: on_default }
                }
            }

            div { class: "rungs",
                div { class: "rungs-h",
                    span { "When a bot gets stuck" }
                    small { "A stuck bot moves up one rung, never two, to the model for the kind of stuck it says it is." }
                }
                if let Some(t1) = tier1.read().clone() {
                    for rung in RUNGS.iter() {
                        div { class: "field", key: "{rung.kind}",
                            span { "{rung.title}" }
                            small { "{rung.blurb}" }
                            ModelPickerField {
                                value: t1.get(rung.kind).to_string(),
                                on_change: {
                                    let kind = rung.kind;
                                    move |m: String| {
                                        spawn(async move {
                                            match api::put_tier1_model(kind, &m).await {
                                                Ok(saved) => {
                                                    let mut next = tier1.read().clone().unwrap_or_default();
                                                    match kind {
                                                        "code" => next.code = saved,
                                                        "reason" => next.reason = saved,
                                                        _ => next.vision = saved,
                                                    }
                                                    tier1.set(Some(next));
                                                }
                                                Err(err) => error.set(Some(err)),
                                            }
                                        });
                                    }
                                },
                            }
                        }
                    }
                }
            }

            div { class: "field",
                span { "Still stuck" }
                small { "Where a specialist that already failed goes next." }
                if let Some(m) = mid.read().clone() {
                    ModelPickerField { value: m, on_change: on_mid }
                }
            }

            div { class: "field",
                span { "Last resort" }
                small { "The strongest model, and the end of the ladder. Never on a timer." }
                if let Some(m) = premium.read().clone() {
                    ModelPickerField { value: m, on_change: on_premium }
                }
            }
        }
    }
}

fn fmt_price(n: f64) -> String {
    if n == 0.0 {
        return "0".to_string();
    }
    if n < 1.0 {
        let s = format!("{n:.2}");
        return s.trim_end_matches('0').trim_end_matches('.').to_string();
    }
    format!("{n:.2}")
}

/// Port of `ModelPicker.tsx`, narrowed to how every call site in this file
/// actually uses it: `value` is always a concrete model id (the settings
/// this ticket ports never carry the nullable "no pin yet" state that
/// `ModelChip`'s own picker does), so the TS original's "platform default"
/// tag and "Use the default" clear button - both dead code when `value` can
/// never differ from `defaultModel` - are dropped rather than ported unused.
#[component]
fn ModelPickerField(value: String, on_change: EventHandler<String>) -> Element {
    let mut open = use_signal(|| false);
    let mut query = use_signal(String::new);
    let mut show_all = use_signal(|| false);
    let mut models = use_signal(Vec::<CatalogEntry>::new);
    let mut total = use_signal(|| 0usize);
    let mut mainstream_total = use_signal(|| 0usize);
    let mut loading = use_signal(|| true);
    let mut load_error = use_signal(|| None::<String>);
    // B-F10 (`S2-R-standards.md`): a `Signal<u32>` here made this effect
    // read `generation` and then write it in the same body, so writing it
    // re-triggered the very effect doing the writing (a debounced keystroke
    // fired every 180ms forever instead of once). A plain `Rc<Cell<u32>>`
    // via `use_hook` holds the same "which fetch is still wanted" counter
    // without being a reactive dependency at all - bumping it does not
    // schedule a re-run, only `open`/`query`/`show_all` do.
    let generation = use_hook(|| Rc::new(Cell::new(0u32)));

    use_effect(move || {
        if !*open.read() {
            return;
        }
        let q = query.read().clone();
        let all = *show_all.read();
        let my_gen = generation.get() + 1;
        generation.set(my_gen);
        loading.set(true);
        load_error.set(None);
        let generation = generation.clone();
        spawn(async move {
            sleep(180).await;
            if generation.get() != my_gen {
                return;
            }
            match api::fetch_models(&q, all).await {
                Ok(body) => {
                    if generation.get() != my_gen {
                        return;
                    }
                    models.set(body.models);
                    total.set(body.total);
                    mainstream_total.set(body.mainstream_total);
                    loading.set(false);
                }
                Err(err) => {
                    if generation.get() != my_gen {
                        return;
                    }
                    load_error.set(Some(err));
                    loading.set(false);
                }
            }
        });
    });

    let is_open = *open.read();
    let is_loading = *loading.read();
    let scoped_total = if *show_all.read() {
        *total.read()
    } else {
        *mainstream_total.read()
    };
    let shown = models.read();
    let remaining = scoped_total.saturating_sub(shown.len());
    let picker_class = if is_open { "picker is-open" } else { "picker" };

    rsx! {
        div { class: "{picker_class}",
            div { class: "picker-current",
                span { class: "mono", "{value}" }
                button {
                    class: "picker-toggle",
                    "aria-expanded": "{is_open}",
                    onclick: move |_| {
                        let next = !*open.read();
                        open.set(next);
                    },
                    if is_open { "Done" } else { "Change" }
                }
            }

            if is_open {
                input {
                    class: "picker-search",
                    value: "{query}",
                    placeholder: "Search models",
                    "aria-label": "Search models",
                    oninput: move |evt| query.set(evt.value()),
                }

                if let Some(err) = load_error.read().clone() {
                    div { class: "picker-error",
                        b { "Could not load the model list." }
                        pre { class: "mono", "{err}" }
                    }
                }

                div { class: "picker-list",
                    if is_loading {
                        p { class: "muted", "Loading…" }
                    }
                    if !is_loading {
                        for m in shown.iter() {
                            button {
                                key: "{m.id}",
                                class: if m.id == value { "picker-row is-on" } else if m.batch_only { "picker-row is-batch" } else { "picker-row" },
                                disabled: m.batch_only,
                                title: if m.batch_only { "Batch-only. Every normal request fails with a 404.".to_string() } else { m.id.clone() },
                                onclick: {
                                    let id = m.id.clone();
                                    move |_| on_change.call(id.clone())
                                },
                                span { class: "picker-row-name",
                                    "{short_model(&m.id)}"
                                    span { class: "picker-row-vendor", "{m.id.split('/').next().unwrap_or_default()}" }
                                }
                                span { class: "picker-row-facts",
                                    span { class: "mono", "${fmt_price(m.in_per_m)} / ${fmt_price(m.out_per_m)}" }
                                    if m.batch_only {
                                        span { class: "picker-batch", "batch only" }
                                    }
                                    if !m.batch_only && *show_all.read() && m.mainstream {
                                        span { class: "picker-mainstream", "mainstream" }
                                    }
                                    if !m.supports_tools && !m.batch_only {
                                        span { class: "picker-no-tools", "no tools" }
                                    }
                                }
                            }
                        }
                        if remaining > 0 {
                            p { class: "muted picker-more", "{remaining} more. Narrow the search." }
                        }
                    }
                }

                if !is_loading {
                    p { class: "picker-scope",
                        if *show_all.read() {
                            button { class: "picker-scope-link", onclick: move |_| show_all.set(false), "Mainstream only" }
                        } else {
                            button { class: "picker-scope-link", onclick: move |_| show_all.set(true), "Show all {total}" }
                        }
                    }
                }
                p { class: "picker-legend", "Price is dollars per million tokens, in / out." }
            }
        }
    }
}

/* --------------------------------------------------------------------- Rules */

#[component]
fn RulesSection() -> Element {
    let mut rules = use_signal(String::new);
    let mut saved = use_signal(String::new);
    let mut busy = use_signal(|| false);

    use_effect(move || {
        spawn(async move {
            if let Ok(r) = api::fetch_rules().await {
                rules.set(r.clone());
                saved.set(r);
            }
        });
    });

    let save = move |_| {
        busy.set(true);
        spawn(async move {
            let text = rules.read().clone();
            if let Ok(r) = api::put_rules(&text).await {
                rules.set(r.clone());
                saved.set(r);
            }
            busy.set(false);
        });
    };

    let is_busy = *busy.read();
    let is_dirty = *rules.read() != *saved.read();

    rsx! {
        div { class: "stg-sub",
            h4 { class: "stg-sub-h", "Rules for every bot" }
            p { class: "set-note", "Added to every bot\u{2019}s instructions, on every run. Keep it short: this rides in each request." }
            textarea {
                class: "rules-box",
                spellcheck: "false",
                rows: "8",
                value: "{rules}",
                oninput: move |evt| rules.set(evt.value()),
            }
            div { class: "rules-foot",
                button {
                    class: "stg-btn",
                    disabled: is_busy || !is_dirty,
                    onclick: save,
                    if is_busy { "Saving…" } else if is_dirty { "Save rules" } else { "Saved" }
                }
                span { class: "rules-warn", "A rule asks; it does not enforce. What actually withholds an invented answer runs on the server." }
            }
        }
    }
}

/* ---------------------------------------------------------- S3-05: shared memory */

/// The shared-core textarea card (`GET`/`PUT /api/shared-core`) - rides in
/// every bot's prompt the same way a bot's own core does
/// (`memory_editor.rs`'s "Core" section), just not scoped to one bot. Same
/// dirty-tracking save button shape as `RulesSection` above.
#[component]
fn SharedMemorySection() -> Element {
    let mut core = use_signal(String::new);
    let mut saved = use_signal(String::new);
    let mut busy = use_signal(|| false);

    use_effect(move || {
        spawn(async move {
            if let Ok(c) = api::fetch_shared_core().await {
                core.set(c.clone());
                saved.set(c);
            }
        });
    });

    let save = move |_| {
        busy.set(true);
        spawn(async move {
            let text = core.read().clone();
            if let Ok(c) = api::put_shared_core(&text).await {
                core.set(c.clone());
                saved.set(c);
            }
            busy.set(false);
        });
    };

    let is_busy = *busy.read();
    let is_dirty = *core.read() != *saved.read();

    rsx! {
        div { class: "stg-sub",
            h4 { class: "stg-sub-h", "Shared memory" }
            p { class: "set-note", "Rides in every bot's prompt, on every run - not one bot's own core." }
            textarea {
                class: "rules-box",
                spellcheck: "false",
                rows: "6",
                "aria-label": "Shared memory core",
                value: "{core}",
                oninput: move |evt| core.set(evt.value()),
            }
            div { class: "rules-foot",
                button {
                    class: "stg-btn",
                    disabled: is_busy || !is_dirty,
                    onclick: save,
                    if is_busy { "Saving…" } else if is_dirty { "Save" } else { "Saved" }
                }
            }
        }
    }
}

/* ------------------------------------------------------------------- People */

fn people_initials(name: &str) -> String {
    let parts: Vec<&str> = name.split_whitespace().filter(|p| !p.is_empty()).collect();
    let first = parts.first().and_then(|p| p.chars().next()).unwrap_or('?');
    let second = if parts.len() > 1 {
        parts
            .last()
            .and_then(|p| p.chars().next())
            .unwrap_or_default()
    } else {
        '\0'
    };
    if second == '\0' {
        first.to_uppercase().to_string()
    } else {
        format!("{}{}", first.to_uppercase(), second.to_uppercase())
    }
}

/// S11-03: owner-only People card — port of `People.tsx`.
#[component]
fn PeopleSection() -> Element {
    let mut users = use_signal(Vec::<PeopleUser>::new);
    let mut invites = use_signal(Vec::<InviteSummary>::new);
    let mut minted = use_signal(|| None::<String>);
    let mut busy = use_signal(|| false);
    let mut problem = use_signal(|| None::<String>);
    let mut refresh = use_signal(|| 0u32);
    let mut ceiling_draft = use_signal(HashMap::<String, String>::new);

    use_effect(move || {
        let _ = *refresh.read();
        spawn(async move {
            if let Ok(body) = api::fetch_users().await {
                users.set(body.users.clone());
                invites.set(body.invites);
                let mut drafts = HashMap::new();
                for person in &body.users {
                    if person.role != "owner" {
                        drafts.insert(
                            person.id.clone(),
                            person
                                .ceiling_usd
                                .map(|v| v.to_string())
                                .unwrap_or_default(),
                        );
                    }
                }
                ceiling_draft.set(drafts);
            }
        });
    });

    rsx! {
        ul { class: "ppl-list",
            for person in users.read().iter().cloned() {
                li {
                    key: "{person.id}",
                    class: if person.archived_at.is_none() { "ppl-row" } else { "ppl-row is-gone" },
                    span { class: "ppl-mark", "aria-hidden": "true", "{people_initials(&person.name)}" }
                    span { class: "ppl-who",
                        b { "{person.name}" }
                        small {
                            if person.role == "owner" {
                                "Owns this Bullpen"
                            } else if person.archived_at.is_none() {
                                "Member"
                            } else {
                                "Member · removed"
                            }
                        }
                    }
                    if person.role == "owner" {
                        span { class: "ppl-ceiling-note", "Uses the box ceiling" }
                    } else {
                        label { class: "ppl-ceiling",
                            span { class: "ppl-ceiling-lbl", "Ceiling" }
                            span { class: "ppl-dollar", "$" }
                            input {
                                r#type: "number",
                                min: "0",
                                step: "1",
                                placeholder: "box",
                                "aria-label": format!("Monthly ceiling for {}", person.name),
                                value: ceiling_draft.read().get(&person.id).cloned().unwrap_or_default(),
                                oninput: {
                                    let id = person.id.clone();
                                    move |evt| {
                                        let mut next = ceiling_draft.read().clone();
                                        next.insert(id.clone(), evt.value());
                                        ceiling_draft.set(next);
                                    }
                                },
                                onblur: {
                                    let id = person.id.clone();
                                    move |_| {
                                        let id = id.clone();
                                        let raw = ceiling_draft.read().get(&id).cloned().unwrap_or_default();
                                        spawn(async move {
                                            let trimmed = raw.trim();
                                            let ceiling = if trimmed.is_empty() {
                                                None
                                            } else if let Ok(n) = trimmed.parse::<f64>() {
                                                if n.is_finite() && n >= 0.0 {
                                                    Some(n)
                                                } else {
                                                    return;
                                                }
                                            } else {
                                                return;
                                            };
                                            let _ = api::set_user_ceiling(&id, ceiling).await;
                                        });
                                    }
                                },
                            }
                            span { class: "ppl-per", "/mo" }
                        }
                    }
                    if person.role == "member" && person.archived_at.is_none() {
                        button {
                            class: "stg-btn danger",
                            onclick: move |_| {
                                let id = person.id.clone();
                                spawn(async move {
                                    let _ = api::archive_user(&id).await;
                                    refresh.set(refresh() + 1);
                                });
                            },
                            "Remove"
                        }
                    }
                }
            }
        }
        div { class: "stg-row",
            span {
                "Invite someone"
                small { class: "ppl-hint", "One person, one use, good for 7 days." }
            }
            button {
                class: "stg-chip",
                disabled: *busy.read(),
                onclick: move |_| {
                    busy.set(true);
                    problem.set(None);
                    spawn(async move {
                        match api::mint_user_invite().await {
                            Ok(body) => {
                                let link = {
                                    #[cfg(target_arch = "wasm32")]
                                    {
                                        web_sys::window()
                                            .and_then(|w| w.location().origin().ok())
                                            .map(|origin| {
                                                format!("{origin}/#invite={}", body.invite.token)
                                            })
                                            .unwrap_or_else(|| body.url)
                                    }
                                    #[cfg(not(target_arch = "wasm32"))]
                                    {
                                        body.url
                                    }
                                };
                                minted.set(Some(link));
                                refresh.set(refresh() + 1);
                            }
                            Err(err) => problem.set(Some(err)),
                        }
                        busy.set(false);
                    });
                },
                if *busy.read() { "Making a link…" } else { "Invite" }
            }
        }
        if let Some(link) = minted.read().clone() {
            div { class: "ppl-link",
                code { "{link}" }
                button {
                    class: "stg-chip",
                    onclick: move |_| {
                        let text = link.clone();
                        spawn(async move {
                            #[cfg(target_arch = "wasm32")]
                            if let Some(window) = web_sys::window() {
                                let _ = window.navigator().clipboard().write_text(&text);
                            }
                            #[cfg(not(target_arch = "wasm32"))]
                            if let Ok(mut clip) = arboard::Clipboard::new() {
                                let _ = clip.set_text(&text);
                            }
                        });
                    },
                    "Copy"
                }
            }
        }
        if !invites.read().is_empty() {
            p { class: "ppl-hint",
                "{invites.read().len()} invite link(s) still unused."
            }
        }
        if let Some(problem) = problem.read().clone() {
            p { class: "ppl-problem", "{problem}" }
        }
        p { class: "ppl-hint",
            "Removing someone archives their account. Their bots, threads, memory and files stay exactly where they are."
        }
    }
}

/* ------------------------------------------------------------------- Spend */

/// SPEND-01: `GET /api/spend` + `PUT /api/spend/ceiling` - port of
/// `Account.tsx:89-118`'s usage figure/bar/per-bot rows, moved into this
/// client's Settings modal rather than a rail popover (this client keeps
/// every setting in one place - see this module's own doc comment - and
/// there is no header popover here to extend). Same dirty-tracking-free
/// save shape as `RulesSection`'s textarea, but the ceiling input has no
/// "unchanged" state to track: every Save attempt round-trips the server,
/// which is the only thing that actually validates a dollar figure.
#[component]
fn SpendSection() -> Element {
    let mut spend = use_signal(|| None::<SpendView>);
    let mut ceiling_input = use_signal(String::new);
    let mut busy = use_signal(|| false);
    let mut save_error = use_signal(|| None::<String>);
    let mut load_error = use_signal(|| None::<String>);

    use_effect(move || {
        spawn(async move {
            match api::fetch_spend(None).await {
                Ok(view) => {
                    ceiling_input.set(format!("{:.2}", view.ceiling));
                    spend.set(Some(view));
                    load_error.set(None);
                }
                Err(err) => load_error.set(Some(err)),
            }
        });
    });

    let save = move |_| {
        let Ok(usd) = ceiling_input.read().trim().parse::<f64>() else {
            save_error.set(Some("Enter a number of dollars, zero or more.".to_string()));
            return;
        };
        busy.set(true);
        spawn(async move {
            match api::set_ceiling(usd).await {
                Ok(ceiling) => {
                    save_error.set(None);
                    let mut next = spend.read().clone();
                    if let Some(view) = next.as_mut() {
                        view.ceiling = ceiling;
                    }
                    spend.set(next);
                    ceiling_input.set(format!("{ceiling:.2}"));
                }
                Err(err) => save_error.set(Some(err)),
            }
            busy.set(false);
        });
    };

    let view = spend.read().clone();
    let is_busy = *busy.read();

    rsx! {
        div { class: "stg-sub",
            // "This month", not "Spend": the group header above already says
            // Spend, and two identical headings stacked on each other read as
            // a rendering bug (caught in the first screenshot of this panel).
            h4 { class: "stg-sub-h", "This month" }

            if let Some(err) = load_error.read().clone() {
                div { class: "refusal",
                    b { "Could not load spend." }
                    p { "{err}" }
                }
            }

            if let Some(view) = view {
                SpendUsage { view }
            }

            if let Some(err) = save_error.read().clone() {
                div { class: "refusal",
                    b { "Refused." }
                    p { "{err}" }
                }
            }

            div { class: "field",
                span { "Ceiling" }
                small { "Runs stop for the rest of the month once total spend reaches this." }
                div { class: "spend-edit",
                    input {
                        r#type: "number",
                        step: "0.01",
                        min: "0",
                        value: "{ceiling_input}",
                        oninput: move |evt| ceiling_input.set(evt.value()),
                    }
                    button {
                        class: "stg-btn",
                        disabled: is_busy,
                        onclick: save,
                        if is_busy { "Saving…" } else { "Save" }
                    }
                }
            }
        }
    }
}

/// The usage figure/bar (or the unreadable-balance copy) and the per-bot
/// rows, split out from `SpendSection` so its fetch/save plumbing above
/// stays flat. `view.account_readable` is the branch to trust, never
/// `view.account.is_some()` alone - the ticket's own contract: a credits
/// read failure must render as "we could not read it", never as "$0.00 of
/// $X" (the inverted-flag mutation the ticket names as the one that
/// matters).
#[component]
fn SpendUsage(view: SpendView) -> Element {
    let used = view.account.map(|a| a.total_usage);
    let pct = match used {
        Some(u) if view.ceiling > 0.0 => (u / view.ceiling * 100.0).min(100.0),
        _ => 0.0,
    };
    // F-SPEND-01b: NO `ceiling > 0.0` guard here, deliberately, unlike the
    // TS panel this is ported from (`Account.tsx:92`). `spend::gate_run`
    // denies a run whenever `used >= ceiling`, and at a ceiling of exactly
    // zero that is true of every run including the first - so a zero ceiling
    // stops the product dead. The TS guard would render that state as a calm
    // empty bar with no warning while every message silently refused. The
    // panel has to say what the gate will actually do.
    let over = used.is_some_and(|u| u >= view.ceiling);
    let near = pct >= 85.0;
    let bar_class = if over {
        "bar is-over"
    } else if near {
        "bar is-near"
    } else {
        "bar"
    };
    // A zero ceiling has no percentage to draw, so the bar renders full
    // rather than empty: it is the stopped state, not the untouched one.
    let width = if over {
        "width: 100%".to_string()
    } else {
        format!("width: {pct}%")
    };
    // COST-01: a bot row whose calls came back unpriced used to read as a
    // bot that cost nothing - `cost_usd` on each row is the sum of only
    // what was priced, so a quiet count rides beside it and the note below
    // points back at the account total as the figure to trust.
    let any_unpriced = view.bots.iter().any(|bot| bot.unpriced_count > 0);

    rsx! {
        if view.account_readable {
            if let Some(u) = used {
                div { class: "acct-usage",
                    div { class: "acct-usage-top",
                        span { class: "acct-fig mono", "${u:.2}" }
                        span { class: "acct-of", "of ${view.ceiling:.2} this month" }
                    }
                    div { class: "{bar_class}",
                        span { style: "{width}" }
                    }
                    if over {
                        p { class: "acct-warn", "Runs are stopped. Raise the ceiling." }
                    }
                }
            }
        } else {
            div { class: "acct-usage",
                b { "Balance unreadable" }
                p { "Runs still allowed. A network blip must not stop your work." }
            }
        }

        if !view.bots.is_empty() {
            div {
                for bot in view.bots.iter() {
                    div { key: "{bot.bot_id}", class: "stg-row",
                        span { "{bot.bot_name}" }
                        span { class: "spend-bot-fig",
                            span { class: "mono", "${bot.cost_usd:.2}" }
                            if bot.unpriced_count > 0 {
                                span { class: "spend-unpriced",
                                    "{bot.unpriced_count} unpriced"
                                }
                            }
                        }
                    }
                }
                if any_unpriced {
                    p { class: "spend-unpriced-note",
                        "The account total above is the figure to trust - some calls below came back with no reported cost, so these rows undercount."
                    }
                }
            }
        }
    }
}

/* ------------------------------------------------------------------- Routing */

#[component]
fn RoutingSection() -> Element {
    let mut state = use_signal(|| None::<RoutingState>);
    let mut text = use_signal(String::new);
    let mut saved = use_signal(String::new);
    let mut busy = use_signal(|| false);

    use_effect(move || {
        spawn(async move {
            if let Ok(s) = api::fetch_routing().await {
                text.set(s.text.clone());
                saved.set(s.text.clone());
                state.set(Some(s));
            }
        });
    });

    let toggle = move |_| {
        let Some(current) = state.read().clone() else {
            return;
        };
        spawn(async move {
            if let Ok(s) = api::put_routing_enabled(!current.enabled).await {
                state.set(Some(s));
            }
        });
    };

    let save = move |_| {
        busy.set(true);
        spawn(async move {
            let value = text.read().clone();
            if let Ok(s) = api::put_routing_text(&value).await {
                text.set(s.text.clone());
                saved.set(s.text.clone());
                state.set(Some(s));
            }
            busy.set(false);
        });
    };

    let enabled = state.read().as_ref().map(|s| s.enabled).unwrap_or(false);
    let is_busy = *busy.read();
    let is_dirty = *text.read() != *saved.read();
    let log = state
        .read()
        .as_ref()
        .map(|s| s.log.clone())
        .unwrap_or_default();

    rsx! {
        div { class: "stg-sub", "data-slot": "routing",
            h4 { class: "stg-sub-h", "Routing" }
            p { class: "set-note",
                "Before a chat run picks its model, a cheap classifier reads your message against this rule and can hand real work to a stronger model. Never a routine, a webhook, or a bot already pinned that high."
            }

            if state.read().is_some() {
                div { class: "stg-row",
                    span { "Route real work to a stronger model" }
                    button {
                        r#type: "button",
                        role: "switch",
                        "aria-checked": "{enabled}",
                        "aria-label": "Route real work to a stronger model",
                        class: if enabled { "stg-toggle is-on" } else { "stg-toggle" },
                        onclick: toggle,
                        span { class: "stg-toggle-knob" }
                    }
                }
            }

            textarea {
                class: "rules-box",
                spellcheck: "false",
                rows: "4",
                "aria-label": "Routing rule",
                value: "{text}",
                oninput: move |evt| text.set(evt.value()),
            }
            div { class: "rules-foot",
                button {
                    class: "stg-btn",
                    disabled: is_busy || !is_dirty,
                    onclick: save,
                    if is_busy { "Saving…" } else if is_dirty { "Save" } else { "Saved" }
                }
            }

            div { class: "routing-log",
                h4 { class: "stg-sub-h", "Last 20 routings" }
                if log.is_empty() {
                    p { class: "muted", "Nothing routed yet." }
                } else {
                    table { class: "rule-table",
                        thead {
                            tr {
                                th { "Time" }
                                th { "Verdict" }
                                th { "Model" }
                            }
                        }
                        tbody {
                            for entry in log.iter() {
                                tr { key: "{entry.id}",
                                    td { "{format_time(&entry.created_at)}" }
                                    td { "{entry.verdict}" }
                                    td { "{entry.model}" }
                                }
                            }
                        }
                    }
                }
            }
        }
    }
}

/* --------------------------------------------------------------- Auto review */

/// S4-05: the platform toggle for `crates/server/src/judge.rs` (the cheap
/// second-opinion model that judges risky tool calls the grid already said
/// Allow to) plus its "Last 20 judgements" log. Same shape as
/// `RoutingSection` above - a toggle and a read-only log table - but no
/// rule text box: the judge has no free-text rule of its own, only the
/// on/off switch `PUT /api/auto-review/judge` flips.
#[component]
fn AutoReviewSection() -> Element {
    let mut state = use_signal(|| None::<AutoReviewState>);
    let mut log = use_signal(Vec::<AutoReviewLogEntry>::new);

    use_effect(move || {
        spawn(async move {
            if let Ok(s) = api::fetch_judge().await {
                state.set(Some(s));
            }
            if let Ok(entries) = api::fetch_judge_log(20).await {
                log.set(entries);
            }
        });
    });

    let toggle = move |_| {
        let Some(current) = *state.read() else {
            return;
        };
        spawn(async move {
            if let Ok(s) = api::put_judge(!current.enabled).await {
                state.set(Some(s));
            }
        });
    };

    let enabled = state.read().as_ref().map(|s| s.enabled).unwrap_or(false);
    let entries = log.read().clone();

    rsx! {
        div { class: "stg-sub", "data-slot": "auto-review",
            h4 { class: "stg-sub-h", "Auto review" }
            p { class: "set-note",
                "Before a risky call the grid already allowed - shell, a desk action, SSH, delegating to another bot, reading a file - a cheap classifier judges it for risk to your data, machine, money or reputation. A risky or dangerous verdict asks first instead of running unwatched."
            }

            if state.read().is_some() {
                div { class: "stg-row",
                    span { "Judge risky calls before they run" }
                    button {
                        r#type: "button",
                        role: "switch",
                        "aria-checked": "{enabled}",
                        "aria-label": "Judge risky calls before they run",
                        class: if enabled { "stg-toggle is-on" } else { "stg-toggle" },
                        onclick: toggle,
                        span { class: "stg-toggle-knob" }
                    }
                }
            }

            div { class: "autoreview-log",
                h4 { class: "stg-sub-h", "Last 20 judgements" }
                if entries.is_empty() {
                    p { class: "muted", "Nothing judged yet." }
                } else {
                    table { class: "rule-table",
                        thead {
                            tr {
                                th { "Time" }
                                th { "Verdict" }
                                th { "Call" }
                                th { "Decision" }
                            }
                        }
                        tbody {
                            for entry in entries.iter() {
                                tr { key: "{entry.id}",
                                    td { "{format_time(&entry.created_at)}" }
                                    td {
                                        span { class: "judge-badge judge-{entry.verdict}", "{entry.verdict}" }
                                    }
                                    td { "{entry.description}" }
                                    td { "{entry.decision}" }
                                }
                            }
                        }
                    }
                }
            }
        }
    }
}

/* ------------------------------------------------------------- S10-02 */

/// S10-02: the skill library - a reusable "how to do this job", written
/// once and shared across bots (`edit_bot.rs`'s per-bot checkbox list is
/// the other half - see that module's own doc). Same fetch-on-mount,
/// plain-list shape `ArchivedBotsSection`/`HiddenBotsSection` below already
/// take.
///
/// 🔴 Lists DESCRIPTIONS, never bodies - `store::skills`'s own "one line per
/// skill" doc: 37 imported skills is a megabyte of markdown nobody reads,
/// and it buries the one line that actually decides anything (when to use
/// it). `show` below fetches a row's body only once Josh opens it, mirroring
/// `Skills.tsx`'s own `show()`.
///
/// Ported from `Skills.tsx`'s Skills section only - NOT its "The shared
/// computer" section at the top of that file. That block calls
/// `GET /api/desk` and links to `/desk/`; in the TS product that IS live
/// TypeScript Bullpen's own browser (`bullpen-desk` is not an orphan
/// there). bullpen-rs uses per-bot VMs instead (`vm_card.rs`,
/// `vm::vm_desk`), and `PROJECT.md` section 7 forbids new code reaching
/// `desk_config(env)` - so there is nothing here to port that block to.
///
/// The empty state says plainly that nothing is set up - unlike the TS
/// original's `npm run import-skills`; bulk import is the
/// `import-skills` CLI, not an in-app picker (S10-04).
///
/// S10-03: create and edit via `PUT /api/skills/{name}`; list and open-row
/// body always follow server truth after a successful save (see
/// `skills_save_outcome` below).
#[component]
fn SkillsSection() -> Element {
    let mut skills = use_signal(|| None::<Vec<SkillSummary>>);
    let mut load_error = use_signal(|| None::<String>);
    let mut open = use_signal(|| None::<String>);
    let mut body = use_signal(String::new);
    let mut body_error = use_signal(|| None::<String>);
    let mut editor = use_signal(|| None::<SkillEditorMode>);
    let mut form_name = use_signal(String::new);
    let mut form_description = use_signal(String::new);
    let mut form_body = use_signal(String::new);
    let mut save_error = use_signal(|| None::<String>);
    let mut saving = use_signal(|| false);
    let mut saved_name_note = use_signal(|| None::<String>);
    let mut confirm_delete = use_signal(|| None::<String>);

    use_effect(move || {
        spawn(async move {
            match api::fetch_skills().await {
                Ok(list) => skills.set(Some(list)),
                Err(err) => load_error.set(Some(err)),
            }
        });
    });

    let show = move |name: String| {
        if editor.read().is_some() {
            return;
        }
        if open.read().as_deref() == Some(name.as_str()) {
            open.set(None);
            return;
        }
        open.set(Some(name.clone()));
        body.set(String::new());
        body_error.set(None);
        spawn(async move {
            match api::fetch_skill_body(&name).await {
                Ok(text) => body.set(if text.is_empty() {
                    "(empty)".to_string()
                } else {
                    text
                }),
                Err(err) => body_error.set(Some(err)),
            }
        });
    };

    let start_create = move |_| {
        editor.set(Some(SkillEditorMode::Create));
        form_name.set(String::new());
        form_description.set(String::new());
        form_body.set(String::new());
        save_error.set(None);
        saved_name_note.set(None);
    };

    let mut start_edit = move |name: String, description: String| {
        editor.set(Some(SkillEditorMode::Edit(name.clone())));
        form_description.set(description);
        save_error.set(None);
        saved_name_note.set(None);
        if open.read().as_deref() == Some(name.as_str()) && !body.read().is_empty() {
            let raw = body.read().clone();
            form_body.set(if raw == "(empty)" { String::new() } else { raw });
        } else {
            form_body.set(String::new());
            let fetch_name = name.clone();
            spawn(async move {
                match api::fetch_skill_body(&fetch_name).await {
                    Ok(text) => form_body.set(text),
                    Err(err) => save_error.set(Some(err)),
                }
            });
        }
    };

    let cancel_editor = move |_| {
        editor.set(None);
        save_error.set(None);
        saved_name_note.set(None);
    };

    let submit_save = move |_| {
        saving.set(true);
        save_error.set(None);
        saved_name_note.set(None);
        let mode = editor.read().clone();
        let path_name = match &mode {
            Some(SkillEditorMode::Create) => form_name.read().clone(),
            Some(SkillEditorMode::Edit(n)) => n.clone(),
            None => {
                saving.set(false);
                return;
            }
        };
        let description = form_description.read().clone();
        let body_text = form_body.read().clone();
        let typed_create_name = form_name.read().clone();
        spawn(async move {
            let response = api::put_skill(&path_name, &description, &body_text).await;
            let err_msg = response.as_ref().err().cloned();
            let before = skills.read().clone().unwrap_or_default();
            if let Some((next_list, saved)) = skills_save_outcome(&before, response) {
                skills.set(Some(next_list));
                editor.set(None);
                open.set(Some(saved.name.clone()));
                body.set(if saved.body.is_empty() {
                    "(empty)".to_string()
                } else {
                    saved.body.clone()
                });
                body_error.set(None);
                if matches!(mode, Some(SkillEditorMode::Create))
                    && saved.name != typed_create_name.trim()
                {
                    saved_name_note.set(Some(format!(
                        "Saved as \"{}\" (names are normalised).",
                        saved.name
                    )));
                }
            } else if let Some(err) = err_msg {
                save_error.set(Some(err));
            }
            saving.set(false);
        });
    };

    let list = skills.read().clone();
    let open_name = open.read().clone();
    let editor_mode = editor.read().clone();
    let is_saving = *saving.read();

    rsx! {
        div { class: "stg-sub",
            h4 { class: "stg-sub-h", "Skills" }
            p { class: "set-note",
                "A way of doing a job, written once. Each bot gets the ones you give it in its own editor, and loads the instructions only when it needs them."
            }

            div { class: "skill-toolbar",
                button {
                    r#type: "button",
                    class: "stg-btn",
                    disabled: editor_mode.is_some() || is_saving,
                    onclick: start_create,
                    "Add skill"
                }
            }

            if let Some(note) = saved_name_note.read().clone() {
                p { class: "notice-inline", "{note}" }
            }

            if let Some(mode) = editor_mode.clone() {
                div { class: "skill-form",
                    if let SkillEditorMode::Create = mode {
                        div { class: "field",
                            span { "Name" }
                            small { "Required. Becomes the skill key after normalisation (spaces to hyphens, lower case)." }
                            input {
                                class: "skill-name-input",
                                r#type: "text",
                                spellcheck: "false",
                                value: "{form_name}",
                                oninput: move |evt| form_name.set(evt.value()),
                            }
                        }
                    } else if let SkillEditorMode::Edit(name) = mode {
                        div { class: "field",
                            span { "Name" }
                            small { "Cannot be renamed here — create a new skill instead." }
                            input {
                                class: "skill-name-input",
                                r#type: "text",
                                spellcheck: "false",
                                value: "{name}",
                                readonly: true,
                            }
                        }
                    }
                    div { class: "field",
                        span { "When to use it" }
                        input {
                            class: "skill-desc-input",
                            r#type: "text",
                            spellcheck: "false",
                            value: "{form_description}",
                            oninput: move |evt| form_description.set(evt.value()),
                        }
                    }
                    div { class: "field",
                        span { "Instructions" }
                        textarea {
                            class: "rules-box skill-body-box",
                            spellcheck: "false",
                            rows: "10",
                            value: "{form_body}",
                            oninput: move |evt| form_body.set(evt.value()),
                        }
                    }
                    if let Some(err) = save_error.read().clone() {
                        p { class: "notice-inline", "{err}" }
                    }
                    div { class: "rules-foot",
                        button {
                            r#type: "button",
                            class: "stg-btn",
                            disabled: is_saving
                                || (matches!(editor_mode, Some(SkillEditorMode::Create))
                                    && form_name.read().trim().is_empty()),
                            onclick: submit_save,
                            if is_saving { "Saving…" } else { "Save skill" }
                        }
                        button {
                            r#type: "button",
                            class: "stg-btn",
                            disabled: is_saving,
                            onclick: cancel_editor,
                            "Cancel"
                        }
                    }
                }
            }

            if let Some(err) = load_error.read().clone() {
                div { class: "refusal",
                    b { "Could not load skills." }
                    p { "{err}" }
                }
            }

            if list.is_none() && load_error.read().is_none() {
                p { class: "muted", "Loading…" }
            }

            if let Some(list) = list {
                if list.is_empty() {
                    p { class: "muted",
                        "None yet. From the repo root, "
                        code { "cargo run -p server --bin import-skills -- --dry" }
                        " previews Claude Code skills; drop "
                        code { "--dry" }
                        " and set "
                        code { "BULLPEN_PASSWORD" }
                        " to import. Or use Add skill above."
                    }
                } else {
                    div { class: "skill-list",
                        for skill in list.iter() {
                            div { key: "{skill.id}", class: "skill-row",
                                button {
                                    r#type: "button",
                                    class: "skill-head",
                                    disabled: editor_mode.is_some(),
                                    onclick: {
                                        let mut show = show;
                                        let name = skill.name.clone();
                                        move |_| show(name.clone())
                                    },
                                    span { class: "skill-name", "{skill.name}" }
                                    if skill.source == "claude-code" {
                                        span { class: "skill-tag", "Claude Code" }
                                    }
                                    span { class: "skill-when", "{skill.description}" }
                                }
                                if open_name.as_deref() == Some(skill.name.as_str()) {
                                    if editor_mode == Some(SkillEditorMode::Edit(skill.name.clone())) {
                                        // Form is rendered above; keep the row open.
                                    } else if let Some(err) = body_error.read().clone() {
                                        p { class: "notice-inline", "{err}" }
                                    } else {
                                        div { class: "skill-open-actions",
                                            button {
                                                r#type: "button",
                                                class: "stg-btn",
                                                disabled: editor_mode.is_some(),
                                                onclick: {
                                                    let name = skill.name.clone();
                                                    let desc = skill.description.clone();
                                                    move |_| start_edit(name.clone(), desc.clone())
                                                },
                                                "Edit"
                                            }
                                            button {
                                                r#type: "button",
                                                class: "stg-btn danger",
                                                disabled: editor_mode.is_some(),
                                                onclick: {
                                                    let skill_name = skill.name.clone();
                                                    move |_| confirm_delete.set(Some(skill_name.clone()))
                                                },
                                                "Delete"
                                            }
                                        }
                                        pre { class: "mono skill-body",
                                            if body.read().is_empty() { "Loading…" } else { "{body}" }
                                        }
                                    }
                                }
                            }
                        }
                    }
                }
            }
        }
        if let Some(delete_name) = confirm_delete.read().clone() {
            DeleteSkillConfirmModal {
                skill_name: delete_name,
                on_close: move |_| confirm_delete.set(None),
                on_deleted: move |_| {
                    let removed = confirm_delete.read().clone().unwrap_or_default();
                    let list = skills.read().clone();
                    confirm_delete.set(None);
                    open.set(None);
                    body.set(String::new());
                    if let Some(list) = list {
                        skills.set(Some(skills_list_after_delete(&list, &removed)));
                    }
                },
            }
        }
    }
}

/// SEC5-03: confirm before removing a skill from the library.
#[component]
fn DeleteSkillConfirmModal(
    skill_name: String,
    on_close: EventHandler<()>,
    on_deleted: EventHandler<()>,
) -> Element {
    let mut busy = use_signal(|| false);
    let mut error = use_signal(|| None::<String>);
    let confirm_name = skill_name.clone();

    let confirm = move |_| {
        if *busy.read() {
            return;
        }
        busy.set(true);
        error.set(None);
        let name = confirm_name.clone();
        spawn(async move {
            match api::delete_skill(&name).await {
                Ok(()) => {
                    busy.set(false);
                    on_deleted.call(());
                }
                Err(err) => {
                    busy.set(false);
                    error.set(Some(err));
                }
            }
        });
    };

    let is_busy = *busy.read();
    let label = skill_name.clone();

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
                "aria-label": "Delete {label}",
                div { class: "modal-head",
                    h2 { "Delete {label}?" }
                    button {
                        class: "modal-x",
                        "aria-label": "Close",
                        onclick: move |_| on_close.call(()),
                        "×"
                    }
                }
                div { class: "modal-body",
                    p { class: "set-note",
                        "Removes the skill from the library and turns it off on every bot that had it enabled."
                    }
                    if let Some(err) = error.read().clone() {
                        div { class: "refusal",
                            b { "Could not delete." }
                            p { "{err}" }
                        }
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
                        if is_busy { "Deleting…" } else { "Delete skill" }
                    }
                }
            }
        }
    }
}

#[derive(Clone, PartialEq, Eq)]
enum SkillEditorMode {
    Create,
    Edit(String),
}

/// List row derived from a full PUT/GET skill - body length only, never the
/// body text (`SkillSummary`'s own contract).
fn skill_summary_from_skill(skill: &Skill) -> SkillSummary {
    SkillSummary {
        id: skill.id.clone(),
        name: skill.name.clone(),
        description: skill.description.clone(),
        bytes: skill.body.len() as u64,
        source: skill.source.clone(),
        created_at: skill.created_at.clone(),
        updated_at: skill.updated_at.clone(),
    }
}

/// Insert or replace one library row from a successful save; sort by name so
/// the list stays stable regardless of upsert vs create.
fn skills_list_after_delete(before: &[SkillSummary], deleted_name: &str) -> Vec<SkillSummary> {
    before
        .iter()
        .filter(|row| row.name != deleted_name)
        .cloned()
        .collect()
}

fn skills_list_after_save(before: &[SkillSummary], saved: &Skill) -> Vec<SkillSummary> {
    let summary = skill_summary_from_skill(saved);
    let mut out = before.to_vec();
    if let Some(i) = out
        .iter()
        .position(|s| s.id == summary.id || s.name == summary.name)
    {
        out[i] = summary;
    } else {
        out.push(summary);
    }
    out.sort_by(|a, b| a.name.cmp(&b.name));
    out
}

/// After PUT: refresh the library from server truth, or change nothing on
/// failure - same discipline as `edit_bot.rs`'s `skill_set_after_response`.
fn skills_save_outcome(
    before: &[SkillSummary],
    response: Result<Skill, String>,
) -> Option<(Vec<SkillSummary>, Skill)> {
    match response {
        Ok(skill) => Some((skills_list_after_save(before, &skill), skill)),
        Err(_) => None,
    }
}

#[cfg(test)]
mod skill_save_tests {
    use super::*;

    fn sample_summary(name: &str, desc: &str) -> SkillSummary {
        SkillSummary {
            id: format!("id-{name}"),
            name: name.to_string(),
            description: desc.to_string(),
            bytes: 0,
            source: "bullpen".to_string(),
            created_at: "t0".to_string(),
            updated_at: "t0".to_string(),
        }
    }

    fn sample_skill(name: &str, desc: &str, body: &str) -> Skill {
        Skill {
            id: format!("id-{name}"),
            name: name.to_string(),
            description: desc.to_string(),
            body: body.to_string(),
            source: "bullpen".to_string(),
            created_at: "t1".to_string(),
            updated_at: "t1".to_string(),
        }
    }

    #[test]
    fn save_outcome_updates_list_without_body_in_summary() {
        let before = vec![sample_summary("alpha", "old")];
        let saved = sample_skill("alpha", "new when", "secret body");

        let Some((after, _)) = skills_save_outcome(&before, Ok(saved.clone())) else {
            panic!("expected success");
        };

        assert_eq!(after.len(), 1);
        assert_eq!(after[0].description, "new when");
        assert_eq!(after[0].bytes, saved.body.len() as u64);
        assert!(!after[0].name.is_empty());
    }

    #[test]
    fn save_outcome_inserts_new_row_sorted() {
        let before = vec![sample_summary("beta", "")];
        let saved = sample_skill("alpha", "a", "");

        let Some((after, _)) = skills_save_outcome(&before, Ok(saved)) else {
            panic!("expected success");
        };

        assert_eq!(after.len(), 2);
        assert_eq!(after[0].name, "alpha");
        assert_eq!(after[1].name, "beta");
    }

    #[test]
    fn save_outcome_err_leaves_before_untouched() {
        let before = vec![sample_summary("keep", "x")];

        let after = skills_save_outcome(&before, Err("nope".to_string()));

        assert_eq!(after, None);
    }

    #[test]
    fn delete_removes_the_row_from_the_local_list() {
        let before = vec![sample_summary("alpha", "a"), sample_summary("beta", "b")];
        let after = skills_list_after_delete(&before, "alpha");
        assert_eq!(after.len(), 1);
        assert_eq!(after[0].name, "beta");
    }
}

/* ------------------------------------------------------------- ARCH-01 */

/// ARCH-01: the only place an archived bot is still reachable - deliberately
/// quiet (the ticket's own word), tucked at the bottom of the "Bot" group
/// rather than a primary rail/header surface, but it has to exist somewhere
/// or archiving is a one-way door. Same fetch-on-mount, plain-list shape as
/// `SpendUsage`'s per-bot rows above; "Restore" is one click, no confirm -
/// unlike `thread.rs`'s `ArchiveConfirmModal`, restoring only ever ADDS a
/// bot back to the roster, so there is nothing here a mis-click could lose.
#[component]
fn ArchivedBotsSection(#[props(default)] on_restored: Option<EventHandler<()>>) -> Element {
    let mut bots = use_signal(Vec::<crate::types::Bot>::new);
    let mut load_error = use_signal(|| None::<String>);
    let mut restore_error = use_signal(|| None::<String>);
    let mut restoring = use_signal(|| None::<String>);
    let mut confirm_delete = use_signal(|| None::<(String, String)>);

    use_effect(move || {
        spawn(async move {
            match api::fetch_archived_bots().await {
                Ok(list) => bots.set(list),
                Err(err) => load_error.set(Some(err)),
            }
        });
    });

    let list = bots.read().clone();
    let busy_id = restoring.read().clone();

    rsx! {
        div { class: "stg-sub",
            h4 { class: "stg-sub-h", "Archived bots" }
            p { class: "set-note",
                "Hidden from the roster. Restore brings a bot back; Delete permanently removes it and all of its conversations, memory, and routines."
            }

            if let Some(err) = load_error.read().clone() {
                div { class: "refusal",
                    b { "Could not load archived bots." }
                    p { "{err}" }
                }
            }

            if let Some(err) = restore_error.read().clone() {
                div { class: "refusal",
                    b { "Could not restore." }
                    p { "{err}" }
                }
            }

            if list.is_empty() && load_error.read().is_none() {
                p { class: "muted", "No archived bots." }
            } else {
                for bot in list.iter() {
                    div { key: "{bot.id}", class: "stg-row",
                        span { "{bot.name}" }
                        button {
                            class: "stg-btn",
                            disabled: busy_id.as_deref() == Some(bot.id.as_str()),
                            onclick: {
                                let id = bot.id.clone();
                                move |_| {
                                    let id = id.clone();
                                    restoring.set(Some(id.clone()));
                                    restore_error.set(None);
                                    spawn(async move {
                                        match api::archive_bot(&id, false).await {
                                            Ok(_) => {
                                                bots.write().retain(|b| b.id != id);
                                                restoring.set(None);
                                                if let Some(handler) = on_restored.as_ref() {
                                                    handler.call(());
                                                }
                                            }
                                            Err(err) => {
                                                restoring.set(None);
                                                restore_error.set(Some(err));
                                            }
                                        }
                                    });
                                }
                            },
                            if busy_id.as_deref() == Some(bot.id.as_str()) { "Restoring…" } else { "Restore" }
                        }
                        button {
                            class: "stg-btn danger",
                            disabled: busy_id.as_deref() == Some(bot.id.as_str()),
                            onclick: {
                                let id = bot.id.clone();
                                let name = bot.name.clone();
                                move |_| confirm_delete.set(Some((id.clone(), name.clone())))
                            },
                            "Delete"
                        }
                    }
                }
            }
        }
        if let Some((id, name)) = confirm_delete.read().clone() {
            HardDeleteBotConfirmModal {
                bot_id: id,
                bot_name: name,
                on_close: move |_| confirm_delete.set(None),
                on_deleted: move |deleted_id| {
                    bots.write().retain(|b| b.id != deleted_id);
                    confirm_delete.set(None);
                },
            }
        }
    }
}

/// SEC5-09: confirm before permanently deleting an archived bot.
#[component]
fn HardDeleteBotConfirmModal(
    bot_id: String,
    bot_name: String,
    on_close: EventHandler<()>,
    on_deleted: EventHandler<String>,
) -> Element {
    let mut busy = use_signal(|| false);
    let mut error = use_signal(|| None::<String>);
    let confirm_id = bot_id.clone();

    let confirm = move |_| {
        if *busy.read() {
            return;
        }
        busy.set(true);
        error.set(None);
        let id = confirm_id.clone();
        spawn(async move {
            match api::hard_delete_bot(&id).await {
                Ok(()) => {
                    busy.set(false);
                    on_deleted.call(id);
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
                "aria-label": "Delete {bot_name}",
                div { class: "modal-head",
                    h2 { "Delete {bot_name} permanently?" }
                    button {
                        class: "modal-x",
                        "aria-label": "Close",
                        onclick: move |_| on_close.call(()),
                        "×"
                    }
                }
                p {
                    "This cannot be undone. All conversations, memory, routines, and spend history for this bot will be removed."
                }
                if let Some(err) = error.read().clone() {
                    div { class: "refusal",
                        b { "Could not delete." }
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
                        if is_busy { "Deleting…" } else { "Delete permanently" }
                    }
                }
            }
        }
    }
}

/* ------------------------------------------------------------- RAIL-02 */

/// RAIL-02: create, rename, and delete a section - the rail's settings
/// affordance the ticket asks for, rather than a new top-level surface
/// (same "tucked into Settings" placement `ArchivedBotsSection`/
/// `HiddenBotsSection` below already take for the rest of the rail's
/// context-menu actions). `sections` arrives as a prop (`app.rs`'s own
/// `data.sections`, the same list `rail.rs` renders headers from) rather
/// than a second fetch - there is no `GET /api/sections` route at all
/// (`crates/server/src/routes/sections.rs`'s own doc: `/api/roster` already
/// carries the list, and creating/renaming/deleting a section is visible
/// there on the very next fetch). `on_changed` fires after every successful
/// create/rename/delete so `app.rs` can re-fetch the roster - reusing
/// `SettingsModal`'s existing `on_restored` handler rather than adding a
/// third differently-named "please refresh" prop for the same thing
/// `ArchivedBotsSection`/`HiddenBotsSection` already ask for.
#[component]
fn SectionsManagerSection(
    sections: Vec<Section>,
    #[props(default)] on_changed: Option<EventHandler<()>>,
) -> Element {
    let mut new_name = use_signal(String::new);
    let mut create_busy = use_signal(|| false);
    let mut create_error = use_signal(|| None::<String>);

    let do_create = move |evt: FormEvent| {
        evt.prevent_default();
        if *create_busy.read() {
            return;
        }
        let name = new_name.read().trim().to_string();
        if name.is_empty() {
            return;
        }
        create_busy.set(true);
        create_error.set(None);
        spawn(async move {
            match api::create_section(&name).await {
                Ok(_) => {
                    create_busy.set(false);
                    new_name.set(String::new());
                    if let Some(handler) = on_changed.as_ref() {
                        handler.call(());
                    }
                }
                Err(err) => {
                    create_busy.set(false);
                    create_error.set(Some(err));
                }
            }
        });
    };

    rsx! {
        div { class: "stg-sub",
            h4 { class: "stg-sub-h", "Sections" }
            p { class: "set-note",
                "Groups the rail. Deleting a section never deletes its bots - they fall back to Unassigned."
            }

            if let Some(err) = create_error.read().clone() {
                div { class: "refusal",
                    b { "Could not create." }
                    p { "{err}" }
                }
            }

            form { class: "stg-row", onsubmit: do_create,
                input {
                    class: "sec-name-input",
                    value: "{new_name.read()}",
                    placeholder: "New section name",
                    "aria-label": "New section name",
                    oninput: move |evt| new_name.set(evt.value()),
                }
                button {
                    class: "stg-btn",
                    r#type: "submit",
                    disabled: *create_busy.read() || new_name.read().trim().is_empty(),
                    if *create_busy.read() { "Adding…" } else { "Add" }
                }
            }

            if sections.is_empty() {
                p { class: "muted", "No sections yet - every bot shows under Unassigned." }
            } else {
                for section in sections.iter() {
                    SectionRow {
                        key: "{section.id}",
                        section: section.clone(),
                        on_changed,
                    }
                }
            }
        }
    }
}

/// One row of `SectionsManagerSection`: an editable name (Rename, no
/// confirm - the ticket's own "Renaming and moving do not ask") and a
/// Delete button that opens `DeleteSectionConfirmModal` below (the ticket's
/// own "Deleting a section asks first").
#[component]
fn SectionRow(section: Section, #[props(default)] on_changed: Option<EventHandler<()>>) -> Element {
    let mut draft = use_signal(|| section.name.clone());
    let mut busy = use_signal(|| false);
    let mut error = use_signal(|| None::<String>);
    let mut confirm_delete = use_signal(|| false);

    let current_name = section.name.clone();
    let changed = {
        let trimmed = draft.read().trim().to_string();
        !trimmed.is_empty() && trimmed != current_name
    };

    let rename_id = section.id.clone();
    let do_rename = move |evt: FormEvent| {
        evt.prevent_default();
        if *busy.read() || !changed {
            return;
        }
        busy.set(true);
        error.set(None);
        let id = rename_id.clone();
        let name = draft.read().trim().to_string();
        spawn(async move {
            match api::rename_section(&id, &name).await {
                Ok(_) => {
                    busy.set(false);
                    if let Some(handler) = on_changed.as_ref() {
                        handler.call(());
                    }
                }
                Err(err) => {
                    busy.set(false);
                    error.set(Some(err));
                }
            }
        });
    };

    rsx! {
        div { class: "stg-row",
            form {
                style: "display: flex; align-items: center; gap: 0.5rem; flex: 1; min-width: 0;",
                onsubmit: do_rename,
                input {
                    class: "sec-name-input",
                    value: "{draft.read()}",
                    "aria-label": "Section name",
                    oninput: move |evt| draft.set(evt.value()),
                }
                button {
                    class: "stg-btn",
                    r#type: "submit",
                    disabled: *busy.read() || !changed,
                    if *busy.read() { "Saving…" } else { "Rename" }
                }
            }
            button {
                class: "stg-btn danger",
                r#type: "button",
                onclick: move |_| confirm_delete.set(true),
                "Delete"
            }
        }
        if let Some(err) = error.read().clone() {
            div { class: "refusal",
                b { "Could not rename." }
                p { "{err}" }
            }
        }
        if *confirm_delete.read() {
            DeleteSectionConfirmModal {
                section: section.clone(),
                on_close: move |_| confirm_delete.set(false),
                on_deleted: move |_| {
                    confirm_delete.set(false);
                    if let Some(handler) = on_changed.as_ref() {
                        handler.call(());
                    }
                },
            }
        }
    }
}

/// RAIL-02: "Deleting a section asks first, and the confirm says its bots
/// move to Unassigned rather than disappearing" - the ticket's own text,
/// same `.modal-scrim`/`.modal`/`.rules-foot`/`.stg-btn`/`.stg-btn.danger`
/// shape `thread.rs`'s `ArchiveConfirmModal` already establishes for
/// exactly this "explain what actually happens, then a Cancel/danger-button
/// pair" posture (see that component's own doc for the class list).
#[component]
fn DeleteSectionConfirmModal(
    section: Section,
    on_close: EventHandler<()>,
    on_deleted: EventHandler<()>,
) -> Element {
    let mut busy = use_signal(|| false);
    let mut error = use_signal(|| None::<String>);

    let confirm_id = section.id.clone();
    let confirm = move |_| {
        if *busy.read() {
            return;
        }
        busy.set(true);
        error.set(None);
        let id = confirm_id.clone();
        spawn(async move {
            match api::delete_section(&id).await {
                Ok(()) => {
                    busy.set(false);
                    on_deleted.call(());
                }
                Err(err) => {
                    busy.set(false);
                    error.set(Some(err));
                }
            }
        });
    };

    let is_busy = *busy.read();
    let name = section.name.clone();

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
                "aria-label": "Delete {name}",
                div { class: "modal-head",
                    h2 { "Delete {name}?" }
                    button {
                        class: "modal-x",
                        "aria-label": "Close",
                        onclick: move |_| on_close.call(()),
                        "×"
                    }
                }
                div { class: "modal-body",
                    p { class: "set-note",
                        "Its bots move to Unassigned - nothing is deleted but the grouping itself."
                    }
                    if let Some(err) = error.read().clone() {
                        div { class: "refusal",
                            b { "Could not delete." }
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
                            if is_busy { "Deleting…" } else { "Delete" }
                        }
                    }
                }
            }
        }
    }
}

/* ------------------------------------------------------------- RAIL-01 */

/// RAIL-01: the only place a hidden bot is reachable other than the header
/// button that hid it - tucked right beside `ArchivedBotsSection` above,
/// the ticket's own pick (the TS original instead grows a collapsible
/// "Hidden" section inside the rail itself, `Roster.tsx:296-318` - out of
/// scope here; this ticket only asks for "reachable and unhideable, next to
/// the archived list in Settings"). Same fetch-on-mount, plain-list shape,
/// same one-click-no-confirm "Unhide" button too: hiding is even MORE
/// reversible than archiving (no confirm on the way in either, per the
/// ticket - "a confirm on a pin would be noise"), so there is even less
/// reason for a confirm on the way back out than `ArchivedBotsSection`'s
/// own "Restore" already has none of.
#[component]
fn HiddenBotsSection(#[props(default)] on_restored: Option<EventHandler<()>>) -> Element {
    let mut bots = use_signal(Vec::<crate::types::Bot>::new);
    let mut load_error = use_signal(|| None::<String>);
    let mut unhide_error = use_signal(|| None::<String>);
    let mut unhiding = use_signal(|| None::<String>);

    use_effect(move || {
        spawn(async move {
            match api::fetch_hidden_bots().await {
                Ok(list) => bots.set(list),
                Err(err) => load_error.set(Some(err)),
            }
        });
    });

    let list = bots.read().clone();
    let busy_id = unhiding.read().clone();

    rsx! {
        div { class: "stg-sub",
            h4 { class: "stg-sub-h", "Hidden bots" }
            p { class: "set-note",
                "Off the rail, not archived and not deleted - conversations, memory and spend history are all still there. Unhide one to bring it back."
            }

            if let Some(err) = load_error.read().clone() {
                div { class: "refusal",
                    b { "Could not load hidden bots." }
                    p { "{err}" }
                }
            }

            if let Some(err) = unhide_error.read().clone() {
                div { class: "refusal",
                    b { "Could not unhide." }
                    p { "{err}" }
                }
            }

            if list.is_empty() && load_error.read().is_none() {
                p { class: "muted", "No hidden bots." }
            } else {
                for bot in list.iter() {
                    div { key: "{bot.id}", class: "stg-row",
                        span { "{bot.name}" }
                        button {
                            class: "stg-btn",
                            disabled: busy_id.as_deref() == Some(bot.id.as_str()),
                            onclick: {
                                let id = bot.id.clone();
                                move |_| {
                                    let id = id.clone();
                                    unhiding.set(Some(id.clone()));
                                    unhide_error.set(None);
                                    spawn(async move {
                                        match api::set_bot_hidden(&id, false).await {
                                            Ok(()) => {
                                                bots.write().retain(|b| b.id != id);
                                                unhiding.set(None);
                                                if let Some(handler) = on_restored.as_ref() {
                                                    handler.call(());
                                                }
                                            }
                                            Err(err) => {
                                                unhiding.set(None);
                                                unhide_error.set(Some(err));
                                            }
                                        }
                                    });
                                }
                            },
                            if busy_id.as_deref() == Some(bot.id.as_str()) { "Unhiding…" } else { "Unhide" }
                        }
                    }
                }
            }
        }
    }
}
