//! Port of the send path and the Enter/Shift+Enter rule from
//! `projects/bullpen-night/src/client/Composer.tsx` (`onSend`, `textarea`,
//! `onKeyDown:212-216`). Everything else in that 900-line file - @/ popups,
//! attachments, dictation, drag-drop, the call button, voice - is out of
//! scope for S1-07a (see the ticket's SKIP list) and not ported.

use dioxus::prelude::*;

#[component]
pub fn Composer(
    bot_name: String,
    #[props(default = false)] disabled: bool,
    on_send: EventHandler<String>,
) -> Element {
    let mut text = use_signal(String::new);

    let mut submit = move || {
        let trimmed = text.read().trim().to_string();
        if trimmed.is_empty() || disabled {
            return;
        }
        text.set(String::new());
        on_send.call(trimmed);
    };

    rsx! {
        div { class: "composer",
            textarea {
                value: "{text}",
                placeholder: "Message {bot_name}",
                "aria-label": "Message {bot_name}",
                rows: 1,
                oninput: move |evt| text.set(evt.value()),
                onkeydown: move |evt| {
                    // Ported from `Composer.tsx:212-215`: Enter alone sends;
                    // Shift+Enter falls through with no `prevent_default`,
                    // so the browser's own textarea default inserts the
                    // newline - the original special-cases nothing for it
                    // either.
                    if evt.key() == Key::Enter && !evt.modifiers().shift() {
                        evt.prevent_default();
                        submit();
                    }
                },
            }
            button {
                r#type: "button",
                disabled,
                onclick: move |_| submit(),
                "Send"
            }
        }
    }
}
