//! VM lifecycle: provision, recreate, reset, doctor.
//!
//! Ported from TypeScript `src/server/vm.ts` lines 230-741. Manages the
//! lifecycle of per-bot VMs: ensuring they exist, starting/stopping them,
//! and idling them down. All docker calls go through an injected `DockerRun`
//! trait so tests use a fake without needing a real daemon.

use chrono::Utc;
use std::sync::Arc;
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
fn create_args(row: &VmRow, bot_name: &str, cfg: &VmConfig) -> Vec<String> {
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
