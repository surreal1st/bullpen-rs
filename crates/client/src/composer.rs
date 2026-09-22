//! Port of the send path from `projects/bullpen-night/src/client/Composer.tsx`.
//! S12-09 adds one attachment chip and a file picker; dictation, drag-drop,
//! @/ popups, and call remain out of scope.

use crate::api::{self, AttachmentInfo};
use crate::transport;
use dioxus::prelude::*;

const FILE_INPUT_ID: &str = "bullpen-composer-file";

fn start_upload(
    name: String,
    content_type: String,
    bytes: Vec<u8>,
    mut attachment: Signal<Option<AttachmentInfo>>,
    mut uploading: Signal<bool>,
    mut error: Signal<Option<String>>,
) {
    error.set(None);
    uploading.set(true);
    transport::spawn_task(async move {
        match api::upload_attachment(&name, &content_type, bytes).await {
            Ok(info) => attachment.set(Some(info)),
            Err(e) => error.set(Some(e)),
        }
        uploading.set(false);
    });
}

#[component]
pub fn Composer(
    bot_name: String,
    #[props(default = false)] disabled: bool,
    on_send: EventHandler<(String, Option<String>)>,
) -> Element {
    let mut text = use_signal(String::new);
    let mut attachment = use_signal(|| None::<AttachmentInfo>);
    let uploading = use_signal(|| false);
    let mut error = use_signal(|| None::<String>);
    let mut file_generation = use_signal(|| 0u32);

    let mut submit = move || {
        if *uploading.read() || disabled {
            return;
        }
        let trimmed = text.read().trim().to_string();
        if trimmed.is_empty() {
            return;
        }
        let id = attachment.read().as_ref().map(|a| a.id.clone());
        text.set(String::new());
        attachment.set(None);
        error.set(None);
        on_send.call((trimmed, id));
    };

    rsx! {
        div { class: "composer-wrap",
            if let Some(err) = error.read().clone() {
                p { class: "composer-error", "{err}" }
            }
            if let Some(att) = attachment.read().clone() {
                div { class: "composer-attachment",
                    span { class: "composer-attachment-name", "{att.name}" }
                    button {
                        r#type: "button",
                        class: "composer-attach-clear",
                        title: "Remove attachment",
                        disabled: *uploading.read() || disabled,
                        onclick: move |_| attachment.set(None),
                        "×"
                    }
                }
            }
            input {
                key: "{file_generation()}",
                id: FILE_INPUT_ID,
                r#type: "file",
                class: "composer-file-input",
                onchange: move |_| {
                    read_selected_file(attachment, uploading, error);
                    file_generation.set(file_generation() + 1);
                },
            }
            div { class: "composer",
                button {
                    r#type: "button",
                    class: "composer-attach",
                    title: "Attach a file",
                    disabled: disabled || *uploading.read(),
                    onclick: move |_| open_file_picker(attachment, uploading, error),
                    if *uploading.read() { "…" } else { "+" }
                }
                textarea {
                    value: "{text}",
                    placeholder: "Message {bot_name}",
                    "aria-label": "Message {bot_name}",
                    rows: 1,
                    disabled: disabled || *uploading.read(),
                    oninput: move |evt| text.set(evt.value()),
                    onkeydown: move |evt| {
                        if evt.key() == Key::Enter && !evt.modifiers().shift() {
                            evt.prevent_default();
                            submit();
                        }
                    },
                }
                button {
                    r#type: "button",
                    disabled: disabled || *uploading.read(),
                    onclick: move |_| submit(),
                    "Send"
                }
            }
        }
    }
}

fn open_file_picker(
    attachment: Signal<Option<AttachmentInfo>>,
    uploading: Signal<bool>,
    error: Signal<Option<String>>,
) {
    #[cfg(target_arch = "wasm32")]
    {
        use wasm_bindgen::JsCast;
        let Some(document) = web_sys::window().and_then(|w| w.document()) else {
            return;
        };
        if let Some(el) = document.get_element_by_id(FILE_INPUT_ID) {
            if let Ok(input) = el.dyn_into::<web_sys::HtmlInputElement>() {
                let _ = input.click();
            }
        }
        let _ = (attachment, uploading, error);
    }

    #[cfg(not(target_arch = "wasm32"))]
    {
        transport::spawn_task(async move {
            let Some(handle) = rfd::AsyncFileDialog::new().pick_file().await else {
                return;
            };
            let path = handle.path().to_path_buf();
            let name = path
                .file_name()
                .and_then(|s| s.to_str())
                .unwrap_or("file")
                .to_string();
            let bytes = match std::fs::read(&path) {
                Ok(b) => b,
                Err(_) => return,
            };
            start_upload(
                name,
                "application/octet-stream".into(),
                bytes,
                attachment,
                uploading,
                error,
            );
        });
    }
}

#[cfg(target_arch = "wasm32")]
fn read_selected_file(
    attachment: Signal<Option<AttachmentInfo>>,
    uploading: Signal<bool>,
    error: Signal<Option<String>>,
) {
    use wasm_bindgen::JsCast;
    let Some(document) = web_sys::window().and_then(|w| w.document()) else {
        return;
    };
    let Some(el) = document.get_element_by_id(FILE_INPUT_ID) else {
        return;
    };
    let Ok(input) = el.dyn_into::<web_sys::HtmlInputElement>() else {
        return;
    };
    let Some(files) = input.files() else {
        return;
    };
    if files.length() == 0 {
        return;
    }
    let Some(file) = files.get(0) else {
        return;
    };
    let name = file.name();
    let content_type = if file.type_().is_empty() {
        "application/octet-stream".to_string()
    } else {
        file.type_()
    };
    transport::spawn_task(async move {
        use wasm_bindgen_futures::JsFuture;
        let Ok(buffer) = JsFuture::from(file.array_buffer()).await else {
            return;
        };
        let array = js_sys::Uint8Array::new(&buffer);
        let mut bytes = vec![0u8; array.length() as usize];
        array.copy_to(&mut bytes);
        start_upload(name, content_type, bytes, attachment, uploading, error);
    });
}

#[cfg(not(target_arch = "wasm32"))]
fn read_selected_file(
    _attachment: Signal<Option<AttachmentInfo>>,
    _uploading: Signal<bool>,
    _error: Signal<Option<String>>,
) {
}
