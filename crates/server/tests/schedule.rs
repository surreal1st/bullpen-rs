use chrono::{Datelike, Local, TimeZone, Timelike, Utc, Weekday};
use server::schedule::{ClockTime, Schedule, describe_schedule, next_run, parse_schedule};

#[test]
fn test_understands_the_four_shapes() {
    let result = parse_schedule("every 15 minutes").expect("parse");
    match result {
        Schedule::Interval { minutes, .. } => assert_eq!(minutes, 15),
        _ => panic!("Expected interval"),
    }

    let result = parse_schedule("every 2 hours").expect("parse");
    match result {
        Schedule::Interval { minutes, .. } => assert_eq!(minutes, 120),
        _ => panic!("Expected interval"),
    }

    let result = parse_schedule("hourly").expect("parse");
    match result {
        Schedule::Hourly { .. } => {}
        _ => panic!("Expected hourly"),
    }

    let result = parse_schedule("daily at 07:30").expect("parse");
    match result {
        Schedule::Daily { hour, minute } => {
            assert_eq!(hour, 7);
            assert_eq!(minute, 30);
        }
        _ => panic!("Expected daily"),
    }

    let result = parse_schedule("weekdays at 08:43").expect("parse");
    match result {
        Schedule::Weekdays { hour, minute } => {
            assert_eq!(hour, 8);
            assert_eq!(minute, 43);
        }
        _ => panic!("Expected weekdays"),
    }
}

#[test]
fn test_says_what_it_does_not_understand_instead_of_guessing() {
    let result = parse_schedule("43 8 * * 1-5");
    assert!(result.is_err());
    let error = result.unwrap_err();
    assert!(error.contains("daily at 07:30"));
}

#[test]
fn test_refuses_an_interval_short_enough_to_be_a_bill_with_no_end() {
    let result = parse_schedule("every 1 minutes");
    assert!(result.is_err());
    assert_eq!(result.unwrap_err(), "The shortest interval is 5 minutes.");

    let result = parse_schedule("every 5 minutes");
    assert!(result.is_ok());
}

#[test]
fn test_reads_back_in_the_same_words() {
    let test_cases = vec![
        "every 15 minutes",
        "every 2 hours",
        "daily at 07:30",
        "weekdays at 08:43",
    ];

    for text in test_cases {
        let parsed = parse_schedule(text).expect("parse");
        let described = describe_schedule(&parsed);
        assert_eq!(
            described, text,
            "describe failed to roundtrip for: {}",
            text
        );
    }
}

#[test]
fn test_never_returns_a_next_run_that_is_not_in_the_future() {
    let from = Local
        .with_ymd_and_hms(2026, 9, 10, 7, 30, 0)
        .unwrap()
        .with_timezone(&Utc);

    let test_cases = vec![
        "daily at 07:30",
        "hourly",
        "every 15 minutes",
        "weekdays at 08:43",
    ];

    for text in test_cases {
        let s = parse_schedule(text).expect("parse");
        let next = next_run(&s, from);
        assert!(
            next > from,
            "next run not in future for: {} (from: {:?}, next: {:?})",
            text,
            from,
            next
        );
    }
}

#[test]
fn test_skips_the_weekend_for_a_weekdays_schedule() {
    let friday = Local
        .with_ymd_and_hms(2026, 9, 11, 9, 0, 0)
        .unwrap()
        .with_timezone(&Utc);

    let s = parse_schedule("weekdays at 08:43").expect("parse");
    let next = next_run(&s, friday);
    let next_local = next.with_timezone(&Local);

    assert_eq!(
        next_local.weekday(),
        Weekday::Mon,
        "should skip to Monday after Friday"
    );
}

#[test]
fn test_parse_empty_string() {
    let result = parse_schedule("");
    assert!(result.is_err());
    let error = result.unwrap_err();
    assert!(error.contains("Give a schedule"));
}

#[test]
fn test_parse_whitespace_only() {
    let result = parse_schedule("   ");
    assert!(result.is_err());
    let error = result.unwrap_err();
    assert!(error.contains("Give a schedule"));
}

#[test]
fn test_parse_every_hour_variant() {
    let result = parse_schedule("every hour").expect("parse");
    match result {
        Schedule::Hourly { minute } => {
            assert_eq!(minute, 0);
        }
        _ => panic!("Expected hourly"),
    }
}

#[test]
fn test_parse_every_day_variant() {
    let result = parse_schedule("daily at 15:45").expect("parse");
    match result {
        Schedule::Daily { hour, minute } => {
            assert_eq!(hour, 15);
            assert_eq!(minute, 45);
        }
        _ => panic!("Expected daily"),
    }

    let result = parse_schedule("every day at 15:45").expect("parse");
    match result {
        Schedule::Daily { hour, minute } => {
            assert_eq!(hour, 15);
            assert_eq!(minute, 45);
        }
        _ => panic!("Expected daily"),
    }
}

#[test]
fn test_parse_weekday_variant() {
    let result = parse_schedule("weekday at 09:30").expect("parse");
    match result {
        Schedule::Weekdays { hour, minute } => {
            assert_eq!(hour, 9);
            assert_eq!(minute, 30);
        }
        _ => panic!("Expected weekdays"),
    }
}

#[test]
fn test_parse_invalid_time() {
    let result = parse_schedule("daily at 25:00");
    assert!(result.is_err());
    assert_eq!(result.unwrap_err(), "That is not a time of day.");

    let result = parse_schedule("daily at 12:60");
    assert!(result.is_err());
    assert_eq!(result.unwrap_err(), "That is not a time of day.");
}

#[test]
fn test_parse_interval_too_long() {
    let result = parse_schedule("every 10080 minutes");
    assert!(result.is_ok());

    let result = parse_schedule("every 10081 minutes");
    assert!(result.is_err());
    assert_eq!(result.unwrap_err(), "That is longer than a week.");
}

#[test]
fn test_parse_interval_in_hours() {
    let result = parse_schedule("every 3 hours").expect("parse");
    match result {
        Schedule::Interval { minutes, .. } => {
            assert_eq!(minutes, 180);
        }
        _ => panic!("Expected interval"),
    }
}

#[test]
fn test_parse_interval_min_variant() {
    let result = parse_schedule("every 30 min").expect("parse");
    match result {
        Schedule::Interval { minutes, .. } => {
            assert_eq!(minutes, 30);
        }
        _ => panic!("Expected interval"),
    }
}

#[test]
fn test_parse_case_insensitive() {
    let result1 = parse_schedule("Every 15 Minutes").expect("parse");
    let result2 = parse_schedule("every 15 minutes").expect("parse");
    assert_eq!(result1, result2);
}

#[test]
fn test_parse_extra_whitespace() {
    let result1 = parse_schedule("every   15   minutes").expect("parse");
    let result2 = parse_schedule("every 15 minutes").expect("parse");
    assert_eq!(result1, result2);
}

#[test]
fn test_describe_interval_minute() {
    let schedule = Schedule::Interval {
        minutes: 45,
        from_hour: None,
        to_hour: None,
    };
    assert_eq!(describe_schedule(&schedule), "every 45 minutes");
}

#[test]
fn test_describe_interval_hours() {
    let schedule = Schedule::Interval {
        minutes: 120,
        from_hour: None,
        to_hour: None,
    };
    assert_eq!(describe_schedule(&schedule), "every 2 hours");

    let schedule = Schedule::Interval {
        minutes: 60,
        from_hour: None,
        to_hour: None,
    };
    assert_eq!(describe_schedule(&schedule), "every hour");
}

#[test]
fn test_describe_hourly() {
    let schedule = Schedule::Hourly { minute: 0 };
    assert_eq!(describe_schedule(&schedule), "every hour");
}

#[test]
fn test_describe_daily() {
    let schedule = Schedule::Daily {
        hour: 7,
        minute: 30,
    };
    assert_eq!(describe_schedule(&schedule), "daily at 07:30");
}

#[test]
fn test_describe_weekdays() {
    let schedule = Schedule::Weekdays {
        hour: 8,
        minute: 43,
    };
    assert_eq!(describe_schedule(&schedule), "weekdays at 08:43");
}

#[test]
fn test_next_run_interval_basic() {
    let from = Local
        .with_ymd_and_hms(2026, 9, 10, 10, 0, 0)
        .unwrap()
        .with_timezone(&Utc);

    let schedule = Schedule::Interval {
        minutes: 15,
        from_hour: None,
        to_hour: None,
    };

    let next = next_run(&schedule, from);
    assert!(next > from);

    let duration = next - from;
    let minutes = duration.num_minutes();
    assert_eq!(minutes, 15, "interval should be 15 minutes");
}

#[test]
fn test_next_run_daily_in_future() {
    let from = Local
        .with_ymd_and_hms(2026, 9, 10, 8, 0, 0)
        .unwrap()
        .with_timezone(&Utc);

    let schedule = Schedule::Daily {
        hour: 14,
        minute: 30,
    };
    let next = next_run(&schedule, from);

    assert!(next > from, "next run should be in the future");
    let next_local = next.with_timezone(&Local);
    assert_eq!(next_local.hour(), 14);
    assert_eq!(next_local.minute(), 30);
}

#[test]
fn test_next_run_daily_in_past() {
    let from = Local
        .with_ymd_and_hms(2026, 9, 10, 20, 0, 0)
        .unwrap()
        .with_timezone(&Utc);

    let schedule = Schedule::Daily { hour: 7, minute: 0 };
    let next = next_run(&schedule, from);

    assert!(next > from, "next run should be in the future");
    let duration = next - from;
    assert!(
        duration.num_hours() > 8,
        "should be more than 8 hours away (next day, from 20:00 to 07:00)"
    );
}

#[test]
fn test_next_run_hourly() {
    let from = Local
        .with_ymd_and_hms(2026, 9, 10, 10, 15, 0)
        .unwrap()
        .with_timezone(&Utc);

    let schedule = Schedule::Hourly { minute: 0 };
    let next = next_run(&schedule, from);

    assert!(next > from);
    let duration = next - from;
    assert!(
        duration.num_minutes() <= 60,
        "hourly should fire within an hour"
    );
}

#[test]
fn test_next_run_weekdays_on_monday() {
    let monday = Local
        .with_ymd_and_hms(2026, 9, 7, 9, 0, 0)
        .unwrap()
        .with_timezone(&Utc);

    let schedule = Schedule::Weekdays { hour: 9, minute: 0 };
    let next = next_run(&schedule, monday);

    let next_local = next.with_timezone(&Local);
    assert_ne!(next_local.weekday(), Weekday::Sat, "should not be Saturday");
    assert_ne!(next_local.weekday(), Weekday::Sun, "should not be Sunday");
}

#[test]
fn test_next_run_weekdays_on_saturday() {
    let saturday = Local
        .with_ymd_and_hms(2026, 9, 12, 9, 0, 0)
        .unwrap()
        .with_timezone(&Utc);

    let schedule = Schedule::Weekdays { hour: 9, minute: 0 };
    let next = next_run(&schedule, saturday);

    let next_local = next.with_timezone(&Local);
    assert_eq!(
        next_local.weekday(),
        Weekday::Mon,
        "should skip to Monday from Saturday"
    );
}

#[test]
fn test_next_run_weekdays_on_sunday() {
    let sunday = Local
        .with_ymd_and_hms(2026, 9, 13, 9, 0, 0)
        .unwrap()
        .with_timezone(&Utc);

    let schedule = Schedule::Weekdays { hour: 9, minute: 0 };
    let next = next_run(&schedule, sunday);

    let next_local = next.with_timezone(&Local);
    assert_eq!(
        next_local.weekday(),
        Weekday::Mon,
        "should skip to Monday from Sunday"
    );
}

#[test]
fn test_clock_single_day_single_time() {
    let result = parse_schedule("monday at 09:00").expect("parse");
    match result {
        Schedule::Clock { days, times } => {
            assert_eq!(days, vec![1]);
            assert_eq!(times, vec![ClockTime { hour: 9, minute: 0 }]);
        }
        _ => panic!("Expected clock"),
    }
}

#[test]
fn test_clock_multiple_days() {
    let result = parse_schedule("monday and thursday at 10:30").expect("parse");
    match result {
        Schedule::Clock { days, times } => {
            assert_eq!(days, vec![1, 4]);
            assert_eq!(
                times,
                vec![ClockTime {
                    hour: 10,
                    minute: 30
                }]
            );
        }
        _ => panic!("Expected clock - got: {:?}", result),
    }
}

#[test]
fn test_clock_multiple_times() {
    let result = parse_schedule("weekdays at 09:00, 13:00 and 17:00").expect("parse");
    match result {
        Schedule::Clock { days, times } => {
            assert_eq!(days, vec![1, 2, 3, 4, 5]);
            assert_eq!(
                times,
                vec![
                    ClockTime { hour: 9, minute: 0 },
                    ClockTime {
                        hour: 13,
                        minute: 0
                    },
                    ClockTime {
                        hour: 17,
                        minute: 0
                    },
                ]
            );
        }
        _ => panic!("Expected clock - got: {:?}", result),
    }
}

#[test]
fn test_clock_weekends() {
    let result = parse_schedule("weekends at 10:00").expect("parse");
    match result {
        Schedule::Clock { days, times } => {
            assert_eq!(days, vec![0, 6]);
            assert_eq!(
                times,
                vec![ClockTime {
                    hour: 10,
                    minute: 0
                }]
            );
        }
        _ => panic!("Expected clock - got: {:?}", result),
    }
}

#[test]
fn test_clock_daily() {
    let result = parse_schedule("daily at 12:00").expect("parse");
    match result {
        Schedule::Daily { hour, minute } => {
            assert_eq!(hour, 12);
            assert_eq!(minute, 0);
        }
        _ => panic!("Expected daily"),
    }
}

#[test]
fn test_clock_with_commas() {
    let result = parse_schedule("tuesday, thursday at 11:00").expect("parse");
    match result {
        Schedule::Clock { days, times } => {
            assert!(days.contains(&2));
            assert!(days.contains(&4));
            assert_eq!(
                times,
                vec![ClockTime {
                    hour: 11,
                    minute: 0
                }]
            );
        }
        _ => panic!("Expected clock"),
    }
}
