//! Tab title badge logic. Port of `projects/bullpen-night/src/shared/attention.ts`.
//! S11-08 polls `GET /api/attention` in `app.rs` rather than recomputing from
//! roster memory (the server route exists from S11-06).

/// What the browser tab is called — count in front so truncation from the
/// right still leaves the badge visible when many tabs are open.
pub fn tab_title(count: i64, base: &str) -> String {
    if count > 0 {
        format!("({count}) {base}")
    } else {
        base.to_string()
    }
}

/// Sets `document.title` when a DOM is available (web and desktop webview).
pub fn apply_document_title(count: i64) {
    let title = tab_title(count, "Bullpen");
    if let Some(document) = web_sys::window().and_then(|w| w.document()) {
        document.set_title(&title);
    }
}

#[cfg(test)]
mod tests {
    use super::tab_title;

    #[test]
    fn tab_title_zero_is_base_only() {
        assert_eq!(tab_title(0, "Bullpen"), "Bullpen");
    }

    #[test]
    fn tab_title_positive_prefixes_count() {
        assert_eq!(tab_title(3, "Bullpen"), "(3) Bullpen");
    }
}
