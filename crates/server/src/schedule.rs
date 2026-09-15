//! Schedules, written the way a person says them.
//!
//! Deliberately not cron. Humans read and write these, and `43 8 * * 1-5` is a
//! thing you have to decode rather than read. Four shapes cover everything the
//! old roster actually did:
//!
//!   every 15 minutes
//!   hourly
//!   daily at 07:30
//!   weekdays at 08:43
//!
//! Times use the local time, because "08:43" means breakfast, not UTC.
//! `next_run` takes and returns `DateTime<Utc>` but performs comparisons and
//! increments using local time, the same way the TS code works.
//!
//! S5-F-02 (F3/F5/F12, `reviews/S5-R.md`): `Schedule` now derives
//! `Serialize`/`Deserialize` with a wire shape byte-equal to TS's own
//! `Schedule` union (`bullpen-night/src/server/schedule.ts:16-35`) -
//! `{"kind":"interval","minutes":15}`, `{"kind":"clock","days":[1],
//! "times":[{"hour":9,"minute":0}]}`, etc. - because `routines.schedule`
//! now holds that JSON (F3), not the typed phrase. `parse_schedule` accepts
//! EITHER shape: JSON first (a TS-written or bullpen-rs-written row), then
//! the human phrase grammar below (a fresh `POST`/`PATCH` body) - one
//! function, so `fire_due`/`resume_absence_paused`/`POST /:id/active`
//! (all outside this ticket's owned files) keep working unmodified once the
//! column's content changes underneath them.

use chrono::{DateTime, Datelike, Local, Timelike, Utc, Weekday};
use serde::{Deserialize, Serialize};

/// Represents a schedule in human-readable form. Wire shape matches TS's
/// `Schedule` interface exactly (`schedule.ts:16-35`) via an internally
/// tagged enum: `"kind"` first, then the variant's own fields in the same
/// order TS's object literals list them.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(
    tag = "kind",
    rename_all = "lowercase",
    rename_all_fields = "camelCase"
)]
pub enum Schedule {
    /// Every N minutes, where N >= 5.
    Interval {
        minutes: u32,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        from_hour: Option<u32>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        to_hour: Option<u32>,
    },
    /// Every hour on a specific minute.
    Hourly { minute: u32 },
    /// Once per day at a specific time.
    Daily { hour: u32, minute: u32 },
    /// On weekdays (Mon-Fri) at a specific time.
    Weekdays { hour: u32, minute: u32 },
    /// On specific days at specific times.
    Clock {
        days: Vec<u32>,
        times: Vec<ClockTime>,
    },
}

/// One fire time within a `Clock` schedule. A struct (not a tuple) because
/// TS's own `times` entries are `{ hour, minute }` objects
/// (`schedule.ts:31`) - a tuple would serialize as a bare `[9,0]` array and
/// a TS `JSON.parse` reader doing `time.hour`/`time.minute` would see
/// `undefined` for both.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct ClockTime {
    pub hour: u32,
    pub minute: u32,
}

const EXAMPLES: &str = "every 15 minutes, hourly, daily at 07:30, weekdays at 08:43";

const DAY_NAMES: &[&str] = &[
    "sunday",
    "monday",
    "tuesday",
    "wednesday",
    "thursday",
    "friday",
    "saturday",
];

/// Parses a schedule string into a `Schedule`.
///
/// Tries the JSON wire shape first (F3: a TS-written or bullpen-rs-written
/// `routines.schedule` column), falling back to the human phrase grammar
/// below (a freshly typed `POST`/`PATCH` body). This double duty is
/// deliberate - see the module doc for why.
pub fn parse_schedule(input: &str) -> Result<Schedule, String> {
    let trimmed = input.trim();
    if trimmed.starts_with('{')
        && let Ok(schedule) = serde_json::from_str::<Schedule>(trimmed)
    {
        return Ok(schedule);
    }

    let text = trimmed.to_lowercase();
    let text = text.split_whitespace().collect::<Vec<_>>().join(" ");

    if text.is_empty() {
        return Err(format!("Give a schedule, such as: {}", EXAMPLES));
    }

    if text == "hourly" || text == "every hour" {
        return Ok(Schedule::Hourly { minute: 0 });
    }

    // Try interval: "every N minutes" or "every N hours"
    if let Some(rest) = text.strip_prefix("every ") {
        let parts: Vec<&str> = rest.split(' ').collect();
        if parts.len() == 2
            && let Ok(n) = parts[0].parse::<u32>()
        {
            let unit = parts[1];
            if matches!(unit, "minute" | "minutes" | "min" | "hour" | "hours") {
                let minutes = if unit.starts_with("hour") { n * 60 } else { n };

                if minutes < 5 {
                    return Err("The shortest interval is 5 minutes.".to_string());
                }
                if minutes > 60 * 24 * 7 {
                    return Err("That is longer than a week.".to_string());
                }
                return Ok(Schedule::Interval {
                    minutes,
                    from_hour: None,
                    to_hour: None,
                });
            }
        }
    }

    // Try "daily at HH:MM" or "weekdays at HH:MM"
    if text.contains(" at ") {
        let at_idx = text.find(" at ").unwrap();
        let kind_part = &text[..at_idx];
        let time_part = &text[at_idx + 4..];

        // F12: this used to time-check ANY `kind_part` before asking
        // whether it was even "daily"/"weekday" - so "foobar at 99:99" hit
        // the hour>23 check and returned "That is not a time of day."
        // instead of falling to `parse_clock`, which is what decides
        // "foobar" isn't a day name and produces the real "not a schedule
        // I understand" message. TS's own `at` regex only matches those
        // two literal kinds at all (`schedule.ts:77`) - anything else never
        // reaches its hour/minute range check.
        let is_daily_or_weekday =
            matches!(kind_part, "daily" | "every day" | "weekdays" | "weekday");

        // time_part not matching a strict "H:MM"/"HH:MM" (F12: "7:5",
        // "007:30") falls through to `parse_clock` below, same as any
        // other `kind_part` - TS's regex simply doesn't match here either.
        if is_daily_or_weekday && let Some((hour, minute)) = parse_time(time_part) {
            if hour > 23 || minute > 59 {
                return Err("That is not a time of day.".to_string());
            }
            return Ok(if kind_part == "daily" || kind_part == "every day" {
                Schedule::Daily { hour, minute }
            } else {
                Schedule::Weekdays { hour, minute }
            });
        }

        match parse_clock(&text) {
            Ok(Some(sched)) => return Ok(sched),
            Ok(None) => {}
            // F12: a clock-shaped error (e.g. "`7:5` is not a time of
            // day.") must reach the caller as-is, not get swallowed into
            // the generic "not a schedule I understand" fallback below.
            Err(msg) => return Err(msg),
        }
    }

    Err(format!(
        "That is not a schedule I understand. Try: {}",
        EXAMPLES
    ))
}

/// Parses a single "H:MM" or "HH:MM" time strictly - TS's own regex is
/// `\d{1,2}:\d{2}` (`schedule.ts:77,107`): 1-2 digit hour, EXACTLY 2 digit
/// minute, digits only.
///
/// F12: this used to parse leniently via `str::parse`, so "7:5" (a 1-digit
/// minute) and "007:30" (a 3-digit hour) both parsed as valid times where
/// TS's anchored regex rejects both - accepting them here meant a typo like
/// "daily at 7:5" silently became "daily at 07:05" instead of falling
/// through to the clock grammar's own (correct) error.
fn parse_time(text: &str) -> Option<(u32, u32)> {
    let (h, m) = text.split_once(':')?;
    if h.is_empty() || h.len() > 2 || !h.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    if m.len() != 2 || !m.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    let hour = h.parse::<u32>().ok()?;
    let minute = m.parse::<u32>().ok()?;
    Some((hour, minute))
}

/// Parses "mondays at 09:00", "tuesdays and thursdays at 11:32", etc.
fn parse_clock(text: &str) -> Result<Option<Schedule>, String> {
    let at_idx = match text.rfind(" at ") {
        Some(idx) => idx,
        None => return Ok(None),
    };

    let day_part = text[..at_idx].trim();
    let time_part = text[at_idx + 4..].trim();

    // Check that time_part looks reasonable (digits, colons, commas, "and", spaces)
    let time_chars_ok = time_part.chars().all(|c| {
        c.is_ascii_digit() || c == ':' || c == ',' || c == ' ' || c == 'a' || c == 'n' || c == 'd'
    });
    if !time_chars_ok {
        return Ok(None);
    }

    let days = parse_days(day_part)?;
    let Some(days) = days else {
        return Ok(None);
    };

    // Parse times from time_part, handling "09:00, 13:00 and 17:00" format
    let mut times: Vec<ClockTime> = Vec::new();

    // Replace " and " with commas for easier splitting
    let normalized = time_part.replace(" and ", ",");

    for chunk in normalized.split(',') {
        let trimmed = chunk.trim();
        if trimmed.is_empty() {
            continue;
        }

        if let Some((hour, minute)) = parse_time(trimmed) {
            if hour > 23 || minute > 59 {
                return Err("That is not a time of day.".to_string());
            }
            times.push(ClockTime { hour, minute });
        } else {
            return Err(format!("`{}` is not a time of day.", trimmed));
        }
    }

    if times.is_empty() {
        return Ok(None);
    }

    times.sort_by_key(|t| (t.hour, t.minute));
    times.dedup();
    Ok(Some(Schedule::Clock { days, times }))
}

/// Parses day names like "monday", "weekdays", etc.
fn parse_days(text: &str) -> Result<Option<Vec<u32>>, String> {
    if text == "daily" || text == "every day" {
        return Ok(Some(vec![0, 1, 2, 3, 4, 5, 6]));
    }
    if text == "weekdays" || text == "weekday" {
        return Ok(Some(vec![1, 2, 3, 4, 5]));
    }
    if text == "weekends" || text == "weekend" {
        return Ok(Some(vec![0, 6]));
    }

    let mut found: Vec<u32> = Vec::new();

    for word in text.split(|c: char| c == ',' || c.is_whitespace()) {
        let word = word.trim();
        // F12: TS strips at most ONE trailing "s" (`text.replace(/s$/,
        // "")`, not a global replace) - `trim_end_matches('s')` stripped
        // EVERY trailing "s", so a typo like "mondayss" wrongly matched
        // "monday" instead of failing to parse.
        let w = word.strip_suffix('s').unwrap_or(word);
        if w.is_empty() || w == "and" {
            continue;
        }

        if let Some(idx) = DAY_NAMES.iter().position(|&d| d == w) {
            if !found.contains(&(idx as u32)) {
                found.push(idx as u32);
            }
        } else {
            return Ok(None);
        }
    }

    if found.is_empty() {
        return Ok(None);
    }

    found.sort();
    Ok(Some(found))
}

/// Describes a schedule in human-readable form.
pub fn describe_schedule(schedule: &Schedule) -> String {
    match schedule {
        Schedule::Interval {
            minutes,
            from_hour: _,
            to_hour: _,
        } => {
            if minutes % 60 == 0 {
                if *minutes == 60 {
                    "every hour".to_string()
                } else {
                    format!("every {} hours", minutes / 60)
                }
            } else {
                format!("every {} minutes", minutes)
            }
        }
        Schedule::Hourly { minute: _ } => "every hour".to_string(),
        Schedule::Daily { hour, minute } => {
            format!("daily at {:02}:{:02}", hour, minute)
        }
        Schedule::Weekdays { hour, minute } => {
            format!("weekdays at {:02}:{:02}", hour, minute)
        }
        Schedule::Clock { days, times } => {
            let days_str = list_days(days);
            let times_str = list_times(times);
            format!("{} at {}", days_str, times_str)
        }
    }
}

/// Lists days in human-readable form.
fn list_days(days: &[u32]) -> String {
    if days.len() == 7 {
        return "daily".to_string();
    }
    if days.len() == 5 && days.iter().all(|d| (1..=5).contains(d)) {
        return "weekdays".to_string();
    }
    if days.len() == 2 && days.contains(&0) && days.contains(&6) {
        return "weekends".to_string();
    }

    let names: Vec<String> = days
        .iter()
        .filter_map(|&d| DAY_NAMES.get(d as usize).map(|n| format!("{}s", n)))
        .collect();

    if names.is_empty() {
        "".to_string()
    } else if names.len() == 1 {
        names[0].clone()
    } else {
        let mut result = names[..names.len() - 1].join(", ");
        result.push_str(" and ");
        result.push_str(&names[names.len() - 1]);
        result
    }
}

/// Lists times in human-readable form.
fn list_times(times: &[ClockTime]) -> String {
    let parts: Vec<String> = times
        .iter()
        .map(|t| format!("{:02}:{:02}", t.hour, t.minute))
        .collect();

    if parts.is_empty() {
        "".to_string()
    } else if parts.len() == 1 {
        parts[0].clone()
    } else {
        let mut result = parts[..parts.len() - 1].join(", ");
        result.push_str(" and ");
        result.push_str(&parts[parts.len() - 1]);
        result
    }
}

/// Returns true if the hour is within the window (handles wrapping past midnight).
fn in_window(hour: u32, from_hour: u32, to_hour: u32) -> bool {
    if from_hour <= to_hour {
        hour >= from_hour && hour <= to_hour
    } else {
        hour >= from_hour || hour <= to_hour
    }
}

/// Calculates the next time this schedule should fire, strictly after `from`.
pub fn next_run(schedule: &Schedule, from: DateTime<Utc>) -> DateTime<Utc> {
    let from_local = from.with_timezone(&Local);

    let next_local = match schedule {
        Schedule::Interval {
            minutes,
            from_hour,
            to_hour,
        } => {
            let mut next = from_local + chrono::Duration::minutes(*minutes as i64);

            if let (Some(from_h), Some(to_h)) = (from_hour, to_hour) {
                // F12: this used to zero the minute and wrap the hour with
                // `% 24` on every step - throwing away the minute TS
                // preserves and never advancing the calendar date when the
                // hour wrapped past midnight. Adding a real hour instead
                // rolls the date automatically, same as TS's
                // `setHours(h + 1, m, 0, 0)`. Unreachable today (nothing
                // sets `from_hour`/`to_hour` yet) but wrong is wrong.
                next = next
                    .with_second(0)
                    .unwrap_or(next)
                    .with_nanosecond(0)
                    .unwrap_or(next);
                let mut guard = 0;
                while !in_window(next.hour(), *from_h, *to_h) && guard < 48 {
                    next += chrono::Duration::hours(1);
                    guard += 1;
                }
            }
            next
        }
        Schedule::Clock { days, times } => {
            for ahead in 0..=8 {
                let day = from_local + chrono::Duration::days(ahead as i64);
                let day_of_week = day.weekday().num_days_from_sunday();
                if !days.contains(&day_of_week) {
                    continue;
                }

                for time in times {
                    let mut test_time = day.with_hour(time.hour).unwrap_or(day);
                    test_time = test_time
                        .with_minute(time.minute)
                        .unwrap_or(test_time)
                        .with_second(0)
                        .unwrap_or(test_time)
                        .with_nanosecond(0)
                        .unwrap_or(test_time);

                    if test_time > from_local {
                        return test_time.with_timezone(&Utc);
                    }
                }
            }
            from_local + chrono::Duration::days(7)
        }
        Schedule::Hourly { minute } => {
            let mut next = from_local.with_minute(*minute).unwrap_or(from_local);
            next = next
                .with_second(0)
                .unwrap_or(next)
                .with_nanosecond(0)
                .unwrap_or(next);
            if next <= from_local {
                next += chrono::Duration::hours(1);
            }
            next
        }
        Schedule::Daily { hour, minute } => {
            let mut next = from_local.with_hour(*hour).unwrap_or(from_local);
            next = next
                .with_minute(*minute)
                .unwrap_or(next)
                .with_second(0)
                .unwrap_or(next)
                .with_nanosecond(0)
                .unwrap_or(next);
            if next <= from_local {
                next += chrono::Duration::days(1);
            }
            next
        }
        Schedule::Weekdays { hour, minute } => {
            let mut next = from_local.with_hour(*hour).unwrap_or(from_local);
            next = next
                .with_minute(*minute)
                .unwrap_or(next)
                .with_second(0)
                .unwrap_or(next)
                .with_nanosecond(0)
                .unwrap_or(next);
            if next <= from_local {
                next += chrono::Duration::days(1);
            }

            while next.weekday() == Weekday::Sun || next.weekday() == Weekday::Sat {
                next += chrono::Duration::days(1);
            }
            next
        }
    };

    next_local.with_timezone(&Utc)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_schedule_understands_four_shapes() {
        let result = parse_schedule("every 15 minutes");
        assert!(result.is_ok());
        match result.unwrap() {
            Schedule::Interval { minutes, .. } => assert_eq!(minutes, 15),
            _ => panic!("Expected interval"),
        }

        let result = parse_schedule("every 2 hours");
        assert!(result.is_ok());
        match result.unwrap() {
            Schedule::Interval { minutes, .. } => assert_eq!(minutes, 120),
            _ => panic!("Expected interval"),
        }

        let result = parse_schedule("hourly");
        assert!(result.is_ok());
        match result.unwrap() {
            Schedule::Hourly { .. } => {}
            _ => panic!("Expected hourly"),
        }

        let result = parse_schedule("daily at 07:30");
        assert!(result.is_ok());
        match result.unwrap() {
            Schedule::Daily { hour, minute } => {
                assert_eq!(hour, 7);
                assert_eq!(minute, 30);
            }
            _ => panic!("Expected daily"),
        }

        let result = parse_schedule("weekdays at 08:43");
        assert!(result.is_ok());
        match result.unwrap() {
            Schedule::Weekdays { hour, minute } => {
                assert_eq!(hour, 8);
                assert_eq!(minute, 43);
            }
            _ => panic!("Expected weekdays"),
        }
    }

    #[test]
    fn test_parse_schedule_rejects_unknown_format() {
        let result = parse_schedule("43 8 * * 1-5");
        assert!(result.is_err());
        let err = result.unwrap_err();
        assert!(err.contains("daily at 07:30"));
    }

    #[test]
    fn test_parse_schedule_enforces_interval_floor() {
        let result = parse_schedule("every 1 minutes");
        assert!(result.is_err());
        assert_eq!(result.unwrap_err(), "The shortest interval is 5 minutes.");

        let result = parse_schedule("every 5 minutes");
        assert!(result.is_ok());
    }

    #[test]
    fn test_describe_schedule_roundtrips() {
        for text in &[
            "every 15 minutes",
            "every 2 hours",
            "daily at 07:30",
            "weekdays at 08:43",
        ] {
            let parsed = parse_schedule(text).expect("parse");
            let described = describe_schedule(&parsed);
            assert_eq!(&described, text, "failed for: {}", text);
        }
    }

    #[test]
    fn test_next_run_is_always_in_future() {
        let from = DateTime::<Utc>::from_naive_utc_and_offset(
            DateTime::from_timestamp(1725955800, 0).unwrap().naive_utc(),
            Utc,
        );

        for text in &[
            "daily at 07:30",
            "hourly",
            "every 15 minutes",
            "weekdays at 08:43",
        ] {
            let s = parse_schedule(text).expect("parse");
            let next = next_run(&s, from);
            assert!(next > from, "next run not in future for: {}", text);
        }
    }

    #[test]
    fn test_weekdays_skip_weekend() {
        // Friday 2026-09-11 at 09:00 LOCAL, past that day's 08:43 - built in
        // Local so the weekday is the weekday the schedule sees. (The first
        // version hand-wrote a UTC epoch that was a Wednesday in 2024.)
        use chrono::TimeZone;
        let friday_utc = Local
            .with_ymd_and_hms(2026, 9, 11, 9, 0, 0)
            .single()
            .expect("unambiguous local time")
            .with_timezone(&Utc);
        assert_eq!(friday_utc.with_timezone(&Local).weekday(), Weekday::Fri);
        let s = parse_schedule("weekdays at 08:43").expect("parse");
        let next = next_run(&s, friday_utc);
        let next_local = next.with_timezone(&Local);
        assert_eq!(next_local.weekday(), Weekday::Mon, "should skip to Monday");
    }

    /// F3: `Schedule`'s wire shape must be byte-equal to TS's own JSON
    /// (`schedule.ts:16-35`) - field names, which fields exist per kind,
    /// and the "kind" tag - since `routines.schedule` now holds exactly
    /// this text and a live TS Bullpen's `JSON.parse` reads it back.
    #[test]
    fn test_schedule_json_shape_matches_ts_byte_for_byte() {
        let cases: &[(Schedule, &str)] = &[
            (
                Schedule::Interval {
                    minutes: 15,
                    from_hour: None,
                    to_hour: None,
                },
                r#"{"kind":"interval","minutes":15}"#,
            ),
            (
                Schedule::Hourly { minute: 0 },
                r#"{"kind":"hourly","minute":0}"#,
            ),
            (
                Schedule::Daily {
                    hour: 7,
                    minute: 30,
                },
                r#"{"kind":"daily","hour":7,"minute":30}"#,
            ),
            (
                Schedule::Weekdays {
                    hour: 8,
                    minute: 43,
                },
                r#"{"kind":"weekdays","hour":8,"minute":43}"#,
            ),
            (
                Schedule::Clock {
                    days: vec![1, 4],
                    times: vec![ClockTime { hour: 9, minute: 0 }],
                },
                r#"{"kind":"clock","days":[1,4],"times":[{"hour":9,"minute":0}]}"#,
            ),
        ];

        for (schedule, expected) in cases {
            let encoded = serde_json::to_string(schedule).expect("serialize");
            assert_eq!(&encoded, expected, "mismatch for {:?}", schedule);
        }
    }

    /// F3: a schedule written as TS-shaped JSON (a live TS Bullpen row, or
    /// this crate's own column after F3) parses straight through
    /// `parse_schedule` - the same function `fire_due`/
    /// `resume_absence_paused`/`POST /:id/active` already call.
    #[test]
    fn test_parse_schedule_accepts_ts_json_shape() {
        let parsed = parse_schedule(r#"{"kind":"interval","minutes":15}"#).expect("parse json");
        assert_eq!(
            parsed,
            Schedule::Interval {
                minutes: 15,
                from_hour: None,
                to_hour: None,
            }
        );

        let parsed =
            parse_schedule(r#"{"kind":"clock","days":[1,4],"times":[{"hour":9,"minute":0}]}"#)
                .expect("parse json clock");
        assert_eq!(
            parsed,
            Schedule::Clock {
                days: vec![1, 4],
                times: vec![ClockTime { hour: 9, minute: 0 }],
            }
        );
    }

    /// F12: "foobar" is not a day name at all, so this must fall all the
    /// way to the generic "not a schedule I understand" message - not the
    /// hour-range check, which only fires for a RECOGNISED "daily"/
    /// "weekday" kind (`schedule.ts`'s own `at` regex never matches
    /// "foobar" in the first place).
    #[test]
    fn test_unrecognised_kind_falls_to_the_generic_message_not_time_of_day() {
        let result = parse_schedule("foobar at 99:99");
        assert!(result.is_err());
        let err = result.unwrap_err();
        assert!(err.contains("not a schedule I understand"), "got: {}", err);
    }

    /// F12: "daily at 99:99" IS a recognised kind with well-formed digit
    /// widths, so it must still hit the explicit hour/minute range check.
    #[test]
    fn test_recognised_kind_with_out_of_range_time_is_not_a_time_of_day() {
        let result = parse_schedule("daily at 99:99");
        assert!(result.is_err());
        assert_eq!(result.unwrap_err(), "That is not a time of day.");
    }

    /// F12: `\d{1,2}:\d{2}` - a 1-digit minute or a 3-digit hour is not a
    /// valid time, and TS falls through to the clock grammar's own error
    /// for it rather than silently rounding.
    #[test]
    fn test_loose_time_digit_widths_are_rejected() {
        let result = parse_schedule("daily at 7:5");
        assert!(result.is_err());
        assert_eq!(result.unwrap_err(), "`7:5` is not a time of day.");

        let result = parse_schedule("daily at 007:30");
        assert!(result.is_err());
        assert_eq!(result.unwrap_err(), "`007:30` is not a time of day.");
    }

    /// F12: TS strips at most ONE trailing "s" from a day word
    /// (`text.replace(/s$/, "")`) - "mondayss" must NOT match "monday".
    #[test]
    fn test_double_trailing_s_on_a_day_name_does_not_parse() {
        let result = parse_schedule("mondayss at 09:00");
        assert!(result.is_err());
    }

    /// F12: every computed `next_run_at` should carry zero sub-second
    /// nanoseconds, matching TS's `setHours`/`setMinutes` (which always
    /// zero ms) - a stray nanosecond would make two "simultaneous" runs
    /// compare unequal for no reason a person typed.
    #[test]
    fn test_next_run_has_no_sub_second_nanoseconds() {
        let from = DateTime::<Utc>::from_naive_utc_and_offset(
            DateTime::from_timestamp(1725955800, 123_456_789)
                .unwrap()
                .naive_utc(),
            Utc,
        );

        for text in &["daily at 07:30", "hourly", "weekdays at 08:43"] {
            let s = parse_schedule(text).expect("parse");
            let next = next_run(&s, from);
            assert_eq!(
                next.timestamp_subsec_nanos(),
                0,
                "next_run_at should have zero nanoseconds for: {}",
                text
            );
        }

        let clock = parse_schedule("monday at 09:00").expect("parse clock");
        let next = next_run(&clock, from);
        assert_eq!(next.timestamp_subsec_nanos(), 0, "clock next_run_at");
    }
}
