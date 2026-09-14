//! Port of `projects/bullpen-night/src/client/Markdown.tsx` (47 lines):
//! renders a bot's answer as markdown.
//!
//! **Why build `rsx!` elements and never `dangerous_inner_html`.** The
//! original's own doc comment is the reason this exists: a bot's reply is
//! model-generated text, and a model can be talked into emitting `<script>`
//! or an `onerror` image. `react-markdown` builds React elements directly
//! and drops raw HTML by default, so that class of bug cannot exist. Piping
//! `pulldown-cmark`'s HTML renderer into `dangerous_inner_html` would
//! reopen exactly the hole the original went out of its way to avoid, so
//! this walks `pulldown-cmark`'s event stream and builds `rsx!` nodes one
//! event at a time instead - raw `Event::Html`/`Event::InlineHtml` is
//! silently dropped, same as `react-markdown`'s default.
//!
//! Links get `target="_blank" rel="noopener noreferrer"` (ported from the
//! `a` override) and a `javascript:`/`vbscript:`/`data:` href is dropped
//! rather than rendered live (ported from the doc comment's claim about
//! `react-markdown`, made true here explicitly since `pulldown-cmark` does
//! not filter hrefs itself). A table is wrapped in `div.md-table` (ported
//! from the `table` override) so a wide one scrolls inside itself.
//!
//! This renders ASSISTANT text only - see `bubble.rs`.
//!
//! One dependency added for this: `pulldown-cmark`.

use dioxus::prelude::*;
use pulldown_cmark::{Event, HeadingLevel, Options, Parser, Tag};

#[component]
pub fn Markdown(text: String) -> Element {
    let mut options = Options::empty();
    options.insert(Options::ENABLE_TABLES);
    options.insert(Options::ENABLE_STRIKETHROUGH);
    options.insert(Options::ENABLE_TASKLISTS);
    let mut events = Parser::new_ext(&text, options);
    let nodes = render_seq(&mut events);
    rsx! {
        div { class: "md", {nodes.into_iter()} }
    }
}

/// Renders one sequence of sibling nodes, stopping at (and consuming) the
/// first `Event::End` it sees or the end of the stream.
///
/// No need to check which tag an `End` closes: `pulldown-cmark`'s events are
/// always properly nested, so a `Start` always recurses into a fresh call of
/// this function for its children, and the first `End` THAT call sees is
/// necessarily the one matching the `Start` that opened it - anything
/// deeper was already consumed by a deeper recursive call.
fn render_seq<'a>(iter: &mut impl Iterator<Item = Event<'a>>) -> Vec<Element> {
    let mut out = Vec::new();
    while let Some(event) = iter.next() {
        match event {
            Event::End(_) => return out,
            Event::Start(Tag::Image {
                dest_url, title, ..
            }) => {
                let alt = collect_text(iter);
                let src = safe_href(&dest_url);
                out.push(rsx! {
                    img { src: "{src}", alt: "{alt}", title: "{title}" }
                });
            }
            Event::Start(tag) => {
                let children = render_seq(iter);
                out.push(render_tag(tag, children));
            }
            Event::Text(text) => out.push(rsx! { "{text}" }),
            Event::Code(text) => out.push(rsx! { code { "{text}" } }),
            Event::SoftBreak => out.push(rsx! { " " }),
            Event::HardBreak => out.push(rsx! { br {} }),
            Event::Rule => out.push(rsx! { hr {} }),
            Event::TaskListMarker(checked) => out.push(rsx! {
                input { r#type: "checkbox", checked, disabled: true }
            }),
            // Raw HTML (`Html`/`InlineHtml`) and footnote references are
            // dropped rather than rendered - see the module doc.
            _ => {}
        }
    }
    out
}

/// Like `render_seq`, but for a span whose ONLY job is to become an `alt`
/// string (an image's link text) - nested formatting is walked to stay
/// balanced with the stream but collapses to plain text.
fn collect_text<'a>(iter: &mut impl Iterator<Item = Event<'a>>) -> String {
    let mut text = String::new();
    while let Some(event) = iter.next() {
        match event {
            Event::End(_) => break,
            Event::Start(_) => {
                text.push_str(&collect_text(iter));
            }
            Event::Text(t) | Event::Code(t) => text.push_str(&t),
            Event::SoftBreak | Event::HardBreak => text.push(' '),
            _ => {}
        }
    }
    text
}

fn render_tag(tag: Tag, children: Vec<Element>) -> Element {
    match tag {
        Tag::Paragraph => rsx! { p { {children.into_iter()} } },
        Tag::Heading { level, .. } => match level {
            HeadingLevel::H1 => rsx! { h1 { {children.into_iter()} } },
            HeadingLevel::H2 => rsx! { h2 { {children.into_iter()} } },
            HeadingLevel::H3 => rsx! { h3 { {children.into_iter()} } },
            // `.md h3, .md h4` share one size in `thread.css`; H5/H6 collapse
            // into H4 rather than growing an unstyled tag.
            _ => rsx! { h4 { {children.into_iter()} } },
        },
        Tag::BlockQuote(_) => rsx! { blockquote { {children.into_iter()} } },
        Tag::CodeBlock(_) => rsx! { pre { code { {children.into_iter()} } } },
        Tag::List(None) => rsx! { ul { {children.into_iter()} } },
        Tag::List(Some(_)) => rsx! { ol { {children.into_iter()} } },
        Tag::Item => rsx! { li { {children.into_iter()} } },
        Tag::Emphasis => rsx! { em { {children.into_iter()} } },
        Tag::Strong => rsx! { strong { {children.into_iter()} } },
        Tag::Strikethrough => rsx! { s { {children.into_iter()} } },
        // A wide table scrolls inside itself rather than pushing the bubble
        // (and the whole conversation column) sideways - ported from
        // Markdown.tsx's `table` override.
        Tag::Table(_) => rsx! {
            div { class: "md-table",
                table { {children.into_iter()} }
            }
        },
        Tag::TableHead => rsx! { tr { {children.into_iter()} } },
        Tag::TableRow => rsx! { tr { {children.into_iter()} } },
        Tag::TableCell => rsx! { td { {children.into_iter()} } },
        Tag::Link {
            dest_url, title, ..
        } => {
            let href = safe_href(&dest_url);
            rsx! {
                a {
                    href: "{href}",
                    title: "{title}",
                    target: "_blank",
                    rel: "noopener noreferrer",
                    {children.into_iter()}
                }
            }
        }
        _ => rsx! { {children.into_iter()} },
    }
}

/// Drops a `javascript:`/`vbscript:`/`data:` href rather than rendering it
/// live - `pulldown-cmark` does not filter these itself, unlike
/// `react-markdown` (see the module doc).
fn safe_href(url: &str) -> String {
    let trimmed = url.trim();
    let lower = trimmed.to_ascii_lowercase();
    if lower.starts_with("javascript:")
        || lower.starts_with("vbscript:")
        || lower.starts_with("data:")
    {
        String::new()
    } else {
        trimmed.to_string()
    }
}
