//! VM lifecycle (provision/hibernate) plus the per-bot SCREEN half: a
//! `DeskConfig` seam into `desk.rs` and the thumbnail/viewer/proxy pieces
//! that make "own screen per bot" (Josh, 2026-09-13) real.
//!
//! Ported from TypeScript `src/server/vm.ts` lines 230-741, by export name
//! (S6-06b corrects S6-06's mis-specification - see
//! `.scratch/bullpen-rs/tickets/S6-tickets.md`). `ensure_vm`/`refresh_vm`/
//! `touch_vm`/`hibernate_idle` landed in S6-06 and are NOT rewritten here.
//! `reset`/`recreate`/`doctor` do not exist in the TS and are not ported -
//! they were never real exports, just prose copied from a plan summary.
//!
//! All docker calls (except `capture_frame`, see its own doc) go through
//! an injected `DockerRun` trait so tests use a fake without needing a
//! real daemon. **There is no Docker and no browser on this workstation**;
//! nothing here may claim a container, a screen capture or a socket
//! actually worked - the real proof is a smoke test on meridian.

use chrono::Utc;
use std::collections::HashMap;
use std::sync::{Arc, Mutex, OnceLock, PoisonError};
use std::time::Duration;
use store::Db;
use store::vms::{
    DockerResult, VmConfig, VmRow, config_volume_for, container_for, get_vm, list_vms,
    parse_container_state,
};

/// Injected docker runner, same pattern as CommandRunner in sandbox.rs.
/// Every call is mocked in tests so no real container is started.
#[async_trait::async_trait]
pub trait DockerRun: Send + Sync {
    /// Runs a docker command with the given arguments and optional timeout.
    /// Returns the result with ok flag, stdout, and stderr.
    async fn call(&self, args: &[&str], timeout_ms: u64) -> DockerResult;
}

/// The result of ensuring a VM exists.
#[derive(Debug, Clone)]
pub struct EnsureOutcome {
    pub ok: bool,
    pub vm: Option<VmRow>,
    /// True when this call created the container rather than finding it.
    pub created: bool,
    pub detail: String,
}

/// The arguments that create one bot's machine.
///
/// Every value here was read off the running `bullpen-desk` with `docker
/// inspect` rather than remembered, so a VM boots the way the desk already
/// does. The three deliberate differences are commented in the TS source.
///
/// Port of TS `createArgs` (`vm.ts:253`) - `pub` (TS reconciliation: the
/// landed S6-06 version was private; the export list this ticket ports
/// against names it, and its body already matches the TS argument order
/// byte for byte, so only the visibility changes).
pub fn create_args(row: &VmRow, bot_name: &str, cfg: &VmConfig) -> Vec<String> {
    vec![
        "run".to_string(),
        "-d".to_string(),
        "--name".to_string(),
        row.container.clone(),
        "--restart".to_string(),
        "unless-stopped".to_string(),
        "-e".to_string(),
        format!("PUID={}", cfg.puid),
        "-e".to_string(),
        format!("PGID={}", cfg.pgid),
        "-e".to_string(),
        format!("TZ={}", cfg.timezone),
        "-e".to_string(),
        format!("TITLE={}'s screen", bot_name),
        "--shm-size".to_string(),
        cfg.shm_size.clone(),
        "--security-opt".to_string(),
        "no-new-privileges".to_string(),
        "--memory".to_string(),
        cfg.memory.clone(),
        "--cpus".to_string(),
        cfg.cpus.clone(),
        "-v".to_string(),
        format!("{}:/config", config_volume_for(&row.bot_id)),
        "-v".to_string(),
        format!("bullpen-work-{}:/work", row.bot_id), // Work volume from sandbox.rs pattern
        "-v".to_string(),
        format!("{}:/custom-cont-init.d:ro", cfg.init_dir),
        "-p".to_string(),
        format!("127.0.0.1:{}:9223", row.cdp_port),
        "-p".to_string(),
        format!("127.0.0.1:{}:3000", row.web_port),
        cfg.image.clone(),
    ]
}

/// The machine this bot works on, created or woken as needed.
pub async fn ensure_vm(
    db: &Db,
    docker: Arc<dyn DockerRun>,
    bot_id: &str,
    bot_name: &str,
    cfg: &VmConfig,
) -> rusqlite::Result<EnsureOutcome> {
    let now = Utc::now().to_rfc3339();
    let mut row = get_vm(db, bot_id)?;

    if row.is_none() {
        let slot = list_vms(db).ok().and_then(|vms| {
            let slots: Vec<i32> = vms.iter().map(|v| v.cdp_port).collect();
            next_slot(&slots, cfg)
        });

        if slot.is_none() {
            return Ok(EnsureOutcome {
                ok: false,
                vm: None,
                created: false,
                detail: format!(
                    "Every one of the {} machine slots is taken. Delete a bot's machine before giving another one.",
                    cfg.slots
                ),
            });
        }

        let slot_idx = slot.unwrap();
        let new_row = VmRow {
            bot_id: bot_id.to_string(),
            container: container_for(bot_id),
            cdp_port: cfg.cdp_base + slot_idx,
            web_port: cfg.web_base + slot_idx,
            state: "new".to_string(),
            last_used_at: now.clone(),
        };

        db.conn().execute(
            "INSERT INTO vms (bot_id, container, cdp_port, web_port, state, last_used_at) VALUES (?, ?, ?, ?, ?, ?)",
            rusqlite::params![
                &new_row.bot_id,
                &new_row.container,
                &new_row.cdp_port,
                &new_row.web_port,
                &new_row.state,
                &new_row.last_used_at
            ],
        )?;

        row = Some(new_row);
    }

    let row = row.unwrap();

    let state = parse_container_state(
        &docker
            .call(
                &[
                    "inspect",
                    "-f",
                    "{{.State.Status}} {{.State.Running}}",
                    &row.container,
                ],
                20_000,
            )
            .await,
    );

    if state.status == "unreachable" {
        touch(db, bot_id, &now, "stopped")?;
        return Ok(EnsureOutcome {
            ok: false,
            vm: Some(VmRow {
                state: "stopped".to_string(),
                ..row.clone()
            }),
            created: false,
            detail: "Docker did not answer, so no machine could be started.".to_string(),
        });
    }

    if !state.exists {
        let args = create_args(&row, bot_name, cfg);
        let arg_refs: Vec<&str> = args.iter().map(|s| s.as_str()).collect();
        let made = docker.call(&arg_refs, 180_000).await;

        if !made.ok {
            touch(db, bot_id, &now, "stopped")?;
            return Ok(EnsureOutcome {
                ok: false,
                vm: Some(VmRow {
                    state: "stopped".to_string(),
                    ..row.clone()
                }),
                created: false,
                detail: first_line(&made.stderr)
                    .or_else(|| Some("docker refused to create the machine.".to_string()))
                    .unwrap(),
            });
        }

        touch(db, bot_id, &now, "starting")?;
        return Ok(EnsureOutcome {
            ok: true,
            vm: Some(VmRow {
                state: "starting".to_string(),
                last_used_at: now.clone(),
                ..row.clone()
            }),
            created: true,
            detail: format!("{}'s machine is booting for the first time.", bot_name),
        });
    }

    if !state.running {
        let started = docker.call(&["start", &row.container], 90_000).await;

        if !started.ok {
            touch(db, bot_id, &now, "stopped")?;
            return Ok(EnsureOutcome {
                ok: false,
                vm: Some(VmRow {
                    state: "stopped".to_string(),
                    ..row.clone()
                }),
                created: false,
                detail: first_line(&started.stderr)
                    .or_else(|| Some("docker refused to start the machine.".to_string()))
                    .unwrap(),
            });
        }

        touch(db, bot_id, &now, "starting")?;
        return Ok(EnsureOutcome {
            ok: true,
            vm: Some(VmRow {
                state: "starting".to_string(),
                last_used_at: now.clone(),
                ..row.clone()
            }),
            created: false,
            detail: format!("{}'s machine is waking up.", bot_name),
        });
    }

    touch(db, bot_id, &now, "running")?;
    Ok(EnsureOutcome {
        ok: true,
        vm: Some(VmRow {
            state: "running".to_string(),
            last_used_at: now.clone(),
            ..row.clone()
        }),
        created: false,
        detail: "running".to_string(),
    })
}

/// Updates a VM row's state and last_used_at.
fn touch(db: &Db, bot_id: &str, stamp: &str, state: &str) -> rusqlite::Result<()> {
    db.conn().execute(
        "UPDATE vms SET state = ?, last_used_at = ? WHERE bot_id = ?",
        rusqlite::params![state, stamp, bot_id],
    )?;
    Ok(())
}

/// Settles a machine the row still calls "starting".
///
/// A read of a "starting" row asks docker once; a container that is up
/// becomes "running". `last_used_at` is left alone: looking is not using.
pub async fn refresh_vm(
    db: &Db,
    docker: Arc<dyn DockerRun>,
    bot_id: &str,
) -> rusqlite::Result<Option<VmRow>> {
    let row = get_vm(db, bot_id)?;
    if row.is_none() || row.as_ref().unwrap().state != "starting" {
        return Ok(row);
    }

    let row = row.unwrap();
    let state = parse_container_state(
        &docker
            .call(
                &[
                    "inspect",
                    "-f",
                    "{{.State.Status}} {{.State.Running}}",
                    &row.container,
                ],
                10_000,
            )
            .await,
    );

    if state.exists && state.running {
        db.conn().execute(
            "UPDATE vms SET state = ? WHERE bot_id = ?",
            rusqlite::params!["running", bot_id],
        )?;
        return Ok(Some(VmRow {
            state: "running".to_string(),
            ..row.clone()
        }));
    }

    if state.exists && !state.running && state.status != "unknown" {
        db.conn().execute(
            "UPDATE vms SET state = ? WHERE bot_id = ?",
            rusqlite::params!["stopped", bot_id],
        )?;
        return Ok(Some(VmRow {
            state: "stopped".to_string(),
            ..row.clone()
        }));
    }

    Ok(Some(row))
}

/// Marks a machine used without touching docker, so idle means idle.
pub fn touch_vm(db: &Db, bot_id: &str) -> rusqlite::Result<()> {
    let now = Utc::now().to_rfc3339();
    db.conn().execute(
        "UPDATE vms SET last_used_at = ? WHERE bot_id = ?",
        rusqlite::params![now, bot_id],
    )?;
    Ok(())
}

/// Extracts the first line of a string, trimmed.
fn first_line(text: &str) -> Option<String> {
    text.trim()
        .lines()
        .next()
        .map(|line| line.trim().to_string())
}

/// Stops every machine nobody has used for `idle_ms`.
///
/// Uses `docker stop`, never `rm`. Hibernation has to be invisible: the
/// profile, the signed-in sessions, the open tabs and /work all survive,
/// and the next `ensure_vm` starts the same container.
pub async fn hibernate_idle(
    db: &Db,
    docker: Arc<dyn DockerRun>,
    cfg: &VmConfig,
) -> rusqlite::Result<Vec<String>> {
    let vms = list_vms(db)?;
    let now = Utc::now().timestamp_millis();
    let cutoff = now - cfg.idle_ms;
    let mut stopped = Vec::new();

    for vm in vms {
        if vm.state != "running" && vm.state != "starting" {
            continue;
        }

        // Parse the ISO timestamp
        let used = chrono::DateTime::parse_from_rfc3339(&vm.last_used_at)
            .ok()
            .map(|dt| dt.timestamp_millis())
            .unwrap_or(0);

        if used > cutoff {
            continue;
        }

        let result = docker
            .call(&["stop", "-t", "10", &vm.container], 60_000)
            .await;

        db.conn().execute(
            "UPDATE vms SET state = 'stopped' WHERE bot_id = ?",
            rusqlite::params![&vm.bot_id],
        )?;

        if result.ok {
            stopped.push(vm.bot_id);
        }
    }

    Ok(stopped)
}

/// The next free slot from a list of used CDP ports.
fn next_slot(used: &[i32], cfg: &VmConfig) -> Option<i32> {
    for slot in 0..cfg.slots {
        let port = cfg.cdp_base + slot;
        if !used.contains(&port) {
            return Some(slot);
        }
    }
    None
}

/// The idle sweep, on a timer. Started by the entry point, never by a test.
///
/// Port of TS `startVmReaper` (`vm.ts:448`). TS's `db: Db` is a synchronous
/// handle with no lock, so `setInterval` can hold it across every
/// `docker()` await inside `hibernateIdle` for free. This crate's only way
/// to get a `&Db` across an `.await` boundary is `Arc<Mutex<Db>>` (same
/// pattern `AppState::db()`, `routines::fire_due` and `goals::
/// start_goal_scheduler` already use) - and `std::sync::MutexGuard` is
/// never `Send`, so a `tokio::spawn`'d future can never hold one across an
/// `await`. Calling `hibernate_idle` (unmodified, per this ticket's "do
/// not rewrite the four landed functions") with a held guard would not
/// compile here. So this reimplements `hibernate_idle`'s tick body with the
/// SAME cutoff/stop/write logic, taking the lock only for the synchronous
/// list-then-write steps and releasing it before every `docker.call()`
/// await - the same discipline `routines::fire_due` already uses for its
/// own db access. This is a Rust-concurrency reconciliation, not a
/// behavior change: same idle cutoff, same `docker stop -t 10`, same
/// "stopped" write regardless of whether `docker stop` itself succeeded.
pub fn start_vm_reaper(
    db: Arc<Mutex<Db>>,
    docker: Arc<dyn DockerRun>,
    cfg: VmConfig,
    every: Duration,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        let mut interval = tokio::time::interval(every);
        loop {
            interval.tick().await;

            let cutoff = Utc::now().timestamp_millis() - cfg.idle_ms;
            let candidates = {
                let guard = db.lock().unwrap_or_else(PoisonError::into_inner);
                list_vms(&guard).unwrap_or_default()
            };

            for vm in candidates {
                if vm.state != "running" && vm.state != "starting" {
                    continue;
                }
                let used = chrono::DateTime::parse_from_rfc3339(&vm.last_used_at)
                    .ok()
                    .map(|dt| dt.timestamp_millis())
                    .unwrap_or(0);
                if used > cutoff {
                    continue;
                }

                // A container that is already gone is also not running,
                // which is the state this is trying to reach, so both
                // outcomes record the same thing (matches `hibernate_idle`).
                let _ = docker
                    .call(&["stop", "-t", "10", &vm.container], 60_000)
                    .await;

                let guard = db.lock().unwrap_or_else(PoisonError::into_inner);
                let _ = guard.conn().execute(
                    "UPDATE vms SET state = 'stopped' WHERE bot_id = ?",
                    rusqlite::params![&vm.bot_id],
                );
            }
        }
    })
}

/* ------------------------------------------------- the seam back into desk.rs */

/// One bot's machine, shaped as a `DeskConfig`.
///
/// Port of TS `vmDesk` (`vm.ts:472`). This single function is what makes
/// `browse`, `read_page`, `click`, `type_text`, `desk_shell`, `desk_act`,
/// `snap_desk` and `record_desk` (`desk.rs`, ported separately) act on the
/// calling bot's own VM: they all take a `DeskConfig` and none of them
/// needs a line changed.
pub fn vm_desk(vm: &VmRow, cfg: &VmConfig) -> crate::desk::DeskConfig {
    crate::desk::DeskConfig {
        cdp: format!("http://127.0.0.1:{}", vm.cdp_port),
        view: format!("http://127.0.0.1:{}", vm.web_port),
        container: vm.container.clone(),
        docker_host: cfg.docker_host.clone(),
    }
}

/// The desk a bot's tools act on: its OWN machine when per-bot machines
/// are on, the shared desk when they are not.
///
/// Port of TS `deskFor` (`vm.ts:490`). The fallback is on `enabled` alone
/// and never on failure. A VM that exists but will not answer must report
/// that it will not answer - falling back to the shared desk would
/// silently run one bot's errand on the machine holding every other bot's
/// cookies, which is exactly the boundary this slice exists to draw.
///
/// TS's optional-`deps` defaulting (real docker/config/env-read) has no
/// clean Rust equivalent and every other function in this port already
/// takes its dependencies explicit (`ensure_vm`'s `docker`/`cfg`) - so
/// `docker`/`cfg`/`enabled` are plain required parameters here, same
/// convention, not a behavior change.
pub async fn desk_for(
    db: &Db,
    docker: Arc<dyn DockerRun>,
    bot_id: &str,
    bot_name: &str,
    fallback: crate::desk::DeskConfig,
    cfg: &VmConfig,
    enabled: bool,
) -> rusqlite::Result<crate::desk::DeskConfig> {
    if !enabled {
        return Ok(fallback);
    }
    let outcome = ensure_vm(db, docker, bot_id, bot_name, cfg).await?;
    // Only a bot with no slot left has no row at all, and that one
    // genuinely has no machine of its own to act on.
    Ok(match outcome.vm {
        Some(vm) => vm_desk(&vm, cfg),
        None => fallback,
    })
}

/* ---------------------------------------------------------------- the picture */

/// PNG's 8-byte signature. A truncated or error-text "image" fails this.
///
/// Port of TS `isPng` (`vm.ts:511`).
pub fn is_png(bytes: &[u8]) -> bool {
    const SIGNATURE: [u8; 8] = [0x89, 0x50, 0x4e, 0x47, 0x0d, 0x0a, 0x1a, 0x0a];
    if bytes.len() < SIGNATURE.len() + 8 {
        return false;
    }
    bytes[..SIGNATURE.len()] == SIGNATURE
}

/// One frame of the VM's X display, straight out of ffmpeg on stdout.
///
/// Port of TS `captureFrame` (`vm.ts:528`). Bypasses `DockerRun` on
/// purpose, exactly like the TS original: the injected seam's
/// `DockerResult.stdout` is a `String`, and a PNG is binary - forcing it
/// through `String` would corrupt it the moment a byte is not valid UTF-8.
/// This is real `docker exec`, shelled out directly with
/// `tokio::process::Command`, so - same as the TS original - there is no
/// test seam and no unit test for it here either: the only proof it works
/// is the meridian smoke test.
///
/// `-video_size` is read from the running X server rather than assumed: a
/// hardcoded size against a differently-sized desktop photographs the
/// top-left corner, which looks like a working feature. `bash -c`, not a
/// login shell - a login shell prints its profile's output onto stdout,
/// and stdout here IS the PNG.
pub async fn capture_frame(container: &str, cfg: &VmConfig, timeout_ms: u64) -> Option<Vec<u8>> {
    let script = "SIZE=$(xdpyinfo -display :1 2>/dev/null | awk \"/dimensions:/{print \\$2}\"); \
         test -n \"$SIZE\" || SIZE=1280x800; \
         exec ffmpeg -nostdin -loglevel quiet -f x11grab -video_size \"$SIZE\" -i :1 \
         -frames:v 1 -f image2 -vcodec png - 2>/dev/null";

    let mut command = tokio::process::Command::new("docker");
    command
        .env("DOCKER_HOST", &cfg.docker_host)
        .args([
            "exec",
            "-u",
            "abc",
            "-e",
            "HOME=/config",
            container,
            "bash",
            "-c",
            script,
        ])
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null());

    // The bytes are checked, not the exit code: ffmpeg writes a usable
    // frame and then exits non-zero often enough that trusting the code
    // throws away good pictures; a zero exit with 40 bytes of text on
    // stdout would otherwise be served as an image. `is_png` covers both.
    match tokio::time::timeout(Duration::from_millis(timeout_ms), command.output()).await {
        Ok(Ok(out)) if is_png(&out.stdout) => Some(out.stdout),
        _ => None,
    }
}

/// The thumbnail cache.
///
/// Port of TS `thumbnail`/`clearThumbnailCache` (`vm.ts:572`, `vm.ts:588`).
/// Five seconds, because the card refreshes on a 5s timer and the
/// Electron shell, a browser tab and a phone can all be watching the same
/// bot. Without this, three viewers is three ffmpeg processes per bot per
/// five seconds on a box that has already lost every service once to
/// unbounded work.
///
/// TS injects `capture: typeof captureFrame = captureFrame` as a test
/// seam; a free async fn has no equivalent default-parameter trick in
/// Rust, so this takes a `FrameCapture` trait object instead - same DI
/// shape as `DockerRun`, and it is what lets a test assert "cache hit
/// records zero capture calls, cache miss records one" without a real
/// ffmpeg.
const THUMB_TTL_MS: i64 = 5_000;

struct CachedThumb {
    at: i64,
    png: Vec<u8>,
}

static THUMBS: OnceLock<Mutex<HashMap<String, CachedThumb>>> = OnceLock::new();

fn thumbs() -> &'static Mutex<HashMap<String, CachedThumb>> {
    THUMBS.get_or_init(|| Mutex::new(HashMap::new()))
}

/// The real capture behind `thumbnail`'s cache, for production callers.
#[async_trait::async_trait]
pub trait FrameCapture: Send + Sync {
    async fn capture(&self, container: &str, cfg: &VmConfig) -> Option<Vec<u8>>;
}

/// `FrameCapture` wired to the real `capture_frame` (20s timeout, same as
/// TS's default).
pub struct RealFrameCapture;

#[async_trait::async_trait]
impl FrameCapture for RealFrameCapture {
    async fn capture(&self, container: &str, cfg: &VmConfig) -> Option<Vec<u8>> {
        capture_frame(container, cfg, 20_000).await
    }
}

pub async fn thumbnail(
    container: &str,
    cfg: &VmConfig,
    capture: &dyn FrameCapture,
    now_ms: i64,
) -> Option<Vec<u8>> {
    {
        let cached = thumbs().lock().unwrap_or_else(PoisonError::into_inner);
        if let Some(entry) = cached.get(container)
            && now_ms - entry.at < THUMB_TTL_MS
        {
            return Some(entry.png.clone());
        }
    }

    let png = capture.capture(container, cfg).await?;
    thumbs()
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .insert(
            container.to_string(),
            CachedThumb {
                at: now_ms,
                png: png.clone(),
            },
        );
    Some(png)
}

/// Test seam: the cache is module-level, so a test has to be able to empty
/// it. Port of TS `clearThumbnailCache` (`vm.ts:588`).
pub fn clear_thumbnail_cache() {
    thumbs()
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .clear();
}
