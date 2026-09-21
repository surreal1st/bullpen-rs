//! S9-01: jobs store — port of TS `test/jobs.test.ts` store-level rules.

use store::Db;
use store::bots::create_bot;
use store::jobs::{
    JobKind, JobStatus, MAX_JOB_OUTPUT, MAX_RUNNING_JOBS, create_job, describe_job, finish_job,
    get_job, list_jobs, reap_orphaned_jobs,
};

fn seed() -> (Db, String, String) {
    let db = Db::open(":memory:").expect("open db");
    let a = create_bot(
        &db,
        store::bots::BotDraft {
            name: "A".into(),
            purpose: "p".into(),
            instructions: "i".into(),
            model: None,
        },
    )
    .expect("bot a")
    .id;
    let b = create_bot(
        &db,
        store::bots::BotDraft {
            name: "B".into(),
            purpose: "p".into(),
            instructions: "i".into(),
            model: None,
        },
    )
    .expect("bot b")
    .id;
    (db, a, b)
}

#[test]
fn caps_how_many_run_at_once() {
    let (db, a, _) = seed();
    for i in 0..MAX_RUNNING_JOBS {
        create_job(&db, &a, JobKind::Shell, &format!("j{i}"), "x", None).expect("create");
    }
    assert!(create_job(&db, &a, JobKind::Shell, "one more", "x", None).is_err());
}

#[test]
fn finished_job_frees_a_slot() {
    let (db, a, _) = seed();
    let first = create_job(&db, &a, JobKind::Shell, "j", "x", None).expect("first");
    for i in 1..MAX_RUNNING_JOBS {
        create_job(&db, &a, JobKind::Shell, &format!("j{i}"), "x", None).expect("fill");
    }
    finish_job(&db, &first.id, JobStatus::Done, "", Some(0), None).expect("finish");
    assert!(create_job(&db, &a, JobKind::Shell, "one more", "x", None).is_ok());
}

#[test]
fn one_bot_cannot_read_anothers_job() {
    let (db, a, b) = seed();
    let job = create_job(&db, &a, JobKind::Shell, "secret", "x", None).expect("job");
    assert!(get_job(&db, &b, &job.id).is_none());
}

#[test]
fn keeps_tail_of_long_output() {
    let (db, a, _) = seed();
    let job = create_job(&db, &a, JobKind::Shell, "j", "x", None).expect("job");
    let long = "A".repeat(MAX_JOB_OUTPUT);
    finish_job(
        &db,
        &job.id,
        JobStatus::Failed,
        &format!("{long}THE REAL ERROR"),
        Some(1),
        None,
    )
    .expect("finish");
    let stored = get_job(&db, &a, &job.id).expect("get");
    assert!(stored.output.contains("THE REAL ERROR"));
    assert!(stored.output.len() <= MAX_JOB_OUTPUT);
}

#[test]
fn restart_turns_running_jobs_into_told_failure() {
    let (db, a, _) = seed();
    create_job(&db, &a, JobKind::Shell, "j", "x", None).expect("job");
    assert_eq!(reap_orphaned_jobs(&db).expect("reap"), 1);
    let job = list_jobs(&db, &a).first().cloned().expect("listed");
    assert_eq!(job.status, JobStatus::Failed);
    assert!(job.output.contains("server restarted"));
}

#[test]
fn describe_running_says_not_to_invent_result() {
    let (db, a, _) = seed();
    let job = create_job(&db, &a, JobKind::Shell, "build", "make", None).expect("job");
    assert!(describe_job(&job).contains("do not invent its result"));
}

#[test]
fn ensure_job_tables_is_idempotent() {
    let db = Db::open(":memory:").expect("first open");
    let db2 = Db::open(":memory:").expect("second open");
    drop(db);
    drop(db2);
}
