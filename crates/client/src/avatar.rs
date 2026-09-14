//! Port of `projects/bullpen-night/src/client/Avatar.tsx:1-130`: a bot's
//! face. An emoji override wins when set; otherwise a generated SVG face
//! whose silhouette hue comes from the bot's section and whose eyes narrow
//! and scan while `busy`.

use dioxus::prelude::*;
use shared::faces::{SHAPES, normalize_shape};

/// Sanitise a bot id for use in SVG attribute ids. Keeps only [A-Za-z0-9_-],
/// replacing others with _. This prevents breaking out of the id="" attribute
/// with quotes or markup injection.
fn sanitise_id(id: &str) -> String {
    id.chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '_' || c == '-' {
                c
            } else {
                '_'
            }
        })
        .collect()
}

/// Stable small hash: same name, same angle, every reload and every machine.
fn spin(text: &str) -> i32 {
    let mut h: i64 = 0;
    for b in text.bytes() {
        h = (h * 31 + b as i64) % 360;
    }
    h as i32
}

/// Sections get hues spaced around the wheel rather than hashed, so two
/// sections cannot land on near-identical colours by accident. Bots within a
/// section vary by a few degrees off their section's hue.
pub fn hue_for(section_id: Option<&str>, section_ids: &[String], name: &str) -> i32 {
    let at = section_id.and_then(|id| section_ids.iter().position(|s| s == id));
    let base = match at {
        None => 220,
        Some(i) => (i as i32 * 360) / (section_ids.len().max(1) as i32),
    };
    (((base + (spin(name) % 24) - 12) % 360) + 360) % 360
}

/// Build the SVG face string for a given bot id and hue. The id MUST be
/// sanitised to prevent breaking out of the id="" attribute with markup injection.
#[cfg_attr(not(test), allow(dead_code))]
#[allow(clippy::too_many_arguments)]
fn build_face_svg(
    id: &str,
    hue: i32,
    hue2: i32,
    path: &str,
    size: f64,
    ex1: f64,
    ex2: f64,
    ey: f64,
) -> String {
    let fill_id = format!("face-{}", sanitise_id(id));
    format!(
        r##"<svg viewBox="0 0 32 32" width="{size}" height="{size}">
  <defs>
    <linearGradient id="{fill_id}" x1="0" y1="0" x2="0.6" y2="1">
      <stop offset="0" stop-color="hsl({hue} 60% 64%)" />
      <stop offset="1" stop-color="hsl({hue2} 56% 45%)" />
    </linearGradient>
  </defs>
  <path d="{path}" fill="url(#{fill_id})" />
  <g class="av-eyes" fill="#141018">
    <rect class="av-eye" x="{ex1}" y="{ey}" width="3" height="4.4" rx="1.5" />
    <rect class="av-eye" x="{ex2}" y="{ey}" width="3" height="4.4" rx="1.5" />
  </g>
</svg>"##
    )
}

#[component]
pub fn Avatar(
    id: String,
    name: String,
    section_id: Option<String>,
    section_ids: Vec<String>,
    #[props(default = false)] busy: bool,
    #[props(default)] avatar: Option<String>,
    #[props(default = 30.0)] size: f64,
    #[props(default)] shape: Option<String>,
) -> Element {
    let hue = hue_for(section_id.as_deref(), &section_ids, &name);

    // An emoji Josh chose beats the generated face.
    if let Some(emoji) = avatar {
        let bg = format!("hsl({hue} 26% 22%)");
        let style = format!(
            "width: {size}px; height: {size}px; font-size: {}px; background: {bg};",
            size * 0.55
        );
        return rsx! {
            span {
                class: "av av-emoji",
                style: "{style}",
                "aria-hidden": "true",
                "{emoji}"
            }
        };
    }

    // Get the shape for this bot
    let shape_name = normalize_shape(shape.as_deref(), &name);
    let shape_info = SHAPES
        .iter()
        .find(|(k, _)| k == &shape_name)
        .map(|(_, v)| v)
        .unwrap_or_else(|| {
            SHAPES
                .iter()
                .find(|(k, _)| k == &"rounded")
                .map(|(_, v)| v)
                .unwrap()
        });

    let eye_y = 32.0 * shape_info.eye_y;
    let gap = shape_info.eye_gap;
    let hue2 = (hue + 22) % 360;
    let ex1 = 16.0 - gap - 1.5;
    let ex2 = 16.0 + gap - 1.5;
    let ey = eye_y - 2.2;

    let svg = build_face_svg(&id, hue, hue2, shape_info.path, size, ex1, ex2, ey);

    let class = if busy {
        "av av-face is-busy"
    } else {
        "av av-face"
    };
    let style = format!("width: {size}px; height: {size}px;");

    rsx! {
        span {
            class: "{class}",
            style: "{style}",
            "aria-hidden": "true",
            dangerous_inner_html: "{svg}",
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_sanitise_id_removes_script_tags() {
        // A bot id that attempts to inject a script tag should be sanitised
        let malicious_id = r#"x"><script>"#;
        let sanitised = sanitise_id(malicious_id);

        // The result should not contain unescaped quotes or angle brackets
        assert!(!sanitised.contains('>'));
        assert!(!sanitised.contains('<'));
        assert!(!sanitised.contains('"'));
        assert!(!sanitised.contains('\''));

        // Only alphanumeric, underscore, and hyphen should remain
        for c in sanitised.chars() {
            assert!(
                c.is_ascii_alphanumeric() || c == '_' || c == '-',
                "Unexpected character in sanitised id: {}",
                c
            );
        }
    }

    #[test]
    fn test_sanitise_id_preserves_valid_chars() {
        // Valid bot id characters should be preserved
        let valid_id = "bot_id-123";
        let sanitised = sanitise_id(valid_id);
        assert_eq!(sanitised, valid_id);
    }

    #[test]
    fn test_sanitise_id_replaces_invalid_chars() {
        // Invalid characters should be replaced with underscore
        let id_with_spaces = "bot id";
        let sanitised = sanitise_id(id_with_spaces);
        assert_eq!(sanitised, "bot_id");

        let id_with_dots = "bot.id";
        let sanitised = sanitise_id(id_with_dots);
        assert_eq!(sanitised, "bot_id");
    }

    #[test]
    fn test_avatar_malicious_id_blocked_from_svg() {
        // Bite check: render the avatar for a bot id x"><script> and verify the
        // output contains no <script and no "><  - proves sanitisation is active.
        let malicious_id = r#"x"><script>"#;
        let hue = 180;
        let hue2 = 202;
        let path = "M16 0 C24.8 0 32 7.2 32 16 C32 24.8 24.8 32 16 32 C7.2 32 0 24.8 0 16 C0 7.2 7.2 0 16 0 Z";
        let size = 30.0;
        let ex1 = 12.0;
        let ex2 = 20.0;
        let ey = 10.0;

        let svg = build_face_svg(malicious_id, hue, hue2, path, size, ex1, ex2, ey);

        // The SVG must not contain the raw malicious pattern or script tags
        assert!(
            !svg.contains("<script"),
            "Avatar SVG must not contain <script (injection blocked)"
        );
        assert!(
            !svg.contains(r#""><"#),
            "Avatar SVG must not contain \">< (attribute escape blocked)"
        );
        assert!(
            !svg.contains(r#"x"><"#),
            "Avatar SVG must not contain raw malicious id"
        );
    }
}
