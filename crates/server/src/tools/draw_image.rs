//! S12-03: `draw_image` tool — OpenRouter image generation into the library.

use std::sync::Arc;

use model::ToolSpec;
use store::{Db, NewMessage, get_attachment, get_or_create_conversation, read_attachment};

use crate::imagegen::{
    AspectRatio, GenerateOptions, ImageGenHttp, data_url_for, generate_image, is_image_content_type,
};

/// Two 25 MB attachments would be a 66 MB request after base64.
pub const MAX_REFERENCE_BYTES: usize = 4 * 1024 * 1024;

pub fn spec() -> ToolSpec {
    ToolSpec {
        name: "draw_image".to_string(),
        description: "Make a picture from a description and post it into this chat. Say what should be in it, the style, and the shape - a card graphic, a banner, a logo sketch. Each picture costs about 3 cents of Josh's budget, so draw one only when he asked for it - never to illustrate an answer he did not request.".to_string(),
        parameters: serde_json::json!({
            "type": "object",
            "properties": {
                "description": { "type": "string", "description": "What the picture should show, in detail." },
                "aspect_ratio": {
                    "type": "string",
                    "enum": ["square", "landscape", "portrait"],
                    "description": "The shape of the picture: square (1:1), landscape (16:9), or portrait (9:16). Default square."
                },
                "reference_attachment_ids": {
                    "type": "array",
                    "items": { "type": "string" },
                    "maxItems": 2,
                    "description": "IDs of images already in this chat to draw from or in the style of. Maximum 2. Optional."
                }
            },
            "required": ["description"]
        }),
    }
}

pub async fn run(
    db: &Arc<std::sync::Mutex<Db>>,
    data_dir: &str,
    bot_id: &str,
    args: &str,
    http: Option<&dyn ImageGenHttp>,
) -> String {
    let input: serde_json::Value =
        serde_json::from_str(args).unwrap_or_else(|_| serde_json::json!({}));
    let description = input
        .get("description")
        .and_then(|v| v.as_str())
        .unwrap_or("");

    let aspect_ratio = input
        .get("aspect_ratio")
        .and_then(|v| v.as_str())
        .and_then(|s| match s {
            "landscape" => Some(AspectRatio::Landscape),
            "portrait" => Some(AspectRatio::Portrait),
            "square" => Some(AspectRatio::Square),
            _ => None,
        });

    let ref_ids: Vec<String> = input
        .get("reference_attachment_ids")
        .and_then(|v| v.as_array())
        .map(|arr| {
            arr.iter()
                .filter_map(|id| id.as_str().map(str::to_string))
                .collect()
        })
        .unwrap_or_default();

    if ref_ids.len() > 2 {
        return "Maximum 2 reference images allowed, but more were provided.".into();
    }

    let references = match load_reference_images(db, data_dir, ref_ids) {
        Ok(refs) => refs,
        Err(msg) => return msg,
    };

    let refs_slice = if references.is_empty() {
        None
    } else {
        Some(references.as_slice())
    };

    let outcome = generate_image(
        db,
        data_dir,
        description,
        GenerateOptions {
            model: None,
            api_key: None,
            timeout_ms: 90_000,
            aspect_ratio,
            references: refs_slice,
            bot_id: Some(bot_id),
            http,
        },
    )
    .await;

    if !outcome.ok {
        return outcome.detail;
    }
    let Some(attachment) = outcome.attachment else {
        return outcome.detail;
    };

    let guard = db.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
    let conv = match get_or_create_conversation(&guard, bot_id) {
        Ok(id) => id,
        Err(_) => {
            return format!(
                "Drawn, but could not post into the chat. {}",
                outcome.detail
            );
        }
    };
    let content: String = description.chars().take(200).collect();
    if let Err(err) = store::append_message(
        &guard,
        &conv,
        "assistant",
        &content,
        NewMessage {
            model: None,
            error: None,
            attachment_id: Some(attachment.id.clone()),
            bot_id: Some(bot_id.to_string()),
            usage: None,
        },
    ) {
        return format!("Drawn, but could not post into the chat: {err}");
    }
    drop(guard);

    format!("Drawn and posted into the chat. {}", outcome.detail)
}

fn load_reference_images(
    db: &Arc<std::sync::Mutex<Db>>,
    data_dir: &str,
    ref_ids: Vec<String>,
) -> Result<Vec<crate::imagegen::ReferenceImage>, String> {
    let guard = db.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
    let mut ref_errors: Vec<String> = Vec::new();
    let mut references: Vec<crate::imagegen::ReferenceImage> = Vec::new();

    for id in ref_ids {
        let attachment = match get_attachment(&guard, &id) {
            Ok(Some(a)) => a,
            Ok(None) | Err(_) => {
                ref_errors.push(format!("Reference image {id} not found."));
                continue;
            }
        };

        if !is_image_content_type(&attachment.content_type) {
            ref_errors.push(format!(
                "Reference image {id} ({}) is not an image.",
                attachment.name
            ));
            continue;
        }

        match read_attachment(data_dir, &id) {
            Ok(bytes) => {
                if bytes.len() > MAX_REFERENCE_BYTES {
                    return Err(format!(
                        "Reference image {id} is too large to send ({} MB; the limit is {} MB).",
                        bytes.len() / 1024 / 1024,
                        MAX_REFERENCE_BYTES / 1024 / 1024
                    ));
                }
                references.push(crate::imagegen::ReferenceImage {
                    data_url: data_url_for(&bytes, &attachment.content_type),
                });
            }
            Err(_) => ref_errors.push(format!("Reference image {id} could not be read.")),
        }
    }

    if !ref_errors.is_empty() {
        return Err(ref_errors.join(" "));
    }
    Ok(references)
}
