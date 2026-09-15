//! Integration tests for `server::workers` - the worker registry CRUD, the
//! `bots.worker` routing decision, and `WorkerSandbox`'s lease/claim/
//! timeout/reclaim lifecycle. The concurrency tests below drive REAL
//! concurrent tokio tasks (multi-thread runtime, `tokio::spawn` + a
//! `Barrier` to force simultaneous arrival at the critical section) - a
//! sequential pair of `.await`ed calls would pass against a completely
//! broken guard and prove nothing (S6's own F18 lesson).
//!
//! Every fake below RECORDS its calls; assertions check the call list, not
//! just a return value, for the same reason.

use async_trait::async_trait;
use server::workers::{
    Clock, DockerResult, DockerRun, JobEvents, WORKER_WAKE_TIMEOUT_MS, WorkerSandbox,
};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

type Responder = Box<dyn Fn(usize, &[String]) -> DockerResult + Send + Sync>;

/// Records every `docker` invocation; each call's outcome is chosen by a
/// caller-supplied closure so a test can script "always asleep", "wake on
/// the Nth ping", etc. without a real daemon.
struct FakeDocker {
    calls: Mutex<Vec<Vec<String>>>,
    responder: Responder,
}

impl FakeDocker {
    fn new(responder: impl Fn(usize, &[String]) -> DockerResult + Send + Sync + 'static) -> Self {
        Self {
            calls: Mutex::new(Vec::new()),
            responder: Box::new(responder),
        }
    }

    fn always_asleep() -> Self {
        Self::new(|_, _| DockerResult {
            ok: false,
            stdout: String::new(),
            stderr: "connection refused".to_string(),
        })
    }

    fn calls(&self) -> Vec<Vec<String>> {
        self.calls.lock().unwrap().clone()
    }

    fn call_count(&self) -> usize {
        self.calls.lock().unwrap().len()
    }
}

#[async_trait]
impl DockerRun for FakeDocker {
    async fn run(&self, args: Vec<String>, _timeout_ms: u64) -> DockerResult {
        let n = {
            let mut calls = self.calls.lock().unwrap();
            calls.push(args.clone());
            calls.len() - 1
        };
        (self.responder)(n, &args)
    }
}

fn ok_version() -> DockerResult {
    DockerResult {
        ok: true,
        stdout: r#"{"Server":{"Os":"linux","Arch":"amd64","Version":"27.0.0"}}"#.to_string(),
        stderr: String::new(),
    }
}

/// Records every `append_output`/`finish_failed` call it receives - the same
/// discipline `FakeDocker` uses, so a test can assert exactly how many times
/// (and with what) the job pipeline was told about a reclaim, not just that
/// `spawn` returned something plausible.
#[derive(Default)]
struct FakeJobEvents {
    output: Mutex<Vec<(String, String)>>,
    finished_failed: Mutex<Vec<(String, String)>>,
}

impl FakeJobEvents {
    fn finished_failed(&self) -> Vec<(String, String)> {
        self.finished_failed.lock().unwrap().clone()
    }
}

impl JobEvents for FakeJobEvents {
    fn append_output(&self, job_id: &str, text: &str) {
        self.output
            .lock()
            .unwrap()
            .push((job_id.to_string(), text.to_string()));
    }
    fn finish_failed(&self, job_id: &str, message: &str) {
        self.finished_failed
            .lock()
            .unwrap()
            .push((job_id.to_string(), message.to_string()));
    }
}

/// A clock that never actually sleeps (so a 12-hour wait loop runs in
/// microseconds) and whose `now_ms()` a test can drive by hand via
/// `advance`, so the timeout-expiry reclaim path is reachable without a
/// real wait.
struct FakeClock {
    now: AtomicU64,
}

impl FakeClock {
    fn new() -> Arc<Self> {
        Arc::new(Self {
            now: AtomicU64::new(0),
        })
    }

    fn advance(&self, ms: u64) {
        self.now.fetch_add(ms, Ordering::SeqCst);
    }
}

#[async_trait]
impl Clock for FakeClock {
    fn now_ms(&self) -> u64 {
        self.now.load(Ordering::SeqCst)
    }

    async fn sleep(&self, ms: u64) {
        // Never actually waits - just advances the clock, so a loop that
        // sleeps N times before its deadline finishes instantly.
        self.advance(ms);
        tokio::task::yield_now().await;
    }
}

fn sandbox(docker: Arc<dyn DockerRun>, jobs: Arc<dyn JobEvents>) -> WorkerSandbox {
    WorkerSandbox::new("w1", "Workstation", docker, jobs)
}

/* ------------------------------------------------------- ordinary lifecycle */

#[tokio::test]
async fn check_reports_online_when_docker_answers() {
    let docker: Arc<dyn DockerRun> = Arc::new(FakeDocker::new(|_, _| ok_version()));
    let jobs: Arc<dyn JobEvents> = Arc::new(FakeJobEvents::default());
    let sb = sandbox(docker, jobs);
    let (ok, detail) = sb.check().await;
    assert!(ok);
    assert!(detail.contains("online"));
}

#[tokio::test]
async fn check_reports_asleep_when_docker_is_unreachable() {
    let docker: Arc<dyn DockerRun> = Arc::new(FakeDocker::always_asleep());
    let jobs: Arc<dyn JobEvents> = Arc::new(FakeJobEvents::default());
    let sb = sandbox(docker, jobs);
    let (ok, detail) = sb.check().await;
    assert!(!ok);
    assert!(detail.contains("asleep"));
}

#[tokio::test]
async fn exec_refuses_a_one_shot_command_when_asleep_without_touching_run() {
    let docker = Arc::new(FakeDocker::always_asleep());
    let jobs: Arc<dyn JobEvents> = Arc::new(FakeJobEvents::default());
    let sb = sandbox(docker.clone(), jobs);
    let result = sb.exec("bot1", "echo hi", None).await;
    assert_eq!(result.exit_code, 127);
    assert!(result.stderr.contains("run_in_background"));
    // Only the ping happened - never a `docker run`.
    assert_eq!(docker.call_count(), 1);
    assert_eq!(docker.calls()[0][0], "version");
}

#[tokio::test]
async fn exec_runs_the_command_when_online() {
    let docker = Arc::new(FakeDocker::new(|n, _| {
        if n == 0 {
            ok_version()
        } else {
            DockerResult {
                ok: true,
                stdout: "hi\n".to_string(),
                stderr: String::new(),
            }
        }
    }));
    let jobs: Arc<dyn JobEvents> = Arc::new(FakeJobEvents::default());
    let sb = sandbox(docker.clone(), jobs);
    let result = sb.exec("bot1", "echo hi", None).await;
    assert_eq!(result.exit_code, 0);
    assert_eq!(result.stdout, "hi\n");

    // Guard-present world: `exec_args` (workers.rs:1061-1093) always builds
    // the full sandbox flag set - `--network none` above everything else,
    // since a worker-routed sandbox has "no network, full stop" as its
    // entire isolation story (workers.rs:983-987). Guard-removed world:
    // delete any one of these flags from `exec_args` and every previous
    // assertion in this test (exit code, stdout) still passes, because none
    // of them look past `args[0]`. The observable that differs is the
    // recorded argv itself.
    let calls = docker.calls();
    assert_eq!(calls.len(), 2, "expected one ping + one run: {calls:?}");
    let argv = &calls[1];
    assert_eq!(argv[0], "run");
    assert!(
        argv.windows(2)
            .any(|w| w[0] == "--network" && w[1] == "none"),
        "missing --network none in {argv:?}"
    );
    assert!(
        argv.iter().any(|a| a == "--read-only"),
        "missing --read-only in {argv:?}"
    );
    assert!(
        argv.windows(2)
            .any(|w| w[0] == "--cap-drop" && w[1] == "ALL"),
        "missing --cap-drop ALL in {argv:?}"
    );
    assert!(
        argv.windows(2)
            .any(|w| w[0] == "--security-opt" && w[1] == "no-new-privileges"),
        "missing --security-opt no-new-privileges in {argv:?}"
    );
    assert!(
        argv.iter().any(|a| a == "--pids-limit"),
        "missing --pids-limit in {argv:?}"
    );
}

#[tokio::test]
async fn spawn_starts_immediately_when_online() {
    let docker = Arc::new(FakeDocker::new(|n, _| {
        if n == 0 {
            ok_version()
        } else {
            DockerResult {
                ok: true,
                stdout: String::new(),
                stderr: String::new(),
            }
        }
    }));
    let jobs: Arc<dyn JobEvents> = Arc::new(FakeJobEvents::default());
    let sb = sandbox(docker.clone(), jobs);
    let result = sb.spawn("bot1", "job-1", "sleep 1").await;
    assert!(result.ok);
    assert_eq!(result.detail, "started");
    assert_eq!(result.handle, "bullpen-job-job-1");

    // Same bite as `exec_runs_the_command_when_online` above, for the
    // `spawn` path's own argv builder (`spawn_args`, workers.rs:1095-1134),
    // which additionally must pin the container to `--name
    // bullpen-job-job-1` so `probe`/`kill` can find it again.
    let calls = docker.calls();
    assert_eq!(calls.len(), 2, "expected one ping + one run: {calls:?}");
    let argv = &calls[1];
    assert_eq!(argv[0], "run");
    assert!(
        argv.windows(2)
            .any(|w| w[0] == "--name" && w[1] == "bullpen-job-job-1"),
        "missing --name bullpen-job-job-1 in {argv:?}"
    );
    assert!(
        argv.windows(2)
            .any(|w| w[0] == "--network" && w[1] == "none"),
        "missing --network none in {argv:?}"
    );
    assert!(
        argv.iter().any(|a| a == "--read-only"),
        "missing --read-only in {argv:?}"
    );
    assert!(
        argv.windows(2)
            .any(|w| w[0] == "--cap-drop" && w[1] == "ALL"),
        "missing --cap-drop ALL in {argv:?}"
    );
    assert!(
        argv.windows(2)
            .any(|w| w[0] == "--security-opt" && w[1] == "no-new-privileges"),
        "missing --security-opt no-new-privileges in {argv:?}"
    );
}

#[tokio::test]
async fn spawn_queues_when_asleep_and_reports_progress_to_jobs() {
    let docker = Arc::new(FakeDocker::always_asleep());
    let jobs = Arc::new(FakeJobEvents::default());
    let sb = sandbox(docker, jobs.clone());
    let result = sb.spawn("bot1", "job-1", "sleep 1").await;
    assert!(result.ok);
    assert!(result.detail.contains("queued"));
    assert_eq!(jobs.output.lock().unwrap().len(), 1);
    assert!(jobs.output.lock().unwrap()[0].1.contains("Waiting for"));
}

#[tokio::test]
async fn probe_reports_running_while_queued_without_touching_docker_inspect() {
    let docker = Arc::new(FakeDocker::always_asleep());
    let jobs: Arc<dyn JobEvents> = Arc::new(FakeJobEvents::default());
    let sb = sandbox(docker.clone(), jobs);
    let spawned = sb.spawn("bot1", "job-1", "sleep 1").await;
    let calls_after_spawn = docker.call_count();

    let probe = sb.probe(&spawned.handle).await;
    assert!(probe.running);
    assert!(probe.detail.contains("queued"));
    // No `docker inspect` call - the queued state answered without it.
    assert_eq!(docker.call_count(), calls_after_spawn);
}

#[tokio::test]
async fn kill_cancels_a_queued_job_without_touching_docker_rm() {
    let docker = Arc::new(FakeDocker::always_asleep());
    let jobs: Arc<dyn JobEvents> = Arc::new(FakeJobEvents::default());
    let sb = sandbox(docker.clone(), jobs);
    let spawned = sb.spawn("bot1", "job-1", "sleep 1").await;
    let calls_after_spawn = docker.call_count();

    let killed = sb.kill(&spawned.handle).await;
    assert!(killed);
    assert_eq!(
        docker.call_count(),
        calls_after_spawn,
        "no `docker rm` for a queued (never-started) job"
    );

    // Once killed, it must stop reporting itself as queued/running.
    let probe = sb.probe(&spawned.handle).await;
    assert!(!probe.running);
}

#[tokio::test]
async fn wait_then_spawn_starts_the_job_once_the_worker_wakes() {
    // Ping 1: asleep (spawn's own check). Ping 2: asleep (first retry).
    // Ping 3: awake (second retry) -> real_spawn's own `docker run -d`.
    let docker: Arc<dyn DockerRun> = Arc::new(FakeDocker::new(|n, args| match n {
        0 | 1 => DockerResult {
            ok: false,
            stdout: String::new(),
            stderr: "refused".to_string(),
        },
        2 => ok_version(),
        _ => {
            assert_eq!(args[0], "run");
            DockerResult {
                ok: true,
                stdout: String::new(),
                stderr: String::new(),
            }
        }
    }));
    let jobs = Arc::new(FakeJobEvents::default());
    let clock = FakeClock::new();
    let sb = WorkerSandbox::with_deps(
        "w1",
        "Workstation",
        docker,
        jobs.clone(),
        clock,
        1,
        WORKER_WAKE_TIMEOUT_MS,
    );

    let spawned = sb.spawn("bot1", "job-1", "sleep 1").await;
    assert!(spawned.detail.contains("queued"));

    // The background wait loop runs on its own tokio task; give it room to
    // finish (the fake clock never really sleeps, so this settles fast).
    for _ in 0..200 {
        if !jobs.finished_failed().is_empty() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(1)).await;
    }
    // It woke up and started cleanly - no failure was ever reported.
    assert!(jobs.finished_failed().is_empty());
}

#[tokio::test]
async fn wait_then_spawn_reclaims_exactly_once_after_the_timeout() {
    let docker = Arc::new(FakeDocker::always_asleep());
    let jobs = Arc::new(FakeJobEvents::default());
    let clock = FakeClock::new();
    // A tiny window: one retry tick already exceeds max_wait_ms, so the
    // loop's very first iteration falls off the deadline.
    let sb = WorkerSandbox::with_deps("w1", "Workstation", docker, jobs.clone(), clock, 10, 5);

    let spawned = sb.spawn("bot1", "job-1", "sleep 1").await;
    assert!(spawned.detail.contains("queued"));

    for _ in 0..500 {
        if !jobs.finished_failed().is_empty() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(1)).await;
    }

    let finished = jobs.finished_failed();
    assert_eq!(
        finished.len(),
        1,
        "the timeout-expiry reclaim must fire exactly once, not zero or twice"
    );
    assert_eq!(finished[0].0, "job-1");
    assert!(finished[0].1.contains("never woke up"));
}

/// Drives the PRODUCTION path (`spawn`, not `try_claim_for_test`) with two
/// calls for the SAME job id while the worker is asleep. Guard-present
/// world: `spawn`'s `if let Some(state) = self.inner.claims.try_claim(...)`
/// (workers.rs:1260) only starts a `wait_then_spawn` background task for the
/// FIRST call; the second call's `try_claim` returns `None` and it starts no
/// task of its own - both calls still answer "queued" (that part of
/// `spawn`'s contract does not change), but only one wait loop exists, so
/// once the worker wakes exactly one `docker run` starts the job. Guard-
/// removed world: `spawn` ignores `try_claim`'s `None` and starts a second
/// wait loop anyway - both loops independently observe the worker waking up
/// and both call `real_spawn`, so `bullpen-job-job-1` gets started (and
/// billed, and occupies the name) TWICE. The observable that differs is the
/// count of recorded `docker` calls whose `args[0] == "run"`: 1 with the
/// guard, 2 without it. `two_concurrent_claimants_cannot_both_win` above
/// does not cover this - it calls `try_claim_for_test` directly and never
/// goes through `spawn` at all.
#[tokio::test]
async fn spawn_called_twice_for_the_same_job_id_starts_the_wait_loop_only_once() {
    // Pings 0 and 1 (spawn's own check, once per call) are asleep. Every
    // ping from then on (the background wait loop's retries) is awake, so
    // whichever wait loop(s) exist will see the worker wake on their very
    // first retry.
    let ping_calls = Arc::new(AtomicU64::new(0));
    let ping_calls_r = ping_calls.clone();
    let docker = Arc::new(FakeDocker::new(move |_, args| {
        if args[0] == "version" {
            let v = ping_calls_r.fetch_add(1, Ordering::SeqCst);
            if v < 2 {
                DockerResult {
                    ok: false,
                    stdout: String::new(),
                    stderr: "refused".to_string(),
                }
            } else {
                ok_version()
            }
        } else {
            assert_eq!(args[0], "run");
            DockerResult {
                ok: true,
                stdout: String::new(),
                stderr: String::new(),
            }
        }
    }));
    let jobs = Arc::new(FakeJobEvents::default());
    let clock = FakeClock::new();
    let sb = WorkerSandbox::with_deps(
        "w1",
        "Workstation",
        docker.clone(),
        jobs.clone(),
        clock,
        1,
        WORKER_WAKE_TIMEOUT_MS,
    );

    let first = sb.spawn("bot1", "job-1", "sleep 1").await;
    let second = sb.spawn("bot1", "job-1", "sleep 1").await;
    assert!(first.detail.contains("queued"));
    assert!(second.detail.contains("queued"));

    // Let any background wait loop(s) settle - the fake clock never really
    // sleeps, so a correct single loop finishes almost immediately. Keep
    // polling a bit past the first "run" so a SECOND loop (the bug) has room
    // to also fire before we count.
    let mut run_calls = 0;
    for _ in 0..300 {
        run_calls = docker.calls().iter().filter(|c| c[0] == "run").count();
        if run_calls >= 1 {
            tokio::time::sleep(Duration::from_millis(20)).await;
            run_calls = docker.calls().iter().filter(|c| c[0] == "run").count();
            break;
        }
        tokio::time::sleep(Duration::from_millis(1)).await;
    }

    assert_eq!(
        run_calls, 1,
        "job-1 must be started exactly once even though spawn() was called \
         twice with the same job id while the worker was asleep - the \
         second call's try_claim(None) must not start its own wait loop"
    );
}

/* ----------------------------------------------------- the concurrency bite */

/// Drives TWO genuinely concurrent tokio tasks (multi-thread runtime,
/// `tokio::spawn`, synchronized with a `Barrier` so both reach
/// `try_claim_for_test` at the same instant) racing to claim the SAME job
/// id. With the guard in place (a single `std::sync::Mutex` critical section
/// covering both the "already claimed?" check and the insert), exactly one
/// of the two must win.
///
/// This drives `WorkerSandbox::try_claim_for_test` directly rather than the
/// full `spawn()` -> `ping()` -> `wait_then_spawn()` chain: an earlier
/// version of this test tried to infer "who won" by counting `docker` calls
/// through `spawn`, and hung - under REAL concurrency, a winning claimant's
/// background wait loop can issue its own retry ping before the OTHER
/// claimant has even made its first ping, so "the first two docker calls
/// are the two claimants" is not a safe assumption once you are actually
/// racing tasks (as opposed to sequencing them, which is the exact mistake
/// this ticket warns against making in the other direction). Testing the
/// guard directly removes that ordering ambiguity entirely while still
/// exercising the identical code path `spawn` uses to decide who may start
/// the wait loop.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn two_concurrent_claimants_cannot_both_win() {
    for attempt in 1..=3 {
        let docker: Arc<dyn DockerRun> = Arc::new(FakeDocker::always_asleep());
        let jobs: Arc<dyn JobEvents> = Arc::new(FakeJobEvents::default());
        let sb = Arc::new(sandbox(docker, jobs));

        // `spawn_blocking` (tokio's dedicated blocking-thread pool) instead
        // of plain `tokio::spawn`, PLUS a std (not tokio) `Barrier`: a
        // cooperatively-scheduled `tokio::spawn` task gives no guarantee two
        // tasks actually land on two DIFFERENT OS threads at the same
        // instant - tokio's work-stealing scheduler can (and, empirically
        // here, sometimes did) place both on one worker's local queue and
        // run them back-to-back, which is not a race at all and made this
        // test flaky. `spawn_blocking` guarantees two real, independent OS
        // threads; the std `Barrier::wait()` (a real blocking join, not an
        // `.await` point) forces both to reach `try_claim_for_test` at the
        // same instant on those threads.
        let barrier = Arc::new(std::sync::Barrier::new(2));
        let mut handles = Vec::new();
        for _ in 0..2 {
            let sb = Arc::clone(&sb);
            let barrier = Arc::clone(&barrier);
            handles.push(tokio::task::spawn_blocking(move || {
                barrier.wait();
                sb.try_claim_for_test("job-1")
            }));
        }

        let mut winners = 0;
        for h in handles {
            if h.await.expect("claimant task panicked") {
                winners += 1;
            }
        }

        assert_eq!(
            winners, 1,
            "attempt {attempt}: exactly one of two genuinely concurrent claimants must win the \
             claim for job-1, got {winners} winners"
        );
    }
}
