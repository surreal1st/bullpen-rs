//! S12-02: deliver tool and deliverables builders.

mod common;

use std::sync::Arc;

use server::deliverables::{
    DeliverDeps, build_pptx, build_xlsx, coerce_table, deliver_file, parse_outline,
    resolve_clip_source,
};
use store::read_attachment;

#[test]
fn resolve_clip_source_refuses_arbitrary_paths() {
    for bad in [
        "/etc/passwd",
        "/workspace/other-file.mp4",
        "/work/../etc/passwd",
    ] {
        assert!(resolve_clip_source(bad).is_err(), "{bad}");
    }
    assert!(resolve_clip_source("/work/recording.mp4").is_ok());
}

#[tokio::test]
async fn deliver_text_and_office_kinds_store_attachments() {
    let temp = tempfile::tempdir().expect("tempdir");
    let db = std::sync::Arc::new(std::sync::Mutex::new(
        store::Db::open(":memory:").expect("open"),
    ));
    {
        let guard = db.lock().unwrap();
        store::ensure_library_tables(&guard).expect("library");
        guard
            .conn()
            .execute(
                "INSERT INTO bots (id, name, purpose, instructions, model, created_at) VALUES ('riley', 'Riley', '', '', 'x', '2020-01-01')",
                [],
            )
            .expect("bot");
    }
    let data_dir = temp.path().to_str().unwrap().to_string();
    let deps = DeliverDeps {
        db: Arc::clone(&db),
        data_dir: data_dir.clone(),
        bot_id: "riley".into(),
        cdp: None,
        runner: None,
    };

    let text = deliver_file(
        deps.clone(),
        &serde_json::json!({"kind":"text","name":"notes","content":"hello world"}),
    )
    .await;
    assert!(text.ok);
    let id = text.attachment.as_ref().unwrap().id.clone();
    assert_eq!(
        read_attachment(&data_dir, &id).unwrap(),
        b"hello world".as_slice()
    );

    let sheet = deliver_file(
        deps.clone(),
        &serde_json::json!({
            "kind":"spreadsheet",
            "name":"report",
            "content":[["Name","Total"],["Widgets",12]]
        }),
    )
    .await;
    assert!(sheet.ok);
    assert_eq!(sheet.attachment.as_ref().unwrap().name, "report.xlsx");
    let bytes = read_attachment(&data_dir, &sheet.attachment.as_ref().unwrap().id).unwrap();
    assert_eq!(&bytes[0..2], b"PK");

    let deck = deliver_file(
        deps.clone(),
        &serde_json::json!({
            "kind":"deck",
            "name":"pitch",
            "content":"# Slide one\nsome point"
        }),
    )
    .await;
    assert!(deck.ok);
    assert_eq!(deck.attachment.as_ref().unwrap().name, "pitch.pptx");
}

#[test]
fn xlsx_and_outline_builders_match_ts_shapes() {
    let buf = build_xlsx(&[
        vec![serde_json::json!("Name"), serde_json::json!("Total")],
        vec![serde_json::json!("Widgets"), serde_json::json!(12)],
    ]);
    assert_eq!(&buf[0..2], b"PK");
    assert!(
        buf.windows(b"xl/workbook.xml".len())
            .any(|w| w == b"xl/workbook.xml")
    );

    let slides = parse_outline("# First\nline one\n\n# Second\nonly");
    assert_eq!(slides.len(), 2);
    let pptx = build_pptx(&slides);
    assert_eq!(&pptx[0..2], b"PK");

    let table = coerce_table(&serde_json::json!([["a", 1], ["b", 2]])).unwrap();
    assert_eq!(table.len(), 2);
}
