//! Run-scoped screen observations and the process-wide admission bounds that
//! keep capture, decoded frames, and image dispatch finite.

use std::collections::HashMap;
use std::fmt;
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};

use rusqlite::OptionalExtension;
use store::Db;
use tokio::sync::{OwnedSemaphorePermit, Semaphore};

use crate::vm::CapturedFrame;

pub const MAX_RETAINED_FRAMES: usize = 4;
pub const MAX_CONCURRENT_DECODES: usize = 2;
pub const MAX_CONCURRENT_DISPATCHES: usize = 2;
pub const MAX_CAPTURE_ATTEMPTS_PER_RUN: u32 = 8;
pub const MAX_IMAGE_DISPATCHES_PER_RUN: u32 = 8;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AdmissionSnapshot {
    pub retained_in_use: usize,
    pub capture_decode_in_use: usize,
    pub dispatch_in_use: usize,
}

pub struct ObservationAdmission {
    retained: Arc<Semaphore>,
    capture_decode: Arc<Semaphore>,
    dispatch: Arc<Semaphore>,
}

impl Default for ObservationAdmission {
    fn default() -> Self {
        Self::new()
    }
}

impl ObservationAdmission {
    pub fn new() -> Self {
        Self {
            retained: Arc::new(Semaphore::new(MAX_RETAINED_FRAMES)),
            capture_decode: Arc::new(Semaphore::new(MAX_CONCURRENT_DECODES)),
            dispatch: Arc::new(Semaphore::new(MAX_CONCURRENT_DISPATCHES)),
        }
    }

    pub fn try_begin_capture(self: &Arc<Self>) -> Option<CaptureReservation> {
        let retained = Arc::clone(&self.retained).try_acquire_owned().ok()?;
        let decode = Arc::clone(&self.capture_decode).try_acquire_owned().ok()?;
        Some(CaptureReservation { retained, decode })
    }

    pub fn try_begin_dispatch(self: &Arc<Self>) -> Option<DispatchReservation> {
        Arc::clone(&self.dispatch)
            .try_acquire_owned()
            .ok()
            .map(|permit| DispatchReservation { _permit: permit })
    }

    pub fn snapshot(&self) -> AdmissionSnapshot {
        AdmissionSnapshot {
            retained_in_use: MAX_RETAINED_FRAMES - self.retained.available_permits(),
            capture_decode_in_use: MAX_CONCURRENT_DECODES - self.capture_decode.available_permits(),
            dispatch_in_use: MAX_CONCURRENT_DISPATCHES - self.dispatch.available_permits(),
        }
    }
}

pub struct CaptureReservation {
    retained: OwnedSemaphorePermit,
    decode: OwnedSemaphorePermit,
}

impl CaptureReservation {
    #[allow(clippy::too_many_arguments)]
    pub fn retain(
        self,
        frame: CapturedFrame,
        run_id: impl Into<String>,
        bot_id: impl Into<String>,
        observation_id: impl Into<String>,
        captured_at: impl Into<String>,
        desktop_generation: u64,
    ) -> ScreenObservation {
        let CaptureReservation { retained, decode } = self;
        drop(decode);
        ScreenObservation {
            metadata: ObservationMetadata {
                run_id: run_id.into(),
                bot_id: bot_id.into(),
                observation_id: observation_id.into(),
                captured_at: captured_at.into(),
                width: frame.width,
                height: frame.height,
                desktop_generation,
                encoded_bytes: frame.png.len(),
            },
            frame,
            _retained: retained,
        }
    }
}

pub struct DispatchReservation {
    _permit: OwnedSemaphorePermit,
}

impl fmt::Debug for DispatchReservation {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("DispatchReservation")
            .finish_non_exhaustive()
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ObservationMetadata {
    pub run_id: String,
    pub bot_id: String,
    pub observation_id: String,
    pub captured_at: String,
    pub width: u32,
    pub height: u32,
    pub desktop_generation: u64,
    pub encoded_bytes: usize,
}

pub struct ScreenObservation {
    metadata: ObservationMetadata,
    frame: CapturedFrame,
    _retained: OwnedSemaphorePermit,
}

impl ScreenObservation {
    pub fn metadata(&self) -> &ObservationMetadata {
        &self.metadata
    }

    pub fn png(&self) -> &[u8] {
        &self.frame.png
    }
}

impl fmt::Debug for ScreenObservation {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ScreenObservation")
            .field("metadata", &self.metadata)
            .finish()
    }
}

#[derive(Default)]
pub struct ObservationRegistry {
    frames: Mutex<HashMap<String, ScreenObservation>>,
}

impl ObservationRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    fn frames(&self) -> MutexGuard<'_, HashMap<String, ScreenObservation>> {
        self.frames.lock().unwrap_or_else(PoisonError::into_inner)
    }

    pub fn replace(&self, observation: ScreenObservation) -> bool {
        let run_id = observation.metadata.run_id.clone();
        self.frames().insert(run_id, observation).is_some()
    }

    pub fn metadata(&self, run_id: &str) -> Option<ObservationMetadata> {
        self.frames()
            .get(run_id)
            .map(|observation| observation.metadata.clone())
    }

    pub fn release(&self, run_id: &str) -> bool {
        self.frames().remove(run_id).is_some()
    }

    pub fn len(&self) -> usize {
        self.frames().len()
    }

    pub fn is_empty(&self) -> bool {
        self.frames().is_empty()
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RunImageCounters {
    pub capture_attempts: u32,
    pub image_dispatches: u32,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CounterClaim {
    Claimed(RunImageCounters),
    Exhausted(RunImageCounters),
    MissingRun,
}

pub fn read_run_image_counters(
    db: &Db,
    run_id: &str,
) -> rusqlite::Result<Option<RunImageCounters>> {
    db.conn()
        .query_row(
            "SELECT screen_capture_attempts, screen_image_dispatches FROM runs WHERE id = ?1",
            [run_id],
            |row| {
                Ok(RunImageCounters {
                    capture_attempts: row.get(0)?,
                    image_dispatches: row.get(1)?,
                })
            },
        )
        .optional()
}

fn claim_counter(
    db: &Db,
    run_id: &str,
    column: &str,
    limit: u32,
) -> rusqlite::Result<CounterClaim> {
    let sql = format!("UPDATE runs SET {column} = {column} + 1 WHERE id = ?1 AND {column} < ?2");
    let changed = db.conn().execute(&sql, rusqlite::params![run_id, limit])?;
    let Some(counters) = read_run_image_counters(db, run_id)? else {
        return Ok(CounterClaim::MissingRun);
    };
    if changed == 1 {
        Ok(CounterClaim::Claimed(counters))
    } else {
        Ok(CounterClaim::Exhausted(counters))
    }
}

pub fn claim_capture_attempt(db: &Db, run_id: &str) -> rusqlite::Result<CounterClaim> {
    claim_counter(
        db,
        run_id,
        "screen_capture_attempts",
        MAX_CAPTURE_ATTEMPTS_PER_RUN,
    )
}

pub fn claim_image_dispatch(db: &Db, run_id: &str) -> rusqlite::Result<CounterClaim> {
    claim_counter(
        db,
        run_id,
        "screen_image_dispatches",
        MAX_IMAGE_DISPATCHES_PER_RUN,
    )
}
