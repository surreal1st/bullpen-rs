//! Run-scoped screen observations and the shared S8d observation-pipeline
//! admission bounds. Thumbnail caching predates and does not use this budget.

use std::collections::HashMap;
use std::fmt;
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::Duration;

use rusqlite::OptionalExtension;
use store::Db;
use tokio::sync::{OwnedSemaphorePermit, Semaphore};
use tokio::time::Instant;

use crate::vm::CapturedFrame;

pub const MAX_RETAINED_FRAMES: usize = 4;
pub const MAX_CONCURRENT_DECODES: usize = 2;
pub const MAX_CONCURRENT_DISPATCHES: usize = 2;
pub const MAX_CAPTURE_ATTEMPTS_PER_RUN: u32 = 8;
pub const MAX_IMAGE_DISPATCHES_PER_RUN: u32 = 8;
pub const OBSERVATION_MAX_AGE: Duration = Duration::from_secs(30);

#[derive(Debug, Default)]
pub struct BotDesktopState {
    generation: u64,
}

impl BotDesktopState {
    pub fn generation(&self) -> u64 {
        self.generation
    }

    pub fn advance(&mut self) -> u64 {
        self.generation = self.generation.saturating_add(1);
        self.generation
    }
}

#[derive(Default)]
pub struct DesktopStateRegistry {
    bots: Mutex<HashMap<String, Arc<tokio::sync::Mutex<BotDesktopState>>>>,
}

impl DesktopStateRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn for_bot(&self, bot_id: &str) -> Arc<tokio::sync::Mutex<BotDesktopState>> {
        let mut bots = self.bots.lock().unwrap_or_else(PoisonError::into_inner);
        Arc::clone(
            bots.entry(bot_id.to_string())
                .or_insert_with(|| Arc::new(tokio::sync::Mutex::new(BotDesktopState::default()))),
        )
    }
}

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
    pub fn into_worker(
        self,
        desktop: tokio::sync::OwnedMutexGuard<BotDesktopState>,
    ) -> CaptureWorkerLease {
        CaptureWorkerLease {
            retained: self.retained,
            decode: self.decode,
            desktop,
        }
    }

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
        RetainedFrameReservation { retained }.retain(
            frame,
            run_id,
            bot_id,
            observation_id,
            captured_at,
            desktop_generation,
        )
    }
}

pub struct CaptureWorkerLease {
    retained: OwnedSemaphorePermit,
    decode: OwnedSemaphorePermit,
    desktop: tokio::sync::OwnedMutexGuard<BotDesktopState>,
}

impl fmt::Debug for CaptureWorkerLease {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("CaptureWorkerLease").finish_non_exhaustive()
    }
}

pub struct CapturedObservationFrame {
    frame: CapturedFrame,
    retained: OwnedSemaphorePermit,
    decode: OwnedSemaphorePermit,
    desktop: tokio::sync::OwnedMutexGuard<BotDesktopState>,
}

impl CaptureWorkerLease {
    pub fn finish(self, frame: CapturedFrame) -> CapturedObservationFrame {
        CapturedObservationFrame {
            frame,
            retained: self.retained,
            decode: self.decode,
            desktop: self.desktop,
        }
    }
}

impl CapturedObservationFrame {
    pub fn into_frame(
        self,
    ) -> (
        CapturedFrame,
        RetainedFrameReservation,
        tokio::sync::OwnedMutexGuard<BotDesktopState>,
    ) {
        let CapturedObservationFrame {
            frame,
            retained,
            decode,
            desktop,
        } = self;
        drop(decode);
        (frame, RetainedFrameReservation { retained }, desktop)
    }

    pub(crate) fn dispose(self) {
        let CapturedObservationFrame {
            frame,
            retained,
            decode,
            desktop,
        } = self;
        drop(frame);
        drop(retained);
        drop(decode);
        drop(desktop);
    }
}

pub struct RetainedFrameReservation {
    retained: OwnedSemaphorePermit,
}

impl RetainedFrameReservation {
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
        let CapturedFrame { png, width, height } = frame;
        ScreenObservation {
            metadata: ObservationMetadata {
                run_id: run_id.into(),
                bot_id: bot_id.into(),
                observation_id: observation_id.into(),
                captured_at: captured_at.into(),
                width,
                height,
                desktop_generation,
                encoded_bytes: png.len(),
            },
            payload: Arc::new(FramePayload {
                png,
                _retained: self.retained,
            }),
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
    payload: Arc<FramePayload>,
}

struct FramePayload {
    png: Vec<u8>,
    _retained: OwnedSemaphorePermit,
}

/// A shared PNG handle keeps the frame's admission reservation alive.
/// Byte access borrows the handle, so it cannot escape its owner.
#[derive(Clone)]
pub struct SharedPng {
    payload: Arc<FramePayload>,
}

impl AsRef<[u8]> for SharedPng {
    fn as_ref(&self) -> &[u8] {
        &self.payload.png
    }
}

impl ScreenObservation {
    pub fn metadata(&self) -> &ObservationMetadata {
        &self.metadata
    }

    pub fn png(&self) -> &[u8] {
        &self.payload.png
    }

    pub fn png_arc(&self) -> SharedPng {
        SharedPng {
            payload: Arc::clone(&self.payload),
        }
    }
}

pub struct ObservationDispatch {
    observation: Option<ScreenObservation>,
    registry: Arc<ObservationRegistry>,
    run_id: String,
    completed: bool,
    _reservation: DispatchReservation,
}

impl ObservationDispatch {
    pub fn new(
        observation: ScreenObservation,
        registry: Arc<ObservationRegistry>,
        reservation: DispatchReservation,
    ) -> Self {
        let run_id = observation.metadata.run_id.clone();
        Self {
            observation: Some(observation),
            registry,
            run_id,
            completed: false,
            _reservation: reservation,
        }
    }

    pub fn observation(&self) -> &ScreenObservation {
        self.observation
            .as_ref()
            .expect("dispatch observation consumed only during drop")
    }

    pub fn complete(mut self) {
        self.observation.take();
        self.completed = true;
    }
}

impl fmt::Debug for ObservationDispatch {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ObservationDispatch")
            .field("metadata", &self.observation().metadata)
            .finish()
    }
}

impl Drop for ObservationDispatch {
    fn drop(&mut self) {
        self.observation.take();
        if self.completed {
            self.registry.release_bytes(&self.run_id);
        } else {
            self.registry.release(&self.run_id);
        }
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
    frames: Mutex<HashMap<String, RegistryObservation>>,
}

struct RegistryObservation {
    metadata: ObservationMetadata,
    payload: Option<Arc<FramePayload>>,
    captured_monotonic: Instant,
}

impl ObservationRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    fn frames(&self) -> MutexGuard<'_, HashMap<String, RegistryObservation>> {
        self.frames.lock().unwrap_or_else(PoisonError::into_inner)
    }

    pub fn replace(&self, observation: ScreenObservation) -> bool {
        let run_id = observation.metadata.run_id.clone();
        self.frames()
            .insert(
                run_id,
                RegistryObservation {
                    metadata: observation.metadata,
                    payload: Some(observation.payload),
                    captured_monotonic: Instant::now(),
                },
            )
            .is_some()
    }

    pub fn store(&self, observation: &ScreenObservation) -> bool {
        let run_id = observation.metadata.run_id.clone();
        self.frames()
            .insert(
                run_id,
                RegistryObservation {
                    metadata: observation.metadata.clone(),
                    payload: Some(Arc::clone(&observation.payload)),
                    captured_monotonic: Instant::now(),
                },
            )
            .is_some()
    }

    pub fn metadata(&self, run_id: &str) -> Option<ObservationMetadata> {
        self.frames()
            .get(run_id)
            .map(|observation| observation.metadata.clone())
    }

    pub fn has_bytes(&self, run_id: &str) -> bool {
        self.frames()
            .get(run_id)
            .is_some_and(|observation| observation.payload.is_some())
    }

    pub fn release_bytes(&self, run_id: &str) -> bool {
        let mut frames = self.frames();
        let Some(observation) = frames.get_mut(run_id) else {
            return false;
        };
        observation.payload.take().is_some()
    }

    pub fn release(&self, run_id: &str) -> bool {
        self.frames().remove(run_id).is_some()
    }

    pub fn invalidate_bot(&self, bot_id: &str) -> usize {
        let mut frames = self.frames();
        let before = frames.len();
        frames.retain(|_, observation| observation.metadata.bot_id != bot_id);
        before - frames.len()
    }

    pub fn consume_coordinates(
        &self,
        run_id: &str,
        bot_id: &str,
        observation_id: &str,
        desktop_generation: u64,
        points: &[(u32, u32)],
    ) -> Result<ObservationMetadata, String> {
        self.consume_coordinates_at(
            run_id,
            bot_id,
            observation_id,
            desktop_generation,
            points,
            Instant::now(),
        )
    }

    pub fn consume_coordinates_at(
        &self,
        run_id: &str,
        bot_id: &str,
        observation_id: &str,
        desktop_generation: u64,
        points: &[(u32, u32)],
        now: Instant,
    ) -> Result<ObservationMetadata, String> {
        let mut frames = self.frames();
        let Some(current) = frames.get(run_id) else {
            return Err(
                "No current screen observation belongs to this run. Capture again.".to_string(),
            );
        };
        if current.metadata.observation_id != observation_id {
            return Err(
                "That screen observation is not this run's latest observation. Capture again."
                    .to_string(),
            );
        }
        let current = frames
            .remove(run_id)
            .expect("matching observation remained present while registry was locked");
        if current.metadata.bot_id != bot_id {
            return Err(
                "That screen observation belongs to another bot. Capture again.".to_string(),
            );
        }
        if current.metadata.desktop_generation != desktop_generation {
            return Err("The desktop changed after that observation. Capture again.".to_string());
        }
        if now.saturating_duration_since(current.captured_monotonic) > OBSERVATION_MAX_AGE {
            return Err(
                "That screen observation is older than 30 seconds. Capture again.".to_string(),
            );
        }
        if points
            .iter()
            .any(|(x, y)| *x >= current.metadata.width || *y >= current.metadata.height)
        {
            return Err(format!(
                "Coordinates must stay inside the observed {}x{} native-pixel desktop. Capture again.",
                current.metadata.width, current.metadata.height
            ));
        }
        Ok(current.metadata)
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
