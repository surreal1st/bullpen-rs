//! Port of `ModelEffortPopover.tsx` (+ `ModelPicker.tsx`'s
//! `MainstreamModelSelect`): the thread header's model chip, and the popover
//! above it that pins a model and picks a Low/Medium/High reasoning effort.
//! Both fields go through `PATCH /api/bots/:id` (`crates/server/src/routes/
//! bots.rs`, added by this same ticket), so the same server-side premium
//! refusal `crate::api::patch_bot` can carry back shows up here rather than
//! being swallowed.
//!
//! Deviations from the TS original, in the interest of this ticket's scope:
//! - No document-level mousedown/Escape listener to close the popover; a
//!   transparent full-viewport `.chip-backdrop` behind it does the same job
//!   with an `onclick`, avoiding a `web_sys` document listener for one
//!   button.
//! - `effectiveModel` (what an in-flight run is ACTUALLY on, which can
//!   differ from the pin for one image turn) is not threaded through from
//!   `ConversationView` - `types::ConversationView`'s own doc already marks
//!   that field out of scope. The chip shows the pin (or the platform
//!   default) instead.

use crate::api;
use crate::types::{Bot, CatalogEntry};
use dioxus::prelude::*;

/// Ported from `Roster.tsx::shortModel`: everything after the first `/`.
pub fn short_model(id: &str) -> &str {
    match id.find('/') {
        Some(i) => &id[i + 1..],
        None => id,
    }
}

const EFFORTS: [&str; 3] = ["low", "medium", "high"];

fn effort_label(effort: &str) -> &'static str {
    match effort {
        "low" => "Low",
        "high" => "High",
        _ => "Medium",
    }
}

fn effort_description(effort: &str) -> &'static str {
    match effort {
        "low" => "Fast responses with lighter reasoning",
        "high" => "Takes longer and thinks harder",
        _ => "Balanced",
    }
}

const BOLT_ICON: &str = r##"<svg viewBox="0 0 16 16" width="13" height="13" fill="currentColor" aria-hidden="true"><path d="M8.6 1 3 9.2h3.4L6.2 15 13 6.4H9.4L8.6 1Z" /></svg>"##;
const RESET_ICON: &str = r##"<svg viewBox="0 0 16 16" width="13" height="13" fill="none" stroke="currentColor" stroke-width="1.4" aria-hidden="true"><path d="M13 8A5 5 0 1 1 11.2 4.2" stroke-linecap="round" /><path d="M13 2.5V5.5H10" stroke-linecap="round" stroke-linejoin="round" /></svg>"##;

/// Applies one PATCH and reflects the result (or rolls a refusal back to
/// what the server actually holds) - a plain function rather than a shared
/// closure, same reasoning `approvals.rs::fire_decide` gives: several call
/// sites need their own copy, and the values captured are not `Copy`.
#[allow(clippy::too_many_arguments)]
fn save(
    bot_id: String,
    body: serde_json::Value,
    mut effort: Signal<String>,
    mut model_value: Signal<Option<String>>,
    mut saving: Signal<bool>,
    mut error: Signal<Option<String>>,
    on_saved: EventHandler<Bot>,
    revert_effort: String,
    revert_model: Option<String>,
) {
    saving.set(true);
    error.set(None);
    spawn(async move {
        match api::patch_bot(&bot_id, body).await {
            Ok(updated) => {
                effort.set(updated.effort.clone());
                model_value.set(updated.model.clone());
                on_saved.call(updated);
            }
            Err(err) => {
                error.set(Some(err));
                effort.set(revert_effort);
                model_value.set(revert_model);
            }
        }
        saving.set(false);
    });
}

/// The header chip, and what opens above it. `on_saved` fires with the
/// server's own bot row on every successful PATCH, so a caller holding a
/// roster copy can update it (`app.rs` does not yet - see this ticket's
/// `## Result` for why that is a known, scoped gap).
#[component]
pub fn ModelChip(bot: Bot, on_saved: EventHandler<Bot>) -> Element {
    let mut open = use_signal(|| false);
    let mut effort = use_signal(|| bot.effort.clone());
    let mut model_value = use_signal(|| bot.model.clone());
    let saving = use_signal(|| false);
    let error = use_signal(|| None::<String>);
    let mut default_model = use_signal(|| None::<String>);

    // The bot underneath can change (switching bots) - pick that up rather
    // than freezing on whatever was true when this first mounted. Ported
    // from the TS `useEffect` keyed on `bot.id`/`bot.effort`/`bot.model`.
    let bot_id = bot.id.clone();
    let bot_effort = bot.effort.clone();
    let bot_model = bot.model.clone();
    use_effect(move || {
        let _ = &bot_id;
        effort.set(bot_effort.clone());
        model_value.set(bot_model.clone());
    });

    use_effect(move || {
        spawn(async move {
            if let Ok(m) = api::fetch_default_model().await {
                default_model.set(Some(m));
            }
        });
    });

    let shown_model = model_value
        .read()
        .clone()
        .or_else(|| default_model.read().clone())
        .unwrap_or_default();
    let suffix = if bot.effort != "medium" {
        format!(" · {}", effort_label(&bot.effort))
    } else {
        String::new()
    };
    let is_default = bot.model.is_none();

    let bot_id_for_reset = bot.id.clone();
    let reset = move |_| {
        save(
            bot_id_for_reset.clone(),
            serde_json::json!({ "effort": "medium", "model": null }),
            effort,
            model_value,
            saving,
            error,
            on_saved,
            effort.read().clone(),
            model_value.read().clone(),
        );
    };

    let bot_id_for_slider = bot.id.clone();
    let on_slider = move |evt: FormEvent| {
        let idx: usize = evt.value().parse().unwrap_or(1);
        let next = EFFORTS.get(idx).copied().unwrap_or("medium").to_string();
        let revert = effort.read().clone();
        effort.set(next.clone());
        save(
            bot_id_for_slider.clone(),
            serde_json::json!({ "effort": next }),
            effort,
            model_value,
            saving,
            error,
            on_saved,
            revert,
            model_value.read().clone(),
        );
    };

    let bot_id_for_model = bot.id.clone();
    let on_pick_model = move |id: String| {
        let revert = model_value.read().clone();
        model_value.set(Some(id.clone()));
        save(
            bot_id_for_model.clone(),
            serde_json::json!({ "model": id }),
            effort,
            model_value,
            saving,
            error,
            on_saved,
            effort.read().clone(),
            revert,
        );
    };

    rsx! {
        div { class: "model-chip",
            button {
                class: "chip mono chip-model",
                "aria-expanded": "{open}",
                "aria-haspopup": "dialog",
                title: "{shown_model}",
                onclick: move |_| {
                    let next = !*open.read();
                    open.set(next);
                },
                "{short_model(&shown_model)}{suffix}"
                if is_default {
                    i { class: "chip-default", "default" }
                }
            }

            if *open.read() {
                div {
                    class: "chip-backdrop",
                    onclick: move |_| open.set(false),
                }
                div { class: "effort-pop", role: "dialog", "aria-label": "Model and reasoning effort",
                    div { class: "effort-pop-head",
                        span { class: "effort-glyph effort-glyph-bolt", "aria-hidden": "true", dangerous_inner_html: "{BOLT_ICON}" }
                        div { class: "effort-pop-title",
                            h3 { "{effort_label(&effort.read())}" }
                            p { class: "mono muted", "{short_model(&shown_model)}" }
                        }
                        button {
                            class: "effort-glyph effort-glyph-reset",
                            disabled: *saving.read(),
                            title: "Reset to Medium and the platform default model",
                            "aria-label": "Reset to Medium and the platform default model",
                            onclick: reset,
                            dangerous_inner_html: "{RESET_ICON}",
                        }
                    }

                    div { class: "effort-slider",
                        input {
                            r#type: "range",
                            class: "effort-slider-input",
                            min: "0",
                            max: "2",
                            step: "1",
                            value: "{EFFORTS.iter().position(|e| *e == effort.read().as_str()).unwrap_or(1)}",
                            disabled: *saving.read(),
                            "aria-label": "Reasoning effort",
                            oninput: on_slider,
                        }
                        div { class: "effort-ticks", "aria-hidden": "true",
                            span {}
                            span {}
                            span {}
                        }
                    }
                    p { class: "effort-description", "{effort_description(&effort.read())}" }

                    if let Some(err) = error.read().clone() {
                        p { class: "effort-error", "{err}" }
                    }

                    ModelSelect { value: shown_model.clone(), on_change: on_pick_model }
                }
            }
        }
    }
}

/// Port of `ModelPicker.tsx`'s `MainstreamModelSelect`: a plain `<select>`
/// fed by `/api/models`, opening on the curated (mainstream) list; picking
/// "Show all…" swaps its own options to the full catalogue in place.
#[component]
fn ModelSelect(value: String, on_change: EventHandler<String>) -> Element {
    let mut show_all = use_signal(|| false);
    let mut models = use_signal(Vec::<CatalogEntry>::new);

    use_effect(move || {
        let all = *show_all.read();
        spawn(async move {
            if let Ok(body) = api::fetch_models("", all).await {
                models.set(body.models);
            }
        });
    });

    const SHOW_ALL: &str = "__bullpen_show_all__";

    rsx! {
        select {
            class: "mainstream-select",
            value: "{value}",
            onchange: move |evt| {
                let v = evt.value();
                if v == SHOW_ALL {
                    show_all.set(true);
                } else {
                    on_change.call(v);
                }
            },
            // The current value might not be in the list yet (mainstream
            // scope, the fetch has not landed) - an option-less selected
            // value renders blank, which reads as "no model chosen" when
            // one plainly is.
            if !value.is_empty() && !models.read().iter().any(|m| m.id == value) {
                option { value: "{value}", "{short_model(&value)}" }
            }
            for m in models.read().iter() {
                option { key: "{m.id}", value: "{m.id}", "{short_model(&m.id)}" }
            }
            if !*show_all.read() {
                option { value: SHOW_ALL, "Show all…" }
            }
        }
    }
}
