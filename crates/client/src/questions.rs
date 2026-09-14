//! Port of `projects/bullpen-night/src/client/Questions.tsx`: a question a
//! bot asked WITHOUT parking its run (`ask_josh { wait: false }`, S2-08's
//! default and the only form the Rust port implements - see
//! `crates/shared/src/ask_josh.rs`'s doc on why "not waiting" is the
//! default). A separate list from `approvals.rs`'s pending approvals on
//! purpose: answering one resumes nothing, it just posts Josh's answer into
//! the conversation as an ordinary message. Same placement as the approvals
//! pane - below the thread, above the composer (`App.tsx:1278-1300`) - and
//! `thread.rs` renders this one second, "because an approval is a run
//! frozen waiting for him and one of these is not. The blocking thing goes
//! first."
//!
//! `GET /api/questions` answers every open question across every bot
//! (`crates/server/src/routes/questions.rs`); this pane filters to the bot
//! whose conversation is open, same as the TS `mine` filter - "so a thread
//! shows what was asked IN it."

use crate::api;
use crate::events::{ChangeKind, subscribe_events};
use crate::types::OpenQuestion;
use dioxus::prelude::*;

async fn reload(mut items: Signal<Vec<OpenQuestion>>) {
    if let Ok(list) = api::fetch_questions().await {
        items.set(list);
    }
}

/// A plain function, not a shared closure - same reasoning as
/// `approvals.rs`'s `fire_decide`: an option button and the typed-answer
/// form both need their own copy of "answer and reload", and the `String`
/// captures involved are not `Copy`.
fn fire_answer(
    items: Signal<Vec<OpenQuestion>>,
    mut busy: Signal<bool>,
    id: String,
    answer: String,
) {
    busy.set(true);
    spawn(async move {
        let _ = api::answer_question(&id, &answer).await;
        busy.set(false);
        reload(items).await;
    });
}

#[component]
pub fn Questions(bot_id: String, bot_name: String) -> Element {
    let items = use_signal(Vec::<OpenQuestion>::new);

    use_effect(move || {
        wasm_bindgen_futures::spawn_local(reload(items));
    });

    // 🔴 `wasm_bindgen_futures::spawn_local`, not `dioxus::prelude::spawn` -
    // see `approvals.rs`'s identical subscription for why (`working_bar.rs`
    // documents the underlying wasm-abort bug this avoids).
    let _events = use_signal(move || {
        subscribe_events(move |kind| {
            if kind == ChangeKind::Questions {
                wasm_bindgen_futures::spawn_local(reload(items));
            }
        })
    });

    let bot_id_filter = bot_id.clone();
    let mine: Vec<OpenQuestion> = items
        .read()
        .iter()
        .filter(|q| q.bot_id == bot_id_filter)
        .cloned()
        .collect();

    if mine.is_empty() {
        return rsx! {};
    }
    let total = mine.len();

    rsx! {
        section { class: "approvals",
            h3 { "Asked you " i { "{total}" } }
            for q in mine {
                QuestionCard {
                    key: "{q.id}",
                    id: q.id.clone(),
                    bot_name: bot_name.clone(),
                    question: q.question.clone(),
                    options: q.options.clone(),
                    items,
                }
            }
        }
    }
}

#[component]
fn QuestionCard(
    id: String,
    bot_name: String,
    question: String,
    options: Vec<String>,
    items: Signal<Vec<OpenQuestion>>,
) -> Element {
    let busy = use_signal(|| false);
    let mut typed = use_signal(String::new);

    rsx! {
        article { class: "approval is-question",
            div { class: "approval-top",
                b { "{bot_name}" }
                // No "wants to run X": nothing is waiting on permission, and
                // saying so would make this read as blocking when it is not.
                span { class: "approval-cause", "asked while carrying on" }
            }
            p { class: "approval-question", "{question}" }
            div { class: "approval-answer",
                if !options.is_empty() {
                    div { class: "approval-options",
                        for option in options.clone() {
                            button {
                                key: "{option}",
                                disabled: *busy.read(),
                                onclick: {
                                    let id = id.clone();
                                    let option = option.clone();
                                    move |_| fire_answer(items, busy, id.clone(), option.clone())
                                },
                                "{option}"
                            }
                        }
                    }
                }
                form {
                    class: "approval-typed",
                    onsubmit: {
                        let id = id.clone();
                        move |evt: FormEvent| {
                            evt.prevent_default();
                            let text = typed.read().trim().to_string();
                            if text.is_empty() {
                                return;
                            }
                            typed.set(String::new());
                            fire_answer(items, busy, id.clone(), text);
                        }
                    },
                    input {
                        value: "{typed}",
                        disabled: *busy.read(),
                        placeholder: if options.is_empty() { "your answer" } else { "or say something else" },
                        oninput: move |evt| typed.set(evt.value()),
                        "aria-label": "Your answer",
                    }
                    button {
                        r#type: "submit",
                        disabled: *busy.read() || typed.read().trim().is_empty(),
                        "Send"
                    }
                }
            }
        }
    }
}
