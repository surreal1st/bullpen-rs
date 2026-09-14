//! Port of `projects/bullpen-night/src/client/Avatar.tsx:1-130`: a bot's
//! face. An emoji override wins when set; otherwise a generated SVG face
//! whose silhouette hue comes from the bot's section and whose eyes narrow
//! and scan while `busy`.
//!
//! The shape-per-bot table (`SHAPES`/`normalizeShape` in `faces.ts`) is out
//! of this ticket's read range, so this ports one fixed rounded silhouette
//! rather than the real shape picker - S0-05 only needs a face that reads
//! as a face and animates when busy.

use dioxus::prelude::*;

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

/// One rounded silhouette (viewBox 0 0 32 32), stand-in until the shape
/// picker lands.
const FACE_PATH: &str = "M16 2 C24 2 30 8 30 16 C30 24 24 30 16 30 C8 30 2 24 2 16 C2 8 8 2 16 2 Z";

#[component]
pub fn Avatar(
    id: String,
    name: String,
    section_id: Option<String>,
    section_ids: Vec<String>,
    #[props(default = false)] busy: bool,
    #[props(default)] avatar: Option<String>,
    #[props(default = 30.0)] size: f64,
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

    let eye_y = 32.0 * 0.55_f64;
    let gap = 5.0_f64;
    let fill_id = format!("face-{id}");
    let hue2 = (hue + 22) % 360;
    let ex1 = 16.0 - gap - 1.5;
    let ex2 = 16.0 + gap - 1.5;
    let ey = eye_y - 2.2;

    let svg = format!(
        r##"<svg viewBox="0 0 32 32" width="{size}" height="{size}">
  <defs>
    <linearGradient id="{fill_id}" x1="0" y1="0" x2="0.6" y2="1">
      <stop offset="0" stop-color="hsl({hue} 60% 64%)" />
      <stop offset="1" stop-color="hsl({hue2} 56% 45%)" />
    </linearGradient>
  </defs>
  <path d="{FACE_PATH}" fill="url(#{fill_id})" />
  <g class="av-eyes" fill="#141018">
    <rect class="av-eye" x="{ex1}" y="{ey}" width="3" height="4.4" rx="1.5" />
    <rect class="av-eye" x="{ex2}" y="{ey}" width="3" height="4.4" rx="1.5" />
  </g>
</svg>"##
    );

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
