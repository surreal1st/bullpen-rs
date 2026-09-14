//! Port of `SettingsModal.tsx` + the slice of `General.tsx` this ticket asks
//! for: the Models section (default/mid/tier1/premium pickers), the Routing
//! card (enabled toggle, rule text, the "Last 20 routings" log) and the
//! house-rules textarea. `Account`/`Appearance`/`System`/`Bot > ModelsShown`/
//! `AutoReviewSlot`/`SecondOpinion`/`Alerts` are explicitly SKIPPED per the
//! ticket - only a single "General" nav item is rendered (the TS original's
//! Computer/Billing/Updates tabs, and its owner-vs-member `isOwner()` gate,
//! have no bullpen-rs equivalent yet).

use crate::api;
use crate::message_time::format_time;
use crate::model_chip::short_model;
use crate::types::{AutoReviewLogEntry, AutoReviewState, CatalogEntry, RoutingState};
use dioxus::prelude::*;
use gloo_timers::future::TimeoutFuture;
use std::cell::Cell;
use std::rc::Rc;

#[component]
pub fn SettingsModal(on_close: EventHandler<()>) -> Element {
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
                        GeneralSettings {}
                    }
                }
            }
        }
    }
}

#[component]
fn GeneralSettings() -> Element {
    rsx! {
        section { class: "stg-group",
            h3 { class: "stg-group-h", "Bot" }
            div { class: "stg-card stg-card-loose",
                ModelsSection {}
                RulesSection {}
                RoutingSection {}
                AutoReviewSection {}
            }
        }
        section { class: "stg-group",
            h3 { class: "stg-group-h", "Memory" }
            div { class: "stg-card stg-card-loose",
                SharedMemorySection {}
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
            TimeoutFuture::new(180).await;
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
