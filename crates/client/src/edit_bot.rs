//! EDIT-01: editing an existing bot's name/purpose/instructions - the hole
//! `new_bot.rs`'s own doc names ("It starts on the platform default model.
//! Give it instructions and a model next." - there was no next). Reuses
//! `new_bot.rs`'s modal shell (`.modal-scrim`/`.modal`/`.modal-head`/
//! `.modal-x`/`.modal-body`/`.room-picker-*`) rather than inventing a
//! fourth modal shape, and IMPORT-02/DUP-01's own `.room-picker-textarea`
//! class for the Instructions field - EDITABLE here (no `readonly`), unlike
//! the read-only preview textarea `new_bot.rs` added that class for.
//!
//! `voice` stays out on purpose: it is S11's device-voice field, and
//! nothing in this port writes it anywhere except `store::duplicate_bot`
//! (which only ever carries an existing value ACROSS to a copy, never sets
//! one from scratch) - adding a writer here for a field with no feature
//! behind it yet would be a column edit pretending to be a feature.
//!
//! On success this hands the updated `Bot` back through `on_saved`.
//! `thread.rs`'s own call site both seeds its `local_bot` from it directly
//! (the same "seed a local copy from the server's response" posture every
//! other header control there already takes - pin/hide/move/avatar/shape/
//! `ModelChip`) AND fires `on_rail_changed` (a name change is visible on
//! the RAIL too, which `local_bot` alone does not reach - the rail's own
//! bot list comes from `app.rs`'s separate roster fetch). Not
//! `on_duplicated`: that prop exists specifically because duplicating
//! creates a NEW bot Josh needs navigated to; editing changes the bot
//! already open, so there is nothing new to select.

use crate::api;
use crate::types::{Bot, SkillSummary};
use dioxus::prelude::*;
use std::collections::HashSet;

#[component]
pub fn EditBotModal(bot: Bot, on_close: EventHandler<()>, on_saved: EventHandler<Bot>) -> Element {
    let mut name = use_signal(|| bot.name.clone());
    let mut purpose = use_signal(|| bot.purpose.clone());
    let mut instructions = use_signal(|| bot.instructions.clone());
    let mut saving = use_signal(|| false);
    let mut error = use_signal(|| None::<String>);

    // Same guard the server itself applies (`routes/bots.rs::patch_bot`'s
    // own doc on `name`): a blank/whitespace-only name means "no change",
    // not something to send. Disabling Save on it here just saves Josh a
    // round trip for a PATCH that would silently keep the old name anyway.
    let save_disabled = name.read().trim().is_empty() || *saving.read();

    let bot_id = bot.id.clone();
    let submit = move |_| {
        if *saving.read() {
            return;
        }
        let trimmed_name = name.read().trim().to_string();
        if trimmed_name.is_empty() {
            error.set(Some("name is required".to_string()));
            return;
        }
        let purpose_value = purpose.read().clone();
        let instructions_value = instructions.read().clone();
        let id = bot_id.clone();
        saving.set(true);
        error.set(None);
        spawn(async move {
            let result =
                api::update_bot_identity(&id, &trimmed_name, &purpose_value, &instructions_value)
                    .await;
            saving.set(false);
            match result {
                Ok(updated) => on_saved.call(updated),
                Err(err) => error.set(Some(err)),
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
                "aria-label": "Edit bot",
                onclick: move |evt| evt.stop_propagation(),
                div { class: "modal-head",
                    h2 { "Edit bot" }
                    button {
                        class: "modal-x",
                        "aria-label": "Close",
                        onclick: move |_| on_close.call(()),
                        "×"
                    }
                }
                div { class: "modal-body",
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
                    label { class: "room-picker-title",
                        "Instructions"
                        textarea {
                            class: "room-picker-textarea",
                            value: "{instructions}",
                            "aria-label": "Instructions",
                            oninput: move |evt| instructions.set(evt.value()),
                        }
                    }
                    SkillsField { bot_id: bot.id.clone() }
                    if let Some(msg) = error.read().clone() {
                        p { class: "notice-inline", "{msg}" }
                    }
                    div { class: "room-picker-actions",
                        button { class: "thread-new", onclick: move |_| on_close.call(()), "Cancel" }
                        button {
                            class: "thread-new room-picker-create",
                            disabled: save_disabled,
                            onclick: submit,
                            if *saving.read() { "Saving…" } else { "Save" }
                        }
                    }
                }
            }
        }
    }
}

/* ------------------------------------------------------------- S10-02 */

/// S10-02: which skills this bot has - a field inside the Edit modal above,
/// the way the TS mounts `BotSkills` inside `BotEditor` (this client has no
/// separate `BotEditor`, so the Edit modal itself is where it lands).
///
/// 🔴 Opt-IN, one checkbox each, and NO "enable all" control -
/// `store::skills::skills_for`'s own doc: 37 enabled skills is an index too
/// long for the model to read, so it skims and picks nothing. A bot with
/// four relevant skills actually uses them.
///
/// 🔴 A toggle's new state always comes from `api::set_bot_skill`'s
/// response, never assumed from the click that sent it - a checkbox that
/// paints itself on after a failed request is a skill Josh believes a bot
/// has and it does not. `toggle` below sends the intended `next` (the same
/// "flip what we already believe" the TS original's own `e.target.checked`
/// reads off the DOM), but the answer that actually reaches `on` is decided
/// entirely by `skill_set_after_response` below, from the raw response
/// alone - see its own doc for why that function does not even have a
/// parameter it could use to paint an optimistic guess.
#[component]
fn SkillsField(bot_id: String) -> Element {
    let mut all = use_signal(|| None::<Vec<SkillSummary>>);
    let mut on = use_signal(HashSet::<String>::new);
    let mut load_error = use_signal(|| None::<String>);
    let mut toggle_error = use_signal(|| None::<String>);
    let mut busy = use_signal(|| None::<String>);

    use_effect({
        let bot_id = bot_id.clone();
        move || {
            let fetch_all_id = bot_id.clone();
            spawn(async move {
                match api::fetch_skills().await {
                    Ok(list) => all.set(Some(list)),
                    Err(err) => load_error.set(Some(err)),
                }
            });
            let fetch_on_id = fetch_all_id.clone();
            spawn(async move {
                if let Ok(names) = api::fetch_bot_skills(&fetch_on_id).await {
                    on.set(names.into_iter().collect());
                }
            });
        }
    });

    let toggle = {
        let bot_id = bot_id.clone();
        move |name: String, next: bool| {
            let bot_id = bot_id.clone();
            busy.set(Some(name.clone()));
            toggle_error.set(None);
            spawn(async move {
                let response = api::set_bot_skill(&bot_id, &name, next).await;
                if let Err(err) = &response {
                    toggle_error.set(Some(err.clone()));
                }
                // Computed once, from the raw response alone - see
                // `skill_set_after_response`'s own doc on why `on` is only
                // ever touched when this comes back `Some`.
                if let Some(next_on) = skill_set_after_response(response) {
                    on.set(next_on);
                }
                busy.set(None);
            });
        }
    };

    let list = all.read().clone();
    let enabled = on.read().clone();
    let busy_name = busy.read().clone();

    rsx! {
        div { class: "field",
            span { "Skills" }
            small { "Adds one line to this bot's prompt per skill: its name and when to use it." }

            if let Some(err) = load_error.read().clone() {
                p { class: "notice-inline", "{err}" }
            }
            if let Some(err) = toggle_error.read().clone() {
                p { class: "notice-inline", "{err}" }
            }

            if list.is_none() {
                p { class: "muted", "Loading skills…" }
            }

            if let Some(list) = list {
                if list.is_empty() {
                    p { class: "muted", "No skills to enable yet. See Settings \u{2192} Skills." }
                } else {
                    div { class: "botskill-list",
                        for skill in list.iter() {
                            label { key: "{skill.id}", class: "botskill",
                                input {
                                    r#type: "checkbox",
                                    checked: enabled.contains(&skill.name),
                                    disabled: busy_name.as_deref() == Some(skill.name.as_str()),
                                    onchange: {
                                        let mut toggle = toggle.clone();
                                        let name = skill.name.clone();
                                        let currently_on = enabled.contains(&skill.name);
                                        move |_| toggle(name.clone(), !currently_on)
                                    },
                                }
                                span { class: "botskill-name", "{skill.name}" }
                                span { class: "botskill-when", "{skill.description}" }
                            }
                        }
                    }
                }
            }
        }
    }
}

/// The enabled set to show after a toggle response, or `None` when nothing
/// on screen should change. This is the whole decision, both outcomes -
/// there is no `before`/`name`/`requested_on` parameter carrying the
/// click's own guess, because a correct answer here never consults it: the
/// function CANNOT paint an optimistic state, because it was never given
/// one to paint (a signature that accepted the click's guess and then had
/// to remember not to use it is exactly the shape that produced S10-02's
/// first, hollow version of this test - see that revision's own history).
///
/// 🔴 `Err(_)` -> `None` is the one rule that matters: a failed request
/// must leave the checkbox exactly as it was, never invent a state from
/// what was clicked - a checkbox that paints itself on after a failed
/// request is a skill Josh believes a bot has and it does not.
///
/// `Ok(names)` -> `Some(names collected)`, even when `names` is empty:
/// "the server says this bot now has zero skills" and "the request
/// failed" are different facts, so the empty case must still be `Some`,
/// never collapse into the same `None` a failure returns.
fn skill_set_after_response(response: Result<Vec<String>, String>) -> Option<HashSet<String>> {
    match response {
        Ok(names) => Some(names.into_iter().collect()),
        Err(_) => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `Ok` is authoritative, in full - the server's list becomes the set,
    /// with nothing about a previous click or a previous state folded in
    /// (there is nothing here TO fold in - see the function's own doc).
    #[test]
    fn ok_response_becomes_exactly_the_servers_set() {
        let server_names = vec!["a".to_string(), "b".to_string()];

        let after = skill_set_after_response(Ok(server_names.clone()));

        assert_eq!(
            after,
            Some(server_names.into_iter().collect::<HashSet<_>>()),
            "the resulting set must be exactly the server's list"
        );
    }

    /// 🔴 THE ONE THAT MATTERS: a failed request must change nothing. This
    /// is the ticket's own single most important rule, and the one case
    /// the crate's earlier, decision-free version of this function could
    /// never actually exercise.
    #[test]
    fn err_response_means_do_not_touch_the_set() {
        let after = skill_set_after_response(Err("network blip".to_string()));

        assert_eq!(
            after, None,
            "a failed request must leave the enabled set untouched, never invent one"
        );
    }

    /// An empty `Ok` is a real, distinct fact ("this bot now has zero
    /// skills enabled") - not the same thing as a failure, and a naive
    /// `Option<Vec<_>>`-shaped implementation (treating an empty vec as
    /// falsy) would collapse the two. `Some(empty)` here is what keeps
    /// them apart.
    #[test]
    fn ok_with_an_empty_list_is_some_empty_not_none() {
        let after = skill_set_after_response(Ok(Vec::new()));

        assert_eq!(
            after,
            Some(HashSet::new()),
            "an empty server answer is a real answer, not a failure"
        );
    }
}
