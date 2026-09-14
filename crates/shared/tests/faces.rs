use shared::faces::normalize_shape;

#[test]
fn test_normalize_shape_none_returns_default_for_arthur() {
    // Hash computed from projects/bullpen-night/src/shared/faces.ts (node -e):
    // h = 0; for each char c in "Arthur": h = (h * 31 + c.charCodeAt(0)) % 9973
    // Result: h=8312, 8312 % 8 = 0 → "circle"
    let result = normalize_shape(None, "Arthur");
    assert_eq!(result, "circle");
}

#[test]
fn test_normalize_shape_hexagon_when_explicitly_set() {
    assert_eq!(normalize_shape(Some("hexagon"), "TestBot"), "hexagon");
}

#[test]
fn test_normalize_shape_falls_back_for_unknown() {
    // Hash computed from projects/bullpen-night/src/shared/faces.ts (node -e):
    // h = 0; for each char c in "TestBot": h = (h * 31 + c.charCodeAt(0)) % 9973
    // Result: h=7240, 7240 % 8 = 0 → "circle"
    let result = normalize_shape(Some("unknown_shape"), "TestBot");
    assert_eq!(result, "circle");
}
