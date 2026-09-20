//! Port of `test/import-skills.test.ts` vectors.

use shared::parse_skill_file;

#[test]
fn plain_one_line_description() {
    let parsed =
        parse_skill_file("---\nname: caveman\ndescription: Short replies.\n---\n\nBody here.");
    assert_eq!(parsed.as_ref().map(|p| p.name.as_str()), Some("caveman"));
    assert_eq!(
        parsed.as_ref().map(|p| p.description.as_str()),
        Some("Short replies.")
    );
    assert_eq!(parsed.as_ref().map(|p| p.body.as_str()), Some("Body here."));
}

#[test]
fn folded_description() {
    let text = [
        "---",
        "name: ponytail",
        "description: >",
        "  Forces the laziest",
        "  solution that works.",
        "---",
        "",
        "The body.",
    ]
    .join("\n");
    let parsed = parse_skill_file(&text).expect("parsed");
    assert_eq!(
        parsed.description,
        "Forces the laziest solution that works."
    );
    assert_eq!(parsed.body, "The body.");
}

#[test]
fn literal_block_description() {
    let text = [
        "---",
        "name: x",
        "description: |",
        "  One",
        "  Two",
        "---",
        "",
        "b",
    ]
    .join("\n");
    let parsed = parse_skill_file(&text).expect("parsed");
    assert_eq!(parsed.description, "One Two");
}

#[test]
fn strips_surrounding_quotes() {
    let parsed =
        parse_skill_file("---\nname: x\ndescription: \"Quoted.\"\n---\nb").expect("parsed");
    assert_eq!(parsed.description, "Quoted.");
}

#[test]
fn body_not_read_as_front_matter() {
    let parsed = parse_skill_file("---\nname: x\ndescription: D\n---\n\nname: not-this\nbody text")
        .expect("parsed");
    assert_eq!(parsed.name, "x");
    assert!(parsed.body.contains("name: not-this"));
}

#[test]
fn null_without_front_matter() {
    assert!(parse_skill_file("# Just a document\n\nNo front matter.").is_none());
}

#[test]
fn survives_crlf() {
    let parsed =
        parse_skill_file("---\r\nname: x\r\ndescription: D\r\n---\r\nbody").expect("parsed");
    assert_eq!(parsed.name, "x");
    assert_eq!(parsed.description, "D");
}
