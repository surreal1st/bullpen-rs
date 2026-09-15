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

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::{SecondsFormat, TimeZone, Utc};

    /// Builds a UTC ISO string for a WALL-CLOCK instant in the machine's
    /// actual local timezone, round-tripped through `Local` rather than a
    /// hardcoded offset. Every function under test re-derives `Local` from
    /// the ISO string it is given, so anchoring inputs this way makes the
    /// expected output (the hour/day/weekday we asked for) the same on any
    /// machine's timezone - the workstation's, meridian's, or CI's -
    /// instead of baking in one assumed offset that would silently pass
    /// here and fail elsewhere (or vice versa).
    fn iso_local(y: i32, mo: u32, d: u32, h: u32, mi: u32, s: u32) -> String {
        let local = Local
            .with_ymd_and_hms(y, mo, d, h, mi, s)
            .single()
            .expect("unambiguous local time (test dates avoid DST transitions)");
        local
            .with_timezone(&Utc)
            .to_rfc3339_opts(SecondsFormat::Millis, true)
    }

    #[test]
    fn format_time_uses_12_hour_clock_with_no_leading_zero_and_upper_am_pm() {
        assert_eq!(format_time(&iso_local(2026, 1, 15, 19, 5, 0)), "7:05 PM");
        assert_eq!(format_time(&iso_local(2026, 1, 15, 0, 0, 0)), "12:00 AM");
        assert_eq!(format_time(&iso_local(2026, 1, 15, 12, 0, 0)), "12:00 PM");
        assert_eq!(format_time(&iso_local(2026, 1, 15, 9, 30, 0)), "9:30 AM");
    }

    #[test]
    fn format_time_returns_empty_string_for_unparsable_input() {
        assert_eq!(format_time("not-a-timestamp"), "");
        assert_eq!(format_time(""), "");
    }

    #[test]
    fn day_key_reflects_the_reader_local_calendar_day_not_a_utc_slice() {
        // The module's whole reason to exist (see the parent doc): slicing
        // the first 10 characters of the stored UTC string instead of
        // resolving through the reader's local timezone puts anything late
        // enough in the evening on the wrong calendar day. Late evening
        // local time must still report ITS OWN date, not tomorrow's UTC
        // date (a naive UTC slice would drift ahead of local for any
        // positive UTC offset, and behind for any negative one).
        assert_eq!(day_key(&iso_local(2026, 1, 15, 23, 50, 0)), "2026-01-15");
        assert_eq!(day_key(&iso_local(2026, 1, 15, 0, 5, 0)), "2026-01-15");
    }

    #[test]
    fn day_key_is_stable_across_the_whole_local_day() {
        let midnight = day_key(&iso_local(2026, 3, 3, 0, 0, 1));
        let noon = day_key(&iso_local(2026, 3, 3, 12, 0, 0));
        let just_before_midnight = day_key(&iso_local(2026, 3, 3, 23, 59, 59));
        assert_eq!(midnight, "2026-03-03");
        assert_eq!(noon, "2026-03-03");
        assert_eq!(just_before_midnight, "2026-03-03");
    }

    #[test]
    fn format_day_reports_today() {
        let now = iso_local(2026, 6, 15, 20, 0, 0);
        let msg = iso_local(2026, 6, 15, 9, 0, 0);
        assert_eq!(format_day(&msg, &now), "Today");
    }

    #[test]
    fn format_day_reports_yesterday() {
        let now = iso_local(2026, 6, 15, 8, 0, 0);
        let msg = iso_local(2026, 6, 14, 23, 0, 0);
        assert_eq!(format_day(&msg, &now), "Yesterday");
    }

    #[test]
    fn format_day_reports_a_weekday_name_inside_the_last_week() {
        // Anchor is a Monday; 3 days back is the Friday before it. Inside
        // the last week (1-6 days back), a name is what someone actually
        // remembers, not a date.
        let now = iso_local(2026, 6, 15, 12, 0, 0); // Monday
        let msg = iso_local(2026, 6, 12, 12, 0, 0); // Friday
        assert_eq!(format_day(&msg, &now), "Friday");
    }

    #[test]
    fn format_day_reports_a_written_date_without_year_in_the_same_year() {
        let now = iso_local(2026, 6, 15, 12, 0, 0);
        let msg = iso_local(2026, 6, 1, 12, 0, 0); // 14 days back - past the weekday tier
        assert_eq!(format_day(&msg, &now), "Mon June 1");
    }

    #[test]
    fn format_day_includes_the_year_only_when_it_differs_from_now() {
        let now = iso_local(2026, 1, 5, 12, 0, 0);
        let msg = iso_local(2025, 12, 20, 12, 0, 0);
        assert_eq!(format_day(&msg, &now), "Sat December 20, 2025");
    }

    #[test]
    fn format_day_returns_empty_string_for_unparsable_input() {
        let now = iso_local(2026, 6, 15, 12, 0, 0);
        assert_eq!(format_day("garbage", &now), "");
        assert_eq!(format_day(&now, "garbage"), "");
    }

    #[test]
    fn format_full_spells_out_weekday_month_and_12_hour_time() {
        assert_eq!(
            format_full(&iso_local(2026, 1, 15, 19, 5, 0)),
            "Thursday, January 15, 2026 at 7:05 PM"
        );
    }

    #[test]
    fn format_full_returns_empty_string_for_unparsable_input() {
        assert_eq!(format_full("not-a-timestamp"), "");
    }
}
