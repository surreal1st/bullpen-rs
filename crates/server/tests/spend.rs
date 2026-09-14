//! S2-05: spend month-to-date and ceiling gate tests.

mod common;

use server::spend;

#[test]
fn spend_query_works_on_empty_db() {
    let db = store::Db::open(":memory:").expect("open test db");

    // When there are no messages, query should return empty
    let now = chrono::Utc::now();
    let month = spend::current_month(now);
    let bot_spends = spend::spend_by_bot(&db, &month).expect("query spend by bot");

    assert_eq!(bot_spends.len(), 0);
}

#[test]
fn put_ceiling_sets_and_returns_the_value() {
    let db = store::Db::open(":memory:").expect("open test db");

    // Default ceiling
    let default = spend::get_ceiling(&db);
    assert_eq!(default, 10.0);

    // Set new ceiling
    let new_val = spend::set_ceiling(&db, 25.5).expect("set ceiling");
    assert_eq!(new_val, 25.5);

    // Verify it persists
    let stored = spend::get_ceiling(&db);
    assert_eq!(stored, 25.5);
}

#[test]
fn put_ceiling_cleans_negative_to_zero() {
    let db = store::Db::open(":memory:").expect("open test db");

    let result = spend::set_ceiling(&db, -5.0).expect("set ceiling");
    assert_eq!(result, 0.0);
    assert_eq!(spend::get_ceiling(&db), 0.0);
}

#[test]
fn gate_run_denies_when_over_ceiling() {
    let ceiling = 10.0;
    let db = store::Db::open(":memory:").expect("open test db");
    let result = spend::gate_run(&db, ceiling, Some(11.0));

    match result {
        spend::GateResult::Denied { reason } => {
            assert!(reason.contains("Spend ceiling reached"));
            assert!(reason.contains("$11.00"));
            assert!(reason.contains("$10.00"));
        }
        _ => panic!("expected Denied, got Allowed"),
    }
}

#[test]
fn gate_run_allows_when_under_ceiling() {
    let ceiling = 10.0;
    let db = store::Db::open(":memory:").expect("open test db");
    let result = spend::gate_run(&db, ceiling, Some(5.0));

    match result {
        spend::GateResult::Allowed { warning } => {
            assert!(warning.is_none());
        }
        _ => panic!("expected Allowed with no warning, got {:?}", result),
    }
}

#[test]
fn gate_run_warns_at_15_percent_headroom() {
    let ceiling = 100.0;
    let db = store::Db::open(":memory:").expect("open test db");
    // At 85% used: 15% headroom, should warn
    let result = spend::gate_run(&db, ceiling, Some(85.0));

    match result {
        spend::GateResult::Allowed { warning } => {
            assert!(warning.is_some());
            let w = warning.unwrap();
            assert!(w.contains("$15.00"));
            assert!(w.contains("$100.00"));
            assert!(w.contains("ceiling stops"));
        }
        _ => panic!("expected Allowed with warning, got {:?}", result),
    }
}

#[test]
fn gate_run_allows_on_network_failure() {
    let ceiling = 10.0;
    let db = store::Db::open(":memory:").expect("open test db");
    // None = network read failed
    let result = spend::gate_run(&db, ceiling, None);

    match result {
        spend::GateResult::Allowed { warning } => {
            assert!(warning.is_some());
            let w = warning.unwrap();
            assert!(w.contains("Could not read"));
        }
        _ => panic!("expected Allowed with warning, got {:?}", result),
    }
}

#[test]
fn current_month_formats_correctly() {
    let dt = chrono::DateTime::parse_from_rfc3339("2026-09-14T12:00:00Z")
        .unwrap()
        .with_timezone(&chrono::Utc);

    let month = spend::current_month(dt);
    assert_eq!(month, "2026-09");
}

#[test]
fn ceiling_gate_denies_posts_when_at_ceiling() {
    let db = store::Db::open(":memory:").expect("open test db");

    // Set ceiling to 0 (no spend allowed)
    spend::set_ceiling(&db, 0.0).expect("set ceiling to 0");

    // Verify that gate_run denies when ceiling is 0 and account_usage is 0
    let result = spend::gate_run(&db, 0.0, Some(0.0));
    match result {
        spend::GateResult::Denied { reason } => {
            assert!(reason.contains("Spend ceiling reached"));
        }
        _ => panic!("expected Denied when at zero ceiling"),
    }
}
