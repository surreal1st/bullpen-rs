mod common;

use std::sync::{Arc, Mutex};
use std::time::Duration;

use tokio::time::Instant;

use common::{ScriptedPort, as_port, own_conversation, seed_bot};
use model::ladder::Trigger;
use server::observations::{
    CounterClaim, ObservationAdmission, ObservationRegistry, RunImageCounters,
    claim_capture_attempt, claim_image_dispatch, read_run_image_counters,
};
use server::runs::RunManager;
use server::tools::{RunExecutionContext, ToolOutcome};
use server::vm::CapturedFrame;
use store::Db;

fn frame(marker: u8) -> CapturedFrame {
    CapturedFrame {
        png: vec![marker; 32],
        width: 4,
        height: 8,
    }
}

fn retain(
    admission: &Arc<ObservationAdmission>,
    run_id: &str,
    observation_id: &str,
    marker: u8,
) -> server::observations::ScreenObservation {
    admission
        .try_begin_capture()
        .expect("capture admission")
        .retain(
            frame(marker),
            run_id,
            "arthur",
            observation_id,
            "2026-09-18T12:00:00Z",
            7,
        )
}

#[test]
fn admission_is_fail_fast_and_drop_releases_every_reservation() {
    let admission = Arc::new(ObservationAdmission::new());

    let capture_a = admission.try_begin_capture().expect("capture A");
    let capture_b = admission.try_begin_capture().expect("capture B");
    assert!(admission.try_begin_capture().is_none());
    assert_eq!(admission.snapshot().capture_decode_in_use, 2);
    drop(capture_b);
    assert_eq!(admission.snapshot().capture_decode_in_use, 1);

    let mut retained = vec![capture_a.retain(
        frame(1),
        "run-1",
        "arthur",
        "obs-1",
        "2026-09-18T12:00:00Z",
        0,
    )];
    for n in 2..=4 {
        retained.push(retain(
            &admission,
            &format!("run-{n}"),
            &format!("obs-{n}"),
            n,
        ));
    }
    assert_eq!(admission.snapshot().retained_in_use, 4);
    assert!(admission.try_begin_capture().is_none());
    drop(retained.pop());
    assert!(admission.try_begin_capture().is_some());

    let dispatch_a = admission.try_begin_dispatch().expect("dispatch A");
    let dispatch_b = admission.try_begin_dispatch().expect("dispatch B");
    assert!(admission.try_begin_dispatch().is_none());
    drop(dispatch_a);
    assert!(admission.try_begin_dispatch().is_some());
    drop(dispatch_b);
}

#[test]
fn registry_replacement_and_terminal_release_hold_only_one_frame_per_run() {
    let admission = Arc::new(ObservationAdmission::new());
    let registry = ObservationRegistry::new();

    assert!(!registry.replace(retain(&admission, "run-1", "obs-1", 1)));
    assert_eq!(admission.snapshot().retained_in_use, 1);
    assert!(registry.replace(retain(&admission, "run-1", "obs-2", 2)));
    assert_eq!(registry.len(), 1);
    assert_eq!(admission.snapshot().retained_in_use, 1);
    assert_eq!(
        registry
            .metadata("run-1")
            .expect("current metadata")
            .observation_id,
        "obs-2"
    );
    assert!(registry.release("run-1"));
    assert_eq!(admission.snapshot().retained_in_use, 0);
    assert!(!registry.release("run-1"));
}

#[test]
fn observation_and_tool_outcome_debug_never_include_png_bytes() {
    let admission = Arc::new(ObservationAdmission::new());
    let observation = retain(&admission, "run-1", "obs-1", 0xab);
    let debug = format!("{observation:?}");
    assert!(debug.contains("obs-1"));
    assert!(debug.contains("encoded_bytes"));
    assert!(!debug.contains("171"));

    let outcome = ToolOutcome::with_observation("captured", None, observation);
    let debug = format!("{outcome:?}");
    assert!(debug.contains("obs-1"));
    assert!(!debug.contains("171"));

    let outcome = ToolOutcome::with_observation(
        "must not leak into text-only flow",
        None,
        retain(&admission, "run-2", "obs-2", 0xcd),
    );
    let (text, usage) = outcome.into_text_only("contract test");
    assert_eq!(
        text,
        "This tool returned a screen observation in a context that cannot deliver it."
    );
    assert!(usage.is_none());
}

#[test]
fn coordinate_claims_are_latest_single_use_generation_fresh_and_native_bounded() {
    let admission = Arc::new(ObservationAdmission::new());
    let registry = ObservationRegistry::new();

    registry.replace(retain(&admission, "run-latest", "obs-current", 1));
    let wrong = registry.consume_coordinates("run-latest", "arthur", "obs-old", 7, &[(0, 0)]);
    assert!(wrong.unwrap_err().contains("latest observation"));
    assert_eq!(
        registry.metadata("run-latest").unwrap().observation_id,
        "obs-current"
    );
    assert!(
        registry
            .consume_coordinates("run-latest", "arthur", "obs-current", 7, &[(3, 7)])
            .is_ok()
    );
    assert!(
        registry
            .consume_coordinates("run-latest", "arthur", "obs-current", 7, &[(3, 7)])
            .unwrap_err()
            .contains("No current")
    );

    registry.replace(retain(&admission, "run-generation", "obs-generation", 2));
    assert!(
        registry
            .consume_coordinates("run-generation", "arthur", "obs-generation", 8, &[(0, 0)])
            .unwrap_err()
            .contains("desktop changed")
    );
    assert!(registry.metadata("run-generation").is_none());

    registry.replace(retain(&admission, "run-bounds", "obs-bounds", 3));
    assert!(
        registry
            .consume_coordinates("run-bounds", "arthur", "obs-bounds", 7, &[(4, 0)])
            .unwrap_err()
            .contains("inside the observed 4x8")
    );
    assert!(registry.metadata("run-bounds").is_none());

    registry.replace(retain(&admission, "run-age", "obs-age", 4));
    assert!(
        registry
            .consume_coordinates_at(
                "run-age",
                "arthur",
                "obs-age",
                7,
                &[(0, 0)],
                Instant::now() + Duration::from_secs(31),
            )
            .unwrap_err()
            .contains("older than 30 seconds")
    );
    assert!(registry.metadata("run-age").is_none());
}

#[test]
fn persisted_attempt_and_dispatch_counters_survive_status_changes_and_cap_at_eight() {
    let db = Arc::new(Mutex::new(Db::open(":memory:").expect("open db")));
    seed_bot(&db, "arthur", "Arthur");
    let conversation_id = own_conversation(&db, "arthur");
    {
        let db = db.lock().expect("db mutex poisoned");
        db.conn()
            .execute(
                "INSERT INTO runs (id, bot_id, conversation_id, trigger, status, model, messages, text, created_at, updated_at) \
                 VALUES ('run-1', 'arthur', ?1, 'chat', 'running', 'test/model', '[]', '', '2026-09-18T12:00:00Z', '2026-09-18T12:00:00Z')",
                rusqlite::params![conversation_id],
            )
            .expect("insert run");
    }

    for used in 1..=8 {
        let claim = claim_capture_attempt(&db.lock().expect("db mutex poisoned"), "run-1")
            .expect("claim capture attempt");
        assert_eq!(
            claim,
            CounterClaim::Claimed(RunImageCounters {
                capture_attempts: used,
                image_dispatches: 0,
            })
        );
    }
    assert_eq!(
        claim_capture_attempt(&db.lock().expect("db mutex poisoned"), "run-1")
            .expect("read capture cap"),
        CounterClaim::Exhausted(RunImageCounters {
            capture_attempts: 8,
            image_dispatches: 0,
        })
    );

    {
        let db = db.lock().expect("db mutex poisoned");
        db.conn()
            .execute("UPDATE runs SET status = 'waiting' WHERE id = 'run-1'", [])
            .expect("pause run");
    }
    for used in 1..=8 {
        let claim = claim_image_dispatch(&db.lock().expect("db mutex poisoned"), "run-1")
            .expect("claim image dispatch");
        assert_eq!(
            claim,
            CounterClaim::Claimed(RunImageCounters {
                capture_attempts: 8,
                image_dispatches: used,
            })
        );
    }
    assert_eq!(
        read_run_image_counters(&db.lock().expect("db mutex poisoned"), "run-1")
            .expect("read counters"),
        Some(RunImageCounters {
            capture_attempts: 8,
            image_dispatches: 8,
        })
    );
    assert_eq!(
        claim_image_dispatch(&db.lock().expect("db mutex poisoned"), "run-1")
            .expect("read dispatch cap"),
        CounterClaim::Exhausted(RunImageCounters {
            capture_attempts: 8,
            image_dispatches: 8,
        })
    );
    assert_eq!(
        claim_capture_attempt(&db.lock().expect("db mutex poisoned"), "missing-run")
            .expect("missing run is a closed claim result"),
        CounterClaim::MissingRun
    );
    assert_eq!(
        claim_image_dispatch(&db.lock().expect("db mutex poisoned"), "missing-run")
            .expect("missing run is a closed claim result"),
        CounterClaim::MissingRun
    );
}

#[test]
fn toolboxes_require_an_owning_run_for_capture_capable_model_context() {
    let db = Arc::new(Mutex::new(Db::open(":memory:").expect("open db")));
    seed_bot(&db, "arthur", "Arthur");
    let manager = Arc::new(RunManager::new(
        Arc::clone(&db),
        as_port(ScriptedPort::new(vec![])),
    ));

    let unbound = manager.toolbox_for("arthur", Trigger::Chat, false, "test/model", None);
    assert_eq!(unbound.execution_context(), &RunExecutionContext::Unbound);
    assert_eq!(unbound.run_id(), None);
    assert_eq!(unbound.bot_id(), "arthur");
    let model = manager.toolbox_for_context(
        "arthur",
        Trigger::Chat,
        false,
        "test/model",
        None,
        RunExecutionContext::ModelTurn {
            run_id: "run-1".to_string(),
        },
    );
    assert_eq!(model.run_id(), Some("run-1"));
    assert_eq!(model.bot_id(), "arthur");
    let direct = manager.toolbox_for_context(
        "arthur",
        Trigger::Routine,
        false,
        "test/model",
        None,
        RunExecutionContext::DirectRoutine,
    );
    assert_eq!(
        direct.execution_context(),
        &RunExecutionContext::DirectRoutine
    );
    assert_eq!(direct.run_id(), None);
}

#[tokio::test]
async fn completed_failed_and_stopped_runs_release_retained_images() {
    for mode in ["success", "error", "stop"] {
        let db = Arc::new(Mutex::new(Db::open(":memory:").unwrap()));
        seed_bot(&db, "arthur", "Arthur");
        let conversation_id = own_conversation(&db, "arthur");
        model::routing::set_routing_settings(&db.lock().unwrap(), Some(false), None).unwrap();
        let events = if mode == "error" {
            vec![model::ModelEvent::Error {
                message: "fixture failure".into(),
                status: None,
            }]
        } else {
            common::text_script("finished")
        };
        let manager = Arc::new(RunManager::new(
            Arc::clone(&db),
            as_port(ScriptedPort::new(vec![events])),
        ));
        let run_id = manager.start(server::runs::StartOptions {
            bot_id: "arthur".into(),
            conversation_id,
            model: "test/model".into(),
            messages: vec![model::ModelMessage::user("fixture")],
            trigger: Trigger::Chat,
            room: false,
        });
        let admission = manager.observation_admission();
        let registry = manager.observation_registry();
        registry.replace(retain(&admission, &run_id, "obs-final", 0xab));
        assert_eq!(admission.snapshot().retained_in_use, 1);
        if mode == "stop" {
            manager.stop(&run_id);
        }
        tokio::time::timeout(
            std::time::Duration::from_secs(2),
            common::drain(manager.subscribe(&run_id)),
        )
        .await
        .expect("run settled");
        assert_eq!(
            admission.snapshot().retained_in_use,
            0,
            "{mode} retained image bytes"
        );
        assert!(
            registry.metadata(&run_id).is_none(),
            "{mode} retained an observation"
        );
    }
}

#[test]
fn text_only_consumer_refuses_an_image_and_releases_its_reservation() {
    let admission = Arc::new(ObservationAdmission::new());
    let observation = retain(&admission, "run-1", "obs-1", 0xab);
    let outcome = ToolOutcome::with_observation("capture succeeded", None, observation);
    let (text, usage) = outcome.into_text_only("test routine");
    assert!(text.contains("cannot deliver"));
    assert_ne!(text, "capture succeeded");
    assert!(usage.is_none());
    assert_eq!(admission.snapshot().retained_in_use, 0);
    assert_eq!(
        ToolOutcome::new("ordinary result", None)
            .into_text_only("test routine")
            .0,
        "ordinary result"
    );
}

#[tokio::test]
async fn app_retains_the_configured_capability_catalog() {
    let catalog: Arc<dyn model::Catalog> =
        Arc::new(model::FixtureCatalog::from_json("[]").unwrap());
    let state = server::AppState::with_catalog(Db::open(":memory:").unwrap(), Arc::clone(&catalog));
    assert!(Arc::ptr_eq(&catalog, &state.catalog));
    // Private manager identity is covered by the app module wiring test.
}

#[test]
fn shared_png_handle_keeps_the_retained_reservation_until_last_handle_drops() {
    let admission = Arc::new(ObservationAdmission::new());
    let observation = retain(&admission, "run-shared", "obs-shared", 79);
    let png = observation.png_arc();
    let another = png.clone();
    drop(observation);
    assert_eq!(png.as_ref(), &[79; 32]);
    assert_eq!(admission.snapshot().retained_in_use, 1);
    drop(png);
    assert_eq!(admission.snapshot().retained_in_use, 1);
    drop(another);
    assert_eq!(admission.snapshot().retained_in_use, 0);
}

#[test]
fn coordinate_claims_refuse_cross_run_cross_bot_and_height_boundary() {
    let admission = Arc::new(ObservationAdmission::new());
    let registry = ObservationRegistry::new();
    registry.replace(retain(&admission, "run-owner", "obs-owner", 1));
    assert!(
        registry
            .consume_coordinates("other-run", "arthur", "obs-owner", 7, &[(0, 0)])
            .is_err()
    );
    assert!(registry.metadata("run-owner").is_some());
    assert!(
        registry
            .consume_coordinates("run-owner", "other-bot", "obs-owner", 7, &[(0, 0)])
            .unwrap_err()
            .contains("another bot")
    );
    assert!(registry.metadata("run-owner").is_none());
    registry.replace(retain(&admission, "run-height", "obs-height", 2));
    assert!(
        registry
            .consume_coordinates("run-height", "arthur", "obs-height", 7, &[(0, 8)])
            .unwrap_err()
            .contains("inside the observed")
    );
    assert!(registry.metadata("run-height").is_none());
    assert_eq!(admission.snapshot().retained_in_use, 0);
}
