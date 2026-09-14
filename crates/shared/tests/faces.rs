use shared::faces::{default_shape_for, normalize_shape};

#[test]
fn test_normalize_shape_none_returns_default_for_arthur() {
    let result = normalize_shape(None, "Arthur");
    let expected = default_shape_for("Arthur");
    assert_eq!(result, expected);
}

#[test]
fn test_normalize_shape_hexagon_when_explicitly_set() {
    assert_eq!(normalize_shape(Some("hexagon"), "TestBot"), "hexagon");
}

#[test]
fn test_normalize_shape_falls_back_for_unknown() {
    let result = normalize_shape(Some("unknown_shape"), "TestBot");
    let expected = default_shape_for("TestBot");
    assert_eq!(result, expected);
}
