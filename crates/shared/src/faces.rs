//! The roster's faces.
//!
//! A face is always on duty: the SILHOUETTE says what kind of bot this is
//! at a glance, set per-bot; the EYES say what it is doing right now.
//!
//! Shapes are named for what they look like rather than for a role, because
//! Josh assigns the roles. A shield is not "the security shape" until he points
//! it at Trinity.

pub struct Shape {
    pub label: &'static str,
    /// The silhouette, in a 32x32 box.
    pub path: &'static str,
    /// How far down the eyes sit, as a fraction of the box.
    ///
    /// Not one number for every shape: a shield narrows towards the bottom and a
    /// diamond narrows at both ends, so eyes parked at a fixed height fall off
    /// the edge of one and float in the empty tip of another.
    pub eye_y: f64,
    /// Horizontal gap between the eyes, in box units.
    pub eye_gap: f64,
}

pub const SHAPES: &[(&str, Shape)] = &[
    (
        "circle",
        Shape {
            label: "Circle",
            path: "M16 1a15 15 0 1 0 0 30a15 15 0 1 0 0-30z",
            eye_y: 0.47,
            eye_gap: 5.2,
        },
    ),
    (
        "rounded",
        Shape {
            label: "Rounded",
            path: "M10 1.5h12a8.5 8.5 0 0 1 8.5 8.5v12a8.5 8.5 0 0 1-8.5 8.5h-12a8.5 8.5 0 0 1-8.5-8.5v-12a8.5 8.5 0 0 1 8.5-8.5z",
            eye_y: 0.47,
            eye_gap: 5.2,
        },
    ),
    (
        "hexagon",
        Shape {
            label: "Hexagon",
            path: "M16 1.4l12.1 7v15.2l-12.1 7-12.1-7V8.4z",
            eye_y: 0.47,
            eye_gap: 5.0,
        },
    ),
    (
        "shield",
        Shape {
            label: "Shield",
            path: "M16 1.5l12.5 4.4v10.6c0 7.3-5.2 12-12.5 14.9C8.7 28.5 3.5 23.8 3.5 16.5V5.9z",
            eye_y: 0.42,
            eye_gap: 5.0,
        },
    ),
    (
        "diamond",
        Shape {
            label: "Diamond",
            path: "M16 1.2l14.8 14.8L16 30.8L1.2 16z",
            eye_y: 0.5,
            eye_gap: 4.2,
        },
    ),
    (
        "leaf",
        Shape {
            label: "Leaf",
            path: "M3 29V15A12 12 0 0 1 15 3h14v14a12 12 0 0 1-12 12z",
            eye_y: 0.47,
            eye_gap: 5.0,
        },
    ),
    (
        "arch",
        Shape {
            label: "Arch",
            path: "M2 17a14 14 0 0 1 28 0v9a4 4 0 0 1-4 4H6a4 4 0 0 1-4-4z",
            eye_y: 0.45,
            eye_gap: 5.2,
        },
    ),
    (
        "chip",
        Shape {
            label: "Chip",
            path: "M9.5 1.5h13L30.5 9.5v13L22.5 30.5h-13L1.5 22.5v-13z",
            eye_y: 0.47,
            eye_gap: 5.2,
        },
    ),
];

/// The shape a bot gets before anyone chooses one.
///
/// Hashed from the name rather than everyone starting as a rounded square: a
/// roster imported in one go should arrive already distinguishable, which is
/// the same reason the colours are generated. Stable, so a bot does not change
/// face on reload.
pub fn default_shape_for(name: &str) -> &'static str {
    let mut h: u32 = 0;
    for b in name.bytes() {
        h = (h.wrapping_mul(31).wrapping_add(b as u32)) % 9973;
    }
    let idx = (h as usize) % SHAPES.len();
    SHAPES[idx].0
}

/// Anything stored, coerced to a shape that exists.
pub fn normalize_shape(value: Option<&str>, name: &str) -> &'static str {
    if let Some(v) = value {
        if let Some((k, _)) = SHAPES.iter().find(|(k, _)| k == &v) {
            return k;
        }
    }
    default_shape_for(name)
}
