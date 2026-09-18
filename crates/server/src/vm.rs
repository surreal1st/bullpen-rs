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
use std::fmt;
use std::io::Cursor;
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

    /// Runs a docker command with `stdin` piped to the child, then closed -
    /// the one thing `call` above cannot do, and the whole reason this
    /// method exists. TS's `deskShellStdin` (`bullpen-night/src/server/
    /// desk.ts:527-565`) has no `DockerRun`-equivalent seam at all - it
    /// shells out to `execFile` directly - so there is no TS signature to
    /// port; this is this Rust port's own seam for the capability TS's
    /// function needs (`docker exec -i` with `child.stdin?.end(stdin)`).
    ///
    /// S8c-02 decision, following that ticket's own recommendation:
    /// DEFAULTED, not required. `DockerRun` has ELEVEN implementors today (2
    /// production in this file, 1 in `tools::desk_shell`'s own test module,
    /// 8 across `tests/desk_routing.rs`, `tests/desk_shell_routing.rs`,
    /// `tests/desk_wake.rs`, `tests/vm.rs`, `tests/vm_routes.rs`,
    /// `tests/workers.rs`) and exactly ONE caller will ever need this
    /// (`deskAction`'s `type` branch, S8c-03) - a required method would mean
    /// editing all eleven for a capability ten of them never exercise.
    ///
    /// The default does NOT forward to `call`. A default that silently
    /// dropped `stdin` and returned whatever `call` gives back for the same
    /// argv would hand a bot's typed text nowhere, then report the ordinary
    /// success `call` happens to return - a bot told its keystrokes landed
    /// when nothing was typed, the exact class of defect this project keeps
    /// paying for (see `RealCdpVersion`'s own doc, S8c-01, for the last
    /// lying default). So the default FAILS LOUDLY: `ok: false`, with
    /// `std::any::type_name::<Self>()` naming the concrete runner that
    /// cannot pipe, so any fake a future test drives through the `type`
    /// path fails with a sentence that says why, instead of a silent
    /// false-positive `ok: true`. Only `RealDockerRun` (below) overrides
    /// this; every one of the ten other implementors keeps its current,
    /// completely unmodified `impl DockerRun` block and inherits this
    /// default as-is.
    async fn call_with_stdin(
        &self,
        _args: &[&str],
        _stdin: &str,
        _timeout_ms: u64,
    ) -> DockerResult {
        DockerResult {
            ok: false,
            stdout: String::new(),
            stderr: format!(
                "{} cannot pipe data to a command's stdin.",
                std::any::type_name::<Self>()
            ),
        }
    }
}

/// The production `DockerRun`: shells out to the real `docker` CLI through
/// the same `CommandRunner` seam `sandbox::DockerSandbox` already drives
/// (`crates/server/src/sandbox.rs`) - S6-W-01's whole point is that this is
/// the ONE place a `docker` command is actually run for real, reusing the
/// same timeout/kill-on-drop/output-capping machinery `TokioRunner` already
/// gives the sandbox, not a second copy of it.
///
/// **No Docker on this workstation.** Every test drives this crate's
/// `RecordingDockerRun`/`FakeRunner`, never this struct - the only proof a
/// real `docker run` for a VM works is the meridian smoke test (S6-W-04).
pub struct RealDockerRun {
    runner: Arc<dyn crate::sandbox::CommandRunner>,
}

impl RealDockerRun {
    /// `runner` must already be configured with the right `DOCKER_HOST`
    /// (`crate::sandbox::TokioRunner::new(cfg.docker_host.clone())`) - this
    /// struct only shapes `DockerRun::call`'s `docker <args>` argv onto
    /// whatever `runner` already knows how to run.
    pub fn new(runner: Arc<dyn crate::sandbox::CommandRunner>) -> Self {
        Self { runner }
    }

    /// The body shared by `call` and `call_with_stdin`: prefix `docker`,
    /// hand `args` and `stdin` to the same `CommandRunner::run`, shape the
    /// result the same way. S8c-02 split this out so "the timeout handling
    /// must match what `call` already does, not a second scheme" (the
    /// ticket's own words) is true by construction - one function, one
    /// `match` on `RunError` - rather than two copies a later edit could
    /// drift apart.
    async fn run_docker(&self, args: &[&str], stdin: Vec<u8>, timeout_ms: u64) -> DockerResult {
        let mut argv = Vec::with_capacity(args.len() + 1);
        argv.push("docker".to_string());
        argv.extend(args.iter().map(|s| s.to_string()));

        // 4 MiB: docker CLI output (an `inspect` format string, a `run`'s
        // container id, or a failure's stderr) is never image bytes -
        // `capture_frame` is the one docker call in this file that carries
        // binary output, and it deliberately bypasses `DockerRun` entirely
        // (see its own doc) for exactly that reason.
        match self
            .runner
            .run(
                argv,
                stdin,
                Duration::from_millis(timeout_ms),
                4 * 1024 * 1024,
            )
            .await
        {
            Ok((stdout, stderr, code)) => DockerResult {
                ok: code == 0,
                stdout,
                stderr,
            },
            Err(crate::sandbox::RunError::Timeout) => DockerResult {
                ok: false,
                stdout: String::new(),
                stderr: "docker did not answer before the timeout.".to_string(),
            },
            Err(crate::sandbox::RunError::Other(e)) => DockerResult {
                ok: false,
                stdout: String::new(),
                stderr: e,
            },
        }
    }
}

#[async_trait::async_trait]
impl DockerRun for RealDockerRun {
    async fn call(&self, args: &[&str], timeout_ms: u64) -> DockerResult {
        self.run_docker(args, Vec::new(), timeout_ms).await
    }

    /// `-i` + a piped, then-closed, stdin - see `DockerRun::call_with_stdin`'s
    /// own doc for why this exists and why it is the only override. `args`
    /// is expected to already contain `-i` (`tools::desk_shell::
    /// desk_shell_stdin` puts it right after `"exec"`, mirroring TS's own
    /// argv at `desk.ts:536-549`): this method does not insert it, the same
    /// way `call` never inserts `-u`/`-w`/`-e` for ITS callers - the caller
    /// assembles the full argv, this method only prefixes `docker` and
    /// threads `stdin`/`timeout_ms` through to `CommandRunner::run`
    /// (`sandbox.rs`), which does the actual `Stdio::piped()` + write +
    /// drop-to-close (see that impl's own doc for why the close lives there
    /// and not here - there is exactly one place in this crate that spawns
    /// a real child process for a docker command, and it should be the only
    /// place that owns its stdin pipe).
    async fn call_with_stdin(&self, args: &[&str], stdin: &str, timeout_ms: u64) -> DockerResult {
        self.run_docker(args, stdin.as_bytes().to_vec(), timeout_ms)
            .await
    }
}

/// `DockerRun` for `BULLPEN_VM` off: never shells out, always refuses. A
/// second gate behind whatever `enabled` check a route already does (S6-W-01
/// bite (b)) - even a route that forgot its own check cannot reach a real
/// container through this, the same defense-in-depth
/// `sandbox::UnavailableSandbox` already gives `shell`/`sandbox_read`.
pub struct DisabledDockerRun;

#[async_trait::async_trait]
impl DockerRun for DisabledDockerRun {
    async fn call(&self, _args: &[&str], _timeout_ms: u64) -> DockerResult {
        DockerResult {
            ok: false,
            stdout: String::new(),
            stderr: "VM support is off here. Set BULLPEN_VM=on where machines are wanted."
                .to_string(),
        }
    }
}

/// The `DockerRun` a production server wires onto `vm.rs`, chosen by
/// `BULLPEN_VM` at startup - same posture `sandbox::default_sandbox` already
/// takes for `BULLPEN_SANDBOX`. `env` is the caller's own snapshot
/// (`std::env::vars().collect()`) rather than reading `std::env` directly in
/// here, so `store::vms::vms_enabled`'s existing signature (already `&HashMap`,
/// used by `AppState::build`'s other env-gated defaults) is the one thing
/// this reads for the decision.
pub fn default_docker_run(env: &HashMap<String, String>, cfg: &VmConfig) -> Arc<dyn DockerRun> {
    if !store::vms::vms_enabled(env) {
        return Arc::new(DisabledDockerRun);
    }
    let runner = Arc::new(crate::sandbox::TokioRunner::new(cfg.docker_host.clone()));
    Arc::new(RealDockerRun::new(runner))
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

/* ------------------------------------------------- Send-safe route wrappers */
//
// `ensure_vm`/`refresh_vm`/`hibernate_idle` above all take `db: &Db` and hold
// that borrow across their own `docker.call().await` - fine for a caller
// that already owns (or, like every test in `tests/vm.rs`, exclusively
// borrows) a `Db` for the call's whole duration. `routes/vms.rs` cannot be
// that caller: `AppState` only ever hands out `Arc<Mutex<Db>>`
// (`db_handle()`), and axum's `Handler` trait requires a route's future to
// be `Send` - which `std::sync::MutexGuard` never is (`start_vm_reaper`'s own
// doc, below, hit this exact wall first), and which a bare `&Db` also never
// is regardless of the guard (`Db` wraps a `rusqlite::Connection`, which is
// deliberately `!Sync`). Holding EITHER across an `.await` makes the
// enclosing future `!Send`, so a route handler can never call `ensure_vm`
// itself directly - `desk.rs`'s `existing_window`/`save_window` split (see
// their doc) hit the identical constraint for `window_for`.
//
// These three reimplement the SAME cutoff/create/start/stop/write logic
// against `Arc<Mutex<Db>>` instead, taking the lock only for each
// synchronous step and dropping it before every `docker.call().await` - the
// same discipline `start_vm_reaper` already uses for `hibernate_idle`'s tick
// body. Per this ticket's constraint, `ensure_vm`/`refresh_vm`/`hibernate_idle`
// themselves are NOT rewritten; these are new siblings, not replacements.

/// `ensure_vm`, reimplemented for a route handler's `Arc<Mutex<Db>>`. See
/// this section's header doc for why `ensure_vm` itself cannot be called
/// from `routes/vms.rs` directly.
pub async fn ensure_vm_in(
    db: &Arc<Mutex<Db>>,
    docker: Arc<dyn DockerRun>,
    bot_id: &str,
    bot_name: &str,
    cfg: &VmConfig,
) -> rusqlite::Result<EnsureOutcome> {
    ensure_vm_in_with_mutation_hook(db, docker, bot_id, bot_name, cfg, || {}).await
}

async fn ensure_vm_in_with_mutation_hook(
    db: &Arc<Mutex<Db>>,
    docker: Arc<dyn DockerRun>,
    bot_id: &str,
    bot_name: &str,
    cfg: &VmConfig,
    mut before_mutation: impl FnMut(),
) -> rusqlite::Result<EnsureOutcome> {
    let now = Utc::now().to_rfc3339();

    let row = {
        let guard = db.lock().unwrap_or_else(PoisonError::into_inner);
        let mut row = get_vm(&guard, bot_id)?;

        if row.is_none() {
            let slot = list_vms(&guard).ok().and_then(|vms| {
                let slots: Vec<i32> = vms.iter().map(|v| v.cdp_port).collect();
                next_slot(&slots, cfg)
            });

            let Some(slot_idx) = slot else {
                return Ok(EnsureOutcome {
                    ok: false,
                    vm: None,
                    created: false,
                    detail: format!(
                        "Every one of the {} machine slots is taken. Delete a bot's machine before giving another one.",
                        cfg.slots
                    ),
                });
            };

            let new_row = VmRow {
                bot_id: bot_id.to_string(),
                container: container_for(bot_id),
                cdp_port: cfg.cdp_base + slot_idx,
                web_port: cfg.web_base + slot_idx,
                state: "new".to_string(),
                last_used_at: now.clone(),
            };

            guard.conn().execute(
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

        row.unwrap()
    };

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
        let guard = db.lock().unwrap_or_else(PoisonError::into_inner);
        touch(&guard, bot_id, &now, "stopped")?;
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
        before_mutation();
        let made = docker.call(&arg_refs, 180_000).await;

        let guard = db.lock().unwrap_or_else(PoisonError::into_inner);
        if !made.ok {
            touch(&guard, bot_id, &now, "stopped")?;
            return Ok(EnsureOutcome {
                ok: false,
                vm: Some(VmRow {
                    state: "stopped".to_string(),
                    ..row.clone()
                }),
                created: false,
                detail: first_line(&made.stderr)
                    .unwrap_or_else(|| "docker refused to create the machine.".to_string()),
            });
        }

        touch(&guard, bot_id, &now, "starting")?;
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
        before_mutation();
        let started = docker.call(&["start", &row.container], 90_000).await;

        let guard = db.lock().unwrap_or_else(PoisonError::into_inner);
        if !started.ok {
            touch(&guard, bot_id, &now, "stopped")?;
            return Ok(EnsureOutcome {
                ok: false,
                vm: Some(VmRow {
                    state: "stopped".to_string(),
                    ..row.clone()
                }),
                created: false,
                detail: first_line(&started.stderr)
                    .unwrap_or_else(|| "docker refused to start the machine.".to_string()),
            });
        }

        touch(&guard, bot_id, &now, "starting")?;
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

    let guard = db.lock().unwrap_or_else(PoisonError::into_inner);
    touch(&guard, bot_id, &now, "running")?;
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

/// `refresh_vm`, reimplemented for a route handler's `Arc<Mutex<Db>>` - see
/// this section's header doc.
/// Ensures a VM while the caller owns this bot's desktop lock.
pub async fn ensure_vm_in_locked(
    db: &Arc<Mutex<Db>>,
    docker: Arc<dyn DockerRun>,
    bot_id: &str,
    bot_name: &str,
    cfg: &VmConfig,
    desktop: &mut crate::observations::BotDesktopState,
    observations: &crate::observations::ObservationRegistry,
) -> rusqlite::Result<EnsureOutcome> {
    ensure_vm_in_with_mutation_hook(db, docker, bot_id, bot_name, cfg, || {
        observations.record_desktop_mutation(bot_id, desktop);
    })
    .await
}

pub async fn ensure_vm_in_owned(
    db: Arc<Mutex<Db>>,
    docker: Arc<dyn DockerRun>,
    bot_id: String,
    bot_name: String,
    cfg: Arc<VmConfig>,
    desktop_states: Arc<crate::observations::DesktopStateRegistry>,
    observations: Arc<crate::observations::ObservationRegistry>,
) -> Result<
    (
        EnsureOutcome,
        tokio::sync::OwnedMutexGuard<crate::observations::BotDesktopState>,
    ),
    String,
> {
    let desktop = desktop_states.for_bot(&bot_id);
    let guard = desktop.lock_owned().await;
    tokio::spawn(async move {
        let mut guard = guard;
        let outcome = ensure_vm_in_locked(
            &db,
            docker,
            &bot_id,
            &bot_name,
            &cfg,
            &mut guard,
            &observations,
        )
        .await
        .map_err(|err| err.to_string())?;
        Ok((outcome, guard))
    })
    .await
    .map_err(|err| format!("VM lifecycle worker failed: {err}"))?
}

pub async fn refresh_vm_in(
    db: &Arc<Mutex<Db>>,
    docker: Arc<dyn DockerRun>,
    bot_id: &str,
) -> rusqlite::Result<Option<VmRow>> {
    let row = {
        let guard = db.lock().unwrap_or_else(PoisonError::into_inner);
        get_vm(&guard, bot_id)?
    };
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
        let guard = db.lock().unwrap_or_else(PoisonError::into_inner);
        guard.conn().execute(
            "UPDATE vms SET state = ? WHERE bot_id = ?",
            rusqlite::params!["running", bot_id],
        )?;
        return Ok(Some(VmRow {
            state: "running".to_string(),
            ..row.clone()
        }));
    }

    if state.exists && !state.running && state.status != "unknown" {
        let guard = db.lock().unwrap_or_else(PoisonError::into_inner);
        guard.conn().execute(
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

/// Refreshes one VM while serializing the state transition with desktop work.
pub async fn refresh_vm_in_tracked(
    db: Arc<Mutex<Db>>,
    docker: Arc<dyn DockerRun>,
    bot_id: String,
    desktop_states: Arc<crate::observations::DesktopStateRegistry>,
    observations: Arc<crate::observations::ObservationRegistry>,
) -> Result<Option<VmRow>, String> {
    tokio::spawn(async move {
        let desktop = desktop_states.for_bot(&bot_id);
        let mut guard = desktop.lock_owned().await;
        let before = {
            let db_guard = db.lock().unwrap_or_else(PoisonError::into_inner);
            get_vm(&db_guard, &bot_id).map_err(|err| err.to_string())?
        };
        let refreshed = refresh_vm_in(&db, docker, &bot_id)
            .await
            .map_err(|err| err.to_string())?;
        if before.as_ref().is_some_and(|vm| vm.state != "stopped")
            && refreshed.as_ref().is_some_and(|vm| vm.state == "stopped")
        {
            observations.record_desktop_mutation(&bot_id, &mut guard);
        }
        Ok(refreshed)
    })
    .await
    .map_err(|err| format!("VM refresh worker failed: {err}"))?
}

pub async fn hibernate_idle_in(
    db: &Arc<Mutex<Db>>,
    docker: Arc<dyn DockerRun>,
    cfg: &VmConfig,
) -> rusqlite::Result<Vec<String>> {
    let now = Utc::now().timestamp_millis();
    let cutoff = now - cfg.idle_ms;

    let candidates = {
        let guard = db.lock().unwrap_or_else(PoisonError::into_inner);
        list_vms(&guard)?
    };

    let mut stopped = Vec::new();
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

        let result = docker
            .call(&["stop", "-t", "10", &vm.container], 60_000)
            .await;

        {
            let guard = db.lock().unwrap_or_else(PoisonError::into_inner);
            guard.conn().execute(
                "UPDATE vms SET state = 'stopped' WHERE bot_id = ?",
                rusqlite::params![&vm.bot_id],
            )?;
        }

        if result.ok {
            stopped.push(vm.bot_id);
        }
    }

    Ok(stopped)
}

/// Stops idle VMs while serializing each stop with that bot's desktop work.
pub async fn hibernate_idle_in_tracked(
    db: Arc<Mutex<Db>>,
    docker: Arc<dyn DockerRun>,
    cfg: Arc<VmConfig>,
    desktop_states: Arc<crate::observations::DesktopStateRegistry>,
    observations: Arc<crate::observations::ObservationRegistry>,
) -> Result<Vec<String>, String> {
    let cutoff = Utc::now().timestamp_millis() - cfg.idle_ms;
    let candidates = {
        let guard = db.lock().unwrap_or_else(PoisonError::into_inner);
        list_vms(&guard).map_err(|err| err.to_string())?
    };
    let mut stopped = Vec::new();
    for candidate in candidates {
        if candidate.state != "running" && candidate.state != "starting" {
            continue;
        }
        let db = Arc::clone(&db);
        let docker = Arc::clone(&docker);
        let states = Arc::clone(&desktop_states);
        let observations = Arc::clone(&observations);
        let bot_id = candidate.bot_id.clone();
        let desktop = states.for_bot(&bot_id);
        let state = desktop.lock_owned().await;
        let result = tokio::spawn(async move {
            let mut state = state;
            let current = {
                let guard = db.lock().unwrap_or_else(PoisonError::into_inner);
                get_vm(&guard, &bot_id).map_err(|err| err.to_string())?
            };
            let Some(current) = current else {
                return Ok::<bool, String>(false);
            };
            if current.state != "running" && current.state != "starting" {
                return Ok(false);
            }
            let used = chrono::DateTime::parse_from_rfc3339(&current.last_used_at)
                .ok()
                .map(|dt| dt.timestamp_millis())
                .unwrap_or(0);
            if used > cutoff {
                return Ok(false);
            }
            observations.record_desktop_mutation(&bot_id, &mut state);
            let result = docker
                .call(&["stop", "-t", "10", &current.container], 60_000)
                .await;
            let guard = db.lock().unwrap_or_else(PoisonError::into_inner);
            guard
                .conn()
                .execute(
                    "UPDATE vms SET state = 'stopped' WHERE bot_id = ?",
                    rusqlite::params![&bot_id],
                )
                .map_err(|err| err.to_string())?;
            Ok(result.ok)
        })
        .await
        .map_err(|err| format!("VM hibernate worker failed: {err}"))??;
        if result {
            stopped.push(candidate.bot_id);
        }
    }
    Ok(stopped)
}

/// What a bot's tools should act on, resolved by `desk_for_in`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DeskResolution {
    /// The bot's own machine.
    Own(crate::desk::DeskConfig),
    /// No machine to hand back - the `String` is why, straight from
    /// `EnsureOutcome::detail`.
    Unavailable(String),
}

/// `desk_for`, reimplemented for a caller holding `Arc<Mutex<Db>>` across an
/// await boundary that must stay `Send` - see this section's header doc for
/// why `desk_for` itself cannot be called from `tools/mod.rs`'s dispatch
/// closure directly (S8a-02: that closure is boxed as `ToolFuture = Pin<Box
/// <dyn Future<...> + Send>>`, the identical wall `ensure_vm_in`'s own doc
/// names for `routes/vms.rs`). `desk_for` itself is left UNCHANGED, same
/// reasoning `ensure_vm`/`refresh_vm`/`hibernate_idle` are not rewritten:
/// it is the correct shape for a caller that already owns (or exclusively
/// borrows) a `Db` - `tests/vm.rs`.
///
/// Deliberately narrower than `desk_for`'s own signature: `enabled` and
/// `fallback` are gone. S8a-02's ONE caller (`desk::cdp_for_bot`) already
/// gates on `vm_enabled` itself before ever reaching this function (see its
/// own doc for why `UnavailableCdp` - not a shared-desk `DeskConfig` - is
/// what "VMs are off" resolves to now), so there is never a `fallback` this
/// function itself would need to invent.
pub async fn desk_for_in(
    db: &Arc<Mutex<Db>>,
    docker: Arc<dyn DockerRun>,
    bot_id: &str,
    bot_name: &str,
    cfg: &VmConfig,
) -> rusqlite::Result<DeskResolution> {
    let outcome = ensure_vm_in(db, docker, bot_id, bot_name, cfg).await?;
    Ok(match outcome.vm {
        Some(vm) => DeskResolution::Own(vm_desk(&vm, cfg)),
        None => DeskResolution::Unavailable(outcome.detail),
    })
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
    cfg: Arc<VmConfig>,
    every: Duration,
    desktop_states: Arc<crate::observations::DesktopStateRegistry>,
    observations: Arc<crate::observations::ObservationRegistry>,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        let mut interval = tokio::time::interval(every);
        loop {
            interval.tick().await;
            if let Err(err) = hibernate_idle_in_tracked(
                Arc::clone(&db),
                Arc::clone(&docker),
                Arc::clone(&cfg),
                Arc::clone(&desktop_states),
                Arc::clone(&observations),
            )
            .await
            {
                tracing::warn!("VM reaper failed: {err}");
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

/// Limits for an uncached observation frame. Rows are decoded one at a time,
/// so validation does not allocate a full 16-bit RGBA image.
pub const MAX_FRAME_PNG_BYTES: usize = 4 * 1024 * 1024;
pub const MAX_FRAME_DIMENSION: u32 = 4_096;
pub const MAX_FRAME_PIXELS: u64 = 8_000_000;
const MAX_FRAME_DECODE_BYTES: usize = 32 * 1024 * 1024;

#[derive(PartialEq, Eq)]
pub struct CapturedFrame {
    pub png: Vec<u8>,
    pub width: u32,
    pub height: u32,
}

impl fmt::Debug for CapturedFrame {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("CapturedFrame")
            .field("encoded_bytes", &self.png.len())
            .field("width", &self.width)
            .field("height", &self.height)
            .finish()
    }
}

/// Validates the whole image with decoder limits applied before headers.
pub fn validate_frame(png_bytes: Vec<u8>) -> Option<CapturedFrame> {
    if png_bytes.len() > MAX_FRAME_PNG_BYTES || !is_png(&png_bytes) {
        return None;
    }
    let decoder = png::Decoder::new_with_limits(
        Cursor::new(&png_bytes),
        png::Limits {
            bytes: MAX_FRAME_DECODE_BYTES,
        },
    );
    let mut reader = decoder.read_info().ok()?;
    let width = reader.info().width;
    let height = reader.info().height;
    if width == 0
        || height == 0
        || width > MAX_FRAME_DIMENSION
        || height > MAX_FRAME_DIMENSION
        || u64::from(width) * u64::from(height) > MAX_FRAME_PIXELS
    {
        return None;
    }
    while reader.next_row().ok()?.is_some() {}
    reader.finish().ok()?;
    Some(CapturedFrame {
        png: png_bytes,
        width,
        height,
    })
}

async fn read_capped_frame_stdout(stdout: &mut tokio::process::ChildStdout) -> Result<Vec<u8>, ()> {
    use tokio::io::AsyncReadExt;
    let mut bytes = Vec::with_capacity(8 * 1024);
    let mut buffer = [0_u8; 8 * 1024];
    loop {
        let remaining = MAX_FRAME_PNG_BYTES.saturating_sub(bytes.len());
        let read_len = buffer.len().min(remaining.saturating_add(1));
        let read = stdout.read(&mut buffer[..read_len]).await.map_err(|_| ())?;
        if read == 0 {
            return Ok(bytes);
        }
        if read > remaining {
            return Err(());
        }
        bytes.extend_from_slice(&buffer[..read]);
    }
}

async fn kill_and_reap(child: &mut tokio::process::Child) {
    let _ = child.kill().await;
    let _ = child.wait().await;
}

/// Worker ownership makes receiver closure an explicit cancellation signal.
async fn collect_frame_from_child<T>(
    mut child: tokio::process::Child,
    timeout: Duration,
    cancelled: &mut tokio::sync::oneshot::Sender<T>,
) -> Option<CapturedFrame> {
    let mut stdout = child.stdout.take()?;
    let deadline = tokio::time::Instant::now() + timeout;
    let bytes = tokio::select! {
        result = read_capped_frame_stdout(&mut stdout) => match result { Ok(bytes) => bytes, Err(()) => { kill_and_reap(&mut child).await; return None; } },
        _ = tokio::time::sleep_until(deadline) => { kill_and_reap(&mut child).await; return None; },
        _ = cancelled.closed() => { kill_and_reap(&mut child).await; return None; },
    };
    tokio::select! {
        _ = child.wait() => {},
        _ = tokio::time::sleep_until(deadline) => { kill_and_reap(&mut child).await; return None; },
        _ = cancelled.closed() => { kill_and_reap(&mut child).await; return None; },
    }
    validate_frame(bytes)
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
/// Bounds the entire remote shell, including X-size discovery. GNU timeout's
/// kill-after prevents a stubborn descendant from surviving its TERM grace.
fn frame_capture_script() -> &'static str {
    "exec timeout -k 5s 15s bash -c 'SIZE=$(xdpyinfo -display :1 2>/dev/null | awk \"/dimensions:/{print \\$2}\"); \
     test -n \"$SIZE\" || exit 64; \
     exec ffmpeg -nostdin -loglevel quiet -f x11grab -video_size \"$SIZE\" -i :1 \
     -frames:v 1 -f image2 -vcodec png - 2>/dev/null'"
}
pub async fn capture_frame(
    container: &str,
    cfg: &VmConfig,
    timeout_ms: u64,
) -> Option<CapturedFrame> {
    let child = spawn_frame_child(container, cfg)?;
    spawn_frame_collection(child, Duration::from_millis(timeout_ms))
        .await
        .ok()
        .flatten()
}

fn spawn_frame_child(container: &str, cfg: &VmConfig) -> Option<tokio::process::Child> {
    let script = frame_capture_script();
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
        .stderr(std::process::Stdio::null())
        .kill_on_drop(true);
    command.spawn().ok()
}
/// Starts the production owner task. Receiver closure is the cancellation
/// signal; this task retains child ownership and always kills then reaps it.
fn spawn_frame_collection(
    child: tokio::process::Child,
    timeout: Duration,
) -> tokio::sync::oneshot::Receiver<Option<CapturedFrame>> {
    let (mut result_tx, result_rx) = tokio::sync::oneshot::channel();
    tokio::spawn(async move {
        let result = collect_frame_from_child(child, timeout, &mut result_tx).await;
        let _ = result_tx.send(result);
    });
    result_rx
}

fn spawn_observation_frame_collection(
    child: tokio::process::Child,
    timeout: Duration,
    worker_lease: crate::observations::CaptureWorkerLease,
) -> tokio::sync::oneshot::Receiver<Option<crate::observations::CapturedObservationFrame>> {
    let (mut result_tx, result_rx) = tokio::sync::oneshot::channel();
    tokio::spawn(async move {
        let result = collect_frame_from_child(child, timeout, &mut result_tx)
            .await
            .map(|frame| worker_lease.finish(frame));
        if let Err(unsent) = result_tx.send(result)
            && let Some(unsent) = unsent
        {
            unsent.dispose();
        }
    });
    result_rx
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
    async fn capture(&self, container: &str, cfg: &VmConfig) -> Option<CapturedFrame>;

    /// Uncached observation capture. Implementors that detach cleanup or
    /// buffer ownership must move the lease into that owner, as the real
    /// implementation does below, rather than relying on this default.
    async fn capture_observation(
        &self,
        container: &str,
        cfg: &VmConfig,
        lease: crate::observations::CaptureWorkerLease,
    ) -> Option<crate::observations::CapturedObservationFrame> {
        self.capture(container, cfg)
            .await
            .map(|frame| lease.finish(frame))
    }
}

/// `FrameCapture` wired to the real `capture_frame` (20s timeout, same as
/// TS's default).
pub struct RealFrameCapture;

#[async_trait::async_trait]
impl FrameCapture for RealFrameCapture {
    async fn capture(&self, container: &str, cfg: &VmConfig) -> Option<CapturedFrame> {
        capture_frame(container, cfg, 20_000).await
    }

    async fn capture_observation(
        &self,
        container: &str,
        cfg: &VmConfig,
        lease: crate::observations::CaptureWorkerLease,
    ) -> Option<crate::observations::CapturedObservationFrame> {
        let child = spawn_frame_child(container, cfg)?;
        spawn_observation_frame_collection(child, Duration::from_millis(20_000), lease)
            .await
            .ok()
            .flatten()
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

    let frame = capture.capture(container, cfg).await?;
    let png = frame.png;
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

#[cfg(test)]
mod capture_process_tests {
    use super::*;
    use std::io::Write;

    #[test]
    fn captured_frame_debug_contains_only_metadata() {
        let frame = CapturedFrame {
            png: vec![0xde, 0xad, 0xbe, 0xef],
            width: 11,
            height: 13,
        };
        let debug = format!("{frame:?}");
        assert!(debug.contains("encoded_bytes: 4"));
        assert!(debug.contains("width: 11"));
        assert!(debug.contains("height: 13"));
        assert!(!debug.contains("222"));
        assert!(!debug.contains("173"));
    }

    #[test]
    fn frame_capture_script_bounds_probe_and_ffmpeg_without_dimension_fallback() {
        let script = frame_capture_script();
        assert!(script.starts_with("exec timeout -k 5s 15s bash -c '"));
        assert!(script.contains("xdpyinfo -display :1"));
        assert!(script.contains("exec ffmpeg"));
        assert!(!script.contains("1280x800"));
    }
    #[test]
    fn capture_child_fixture() {
        let Ok(mode) = std::env::var("S8D_CAPTURE_FIXTURE") else {
            return;
        };
        match mode.as_str() {
            "overflow" => {
                std::io::stdout()
                    .write_all(&vec![b'x'; MAX_FRAME_PNG_BYTES + 1])
                    .unwrap();
                std::io::stdout().flush().unwrap();
            }
            "sleep" => {}
            _ => panic!("unknown fixture mode"),
        }
        std::thread::sleep(Duration::from_secs(10));
    }

    async fn fixture(mode: &str) -> tokio::process::Child {
        let mut command = tokio::process::Command::new(std::env::current_exe().unwrap());
        command
            .args([
                "--exact",
                "vm::capture_process_tests::capture_child_fixture",
                "--nocapture",
            ])
            .env("S8D_CAPTURE_FIXTURE", mode)
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::null())
            .kill_on_drop(true);
        command.spawn().unwrap()
    }

    #[cfg(windows)]
    async fn pid_is_alive(pid: u32) -> bool {
        let output = tokio::process::Command::new("tasklist")
            .args(["/FI", &format!("PID eq {pid}"), "/NH"])
            .output()
            .await
            .unwrap();
        String::from_utf8_lossy(&output.stdout).contains(&pid.to_string())
    }

    #[cfg(unix)]
    async fn pid_is_alive(pid: u32) -> bool {
        tokio::process::Command::new("kill")
            .args(["-0", &pid.to_string()])
            .status()
            .await
            .map(|status| status.success())
            .unwrap_or(false)
    }

    async fn assert_reaped(pid: u32) {
        for _ in 0..20 {
            if !pid_is_alive(pid).await {
                return;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        panic!("capture child PID {pid} survived cleanup");
    }

    #[tokio::test]
    async fn capture_child_overflow_refuses_before_timeout_and_reaps_the_real_process() {
        let child = fixture("overflow").await;
        let pid = child.id().unwrap();
        let (mut tx, _rx) = tokio::sync::oneshot::channel::<Option<CapturedFrame>>();
        let started = tokio::time::Instant::now();
        assert!(
            collect_frame_from_child(child, Duration::from_secs(2), &mut tx)
                .await
                .is_none()
        );
        assert!(
            started.elapsed() < Duration::from_secs(1),
            "overflow must not wait for timeout"
        );
        assert_reaped(pid).await;
    }

    #[tokio::test]
    async fn capture_child_timeout_kills_and_reaps_the_real_process() {
        let child = fixture("sleep").await;
        let pid = child.id().unwrap();
        let (mut tx, _rx) = tokio::sync::oneshot::channel::<Option<CapturedFrame>>();
        let started = std::time::Instant::now();
        assert!(
            collect_frame_from_child(child, Duration::from_millis(30), &mut tx)
                .await
                .is_none()
        );
        assert!(
            started.elapsed() < Duration::from_millis(500),
            "capture exceeded its deadline"
        );
        assert_reaped(pid).await;
    }

    #[tokio::test]
    async fn dropping_a_live_capture_receiver_kills_and_reaps_the_real_process() {
        let child = fixture("sleep").await;
        let pid = child.id().unwrap();
        let receiver = spawn_frame_collection(child, Duration::from_secs(2));
        tokio::time::sleep(Duration::from_millis(30)).await;
        drop(receiver);
        assert_reaped(pid).await;
    }

    #[tokio::test]
    async fn cancelled_real_child_owner_holds_both_capture_leases_until_reaped() {
        let admission = Arc::new(crate::observations::ObservationAdmission::new());
        let desktop = Arc::new(tokio::sync::Mutex::new(
            crate::observations::BotDesktopState::default(),
        ));
        let worker_lease = admission
            .try_begin_capture()
            .expect("capture admission")
            .into_worker(Arc::clone(&desktop).lock_owned().await);
        let child = fixture("sleep").await;
        let pid = child.id().unwrap();
        let receiver =
            spawn_observation_frame_collection(child, Duration::from_secs(2), worker_lease);
        tokio::time::sleep(Duration::from_millis(30)).await;
        assert!(
            pid_is_alive(pid).await,
            "owner child exited before cancellation"
        );
        assert_eq!(admission.snapshot().capture_decode_in_use, 1);
        assert_eq!(admission.snapshot().retained_in_use, 1);
        assert!(
            tokio::time::timeout(Duration::from_millis(30), Arc::clone(&desktop).lock_owned())
                .await
                .is_err(),
            "desktop lock was released while the real capture child was alive"
        );
        drop(receiver);
        while pid_is_alive(pid).await {
            assert_eq!(
                admission.snapshot().capture_decode_in_use,
                1,
                "decode lease released while the child was still alive"
            );
            assert_eq!(
                admission.snapshot().retained_in_use,
                1,
                "retained-frame lease released while the child was still alive"
            );
            assert!(
                tokio::time::timeout(Duration::from_millis(1), Arc::clone(&desktop).lock_owned())
                    .await
                    .is_err(),
                "desktop lock released while the child was still alive"
            );
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        assert_reaped(pid).await;
        tokio::time::timeout(Duration::from_secs(1), async {
            while admission.snapshot().capture_decode_in_use != 0
                || admission.snapshot().retained_in_use != 0
            {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("owner task released decode lease after reap");
        tokio::time::timeout(Duration::from_secs(1), desktop.lock_owned())
            .await
            .expect("owner task released desktop lock after reap");
    }
}
