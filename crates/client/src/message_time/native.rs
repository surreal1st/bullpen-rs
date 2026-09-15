//! The non-wasm half of `message_time`'s seam - S13a-01b, new for the
//! desktop build. See the parent module's doc for the headline: **desktop
//! time formatting is not locale-driven**, a fixed `en-US`-shaped format via
//! `chrono` on every OS, because `chrono` alone carries no locale/ICU data
//! and re-deriving real locale awareness from scratch here would be a
//! second, worse implementation of what `web.rs`'s `toLocaleTimeString`/
//! `toLocaleDateString` already do properly in the browser.
//!
//! `chrono::Local` (not a hardcoded offset) still resolves the OS's actual
//! timezone via `iana-time-zone`/`windows-link` (this crate's `chrono`
//! carries the `clock` feature by way of the workspace default), so "today"
//! and the clock time are still Josh's own local time - only the WORDING
//! around them (weekday/month names, AM/PM) is fixed English rather than
//! reading his OS locale.

use chrono::{DateTime, Datelike, Local};

fn parse(iso: &str) -> Option<DateTime<Local>> {
    DateTime::parse_from_rfc3339(iso)
        .ok()
        .map(|d| d.with_timezone(&Local))
}

/// The clock time on a message row, e.g. "7:05 PM". `%-I` drops the leading
/// zero on the hour the way `{ hour: "numeric" }` does on web; `%M`/`%p` are
/// already zero-padded/upper-case to match `web.rs`'s "2-digit"/AM-PM shape.
pub fn format_time(iso: &str) -> String {
    let Some(date) = parse(iso) else {
        return String::new();
    };
    date.format("%-I:%M %p").to_string()
}

/// A stable key for the LOCAL calendar day - see the parent module's doc on
/// why this goes through a real parsed timestamp rather than string-slicing
/// the stored UTC ISO string.
pub fn day_key(iso: &str) -> String {
    let Some(date) = parse(iso) else {
        return String::new();
    };
    date.format("%Y-%m-%d").to_string()
}

/// The divider label: "Today", "Yesterday", a weekday name inside the last
/// week, or a written date - same tiers `web.rs::format_day` uses, English
/// wording fixed rather than locale-read (see this module's doc).
pub fn format_day(iso: &str, now_iso: &str) -> String {
    let (Some(date), Some(now)) = (parse(iso), parse(now_iso)) else {
        return String::new();
    };
    let key = day_key(iso);
    if key == day_key(now_iso) {
        return "Today".to_string();
    }

    let yesterday = now - chrono::Duration::days(1);
    if key == yesterday.format("%Y-%m-%d").to_string() {
        return "Yesterday".to_string();
    }

    // Inside the last week, the weekday is what someone actually remembers.
    let days = (now.date_naive() - date.date_naive()).num_days();
    if days > 0 && days < 7 {
        return date.format("%A").to_string();
    }

    // The year only when it is not this one.
    if date.year() != now.year() {
        date.format("%a %B %-d, %Y").to_string()
    } else {
        date.format("%a %B %-d").to_string()
    }
}

/// The full thing, for a tooltip on the short time.
pub fn format_full(iso: &str) -> String {
    let Some(date) = parse(iso) else {
        return String::new();
    };
    date.format("%A, %B %-d, %Y at %-I:%M %p").to_string()
}
