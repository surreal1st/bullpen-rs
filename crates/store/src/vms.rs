//! VM registry, slots and container state. Byte-compatible with TS Bullpen's
//! vm.ts (lines 41-230 only).

use crate::Db;
use rusqlite::OptionalExtension;

pub struct VmConfig {
    /// Byte-for-byte the desk's image. A second image is a second set of bugs.
    pub image: String,
    pub docker_host: String,
    /// First host port for a VM's CDP bridge. One slot per bot.
    pub cdp_base: i32,
    /// First host port for a VM's web desktop, same slot index as the CDP port.
    pub web_base: i32,
    /// How many bots can have a machine at once.
    pub slots: i32,
    /// Idle time before a VM is stopped. Its disk survives; only the RAM goes.
    pub idle_ms: i64,
    /// Host directory of init scripts, mounted read-only into every VM.
    pub init_dir: String,
    pub memory: String,
    pub cpus: String,
    pub shm_size: String,
    pub timezone: String,
    /// The uid/gid the desktop runs as: meridian's `bullpen` user.
    pub puid: String,
    pub pgid: String,
}

/// OFF unless asked for, exactly like BULLPEN_SANDBOX. A developer machine
/// with a personal Docker must never start fifteen desktops because someone
/// opened a panel, and every test and the preview harness run with it off.
pub fn vms_enabled(env: &std::collections::HashMap<String, String>) -> bool {
    env.get("BULLPEN_VM").map(|v| v == "on").unwrap_or(false)
}

pub fn vm_config(env: &std::collections::HashMap<String, String>) -> VmConfig {
    VmConfig {
        image: env
            .get("BULLPEN_VM_IMAGE")
            .cloned()
            .unwrap_or_else(|| "lscr.io/linuxserver/webtop:ubuntu-xfce".to_string()),
        docker_host: env
            .get("DOCKER_HOST")
            .cloned()
            .unwrap_or_else(|| "unix:///run/user/1004/docker.sock".to_string()),
        cdp_base: env
            .get("BULLPEN_VM_CDP_BASE")
            .and_then(|s| s.parse().ok())
            .unwrap_or(9300),
        web_base: env
            .get("BULLPEN_VM_WEB_BASE")
            .and_then(|s| s.parse().ok())
            .unwrap_or(6200),
        slots: env
            .get("BULLPEN_VM_SLOTS")
            .and_then(|s| s.parse().ok())
            .unwrap_or(24),
        idle_ms: env
            .get("BULLPEN_VM_IDLE_MS")
            .and_then(|s| s.parse().ok())
            .unwrap_or(30 * 60 * 1000),
        init_dir: env
            .get("BULLPEN_VM_INIT_DIR")
            .cloned()
            .unwrap_or_else(|| "/home/bullpen/vm-init".to_string()),
        memory: env
            .get("BULLPEN_VM_MEMORY")
            .cloned()
            .unwrap_or_else(|| "3g".to_string()),
        cpus: env
            .get("BULLPEN_VM_CPUS")
            .cloned()
            .unwrap_or_else(|| "1.5".to_string()),
        shm_size: env
            .get("BULLPEN_VM_SHM")
            .cloned()
            .unwrap_or_else(|| "1g".to_string()),
        timezone: env
            .get("TZ")
            .cloned()
            .unwrap_or_else(|| "America/New_York".to_string()),
        puid: env
            .get("BULLPEN_VM_PUID")
            .cloned()
            .unwrap_or_else(|| "1004".to_string()),
        pgid: env
            .get("BULLPEN_VM_PGID")
            .cloned()
            .unwrap_or_else(|| "1004".to_string()),
    }
}

/* --------------------------------------------------------------- the table */

pub type VmState = str;

#[derive(Clone, Debug)]
pub struct VmRow {
    pub bot_id: String,
    pub container: String,
    pub cdp_port: i32,
    pub web_port: i32,
    pub state: String,
    pub last_used_at: String,
}

#[derive(Clone, Debug)]
struct VmRecord {
    bot_id: String,
    container: String,
    cdp_port: i32,
    web_port: i32,
    state: String,
    last_used_at: String,
}

fn to_row(record: VmRecord) -> VmRow {
    VmRow {
        bot_id: record.bot_id,
        container: record.container,
        cdp_port: record.cdp_port,
        web_port: record.web_port,
        state: if is_vm_state(&record.state) {
            record.state
        } else {
            "new".to_string()
        },
        last_used_at: record.last_used_at,
    }
}

fn is_vm_state(value: &str) -> bool {
    matches!(value, "new" | "starting" | "running" | "stopped")
}

/// Ensures the vms table exists. Called from lib.rs during Db::open.
pub(crate) fn ensure_vm_tables(db: &Db) -> rusqlite::Result<()> {
    db.conn().execute(
        "CREATE TABLE IF NOT EXISTS vms (
      bot_id       TEXT PRIMARY KEY,
      container    TEXT NOT NULL,
      cdp_port     INTEGER NOT NULL,
      web_port     INTEGER NOT NULL,
      state        TEXT NOT NULL,
      last_used_at TEXT NOT NULL
    )",
        [],
    )?;
    Ok(())
}

pub fn get_vm(db: &Db, bot_id: &str) -> rusqlite::Result<Option<VmRow>> {
    db.conn()
        .query_row("SELECT bot_id, container, cdp_port, web_port, state, last_used_at FROM vms WHERE bot_id = ?", [bot_id], |row| {
            Ok(VmRecord {
                bot_id: row.get(0)?,
                container: row.get(1)?,
                cdp_port: row.get(2)?,
                web_port: row.get(3)?,
                state: row.get(4)?,
                last_used_at: row.get(5)?,
            })
        })
        .optional()
        .map(|opt| opt.map(to_row))
}

pub fn list_vms(db: &Db) -> rusqlite::Result<Vec<VmRow>> {
    let mut stmt = db.conn()
        .prepare("SELECT bot_id, container, cdp_port, web_port, state, last_used_at FROM vms ORDER BY cdp_port")?;
    let vms = stmt
        .query_map([], |row| {
            Ok(VmRecord {
                bot_id: row.get(0)?,
                container: row.get(1)?,
                cdp_port: row.get(2)?,
                web_port: row.get(3)?,
                state: row.get(4)?,
                last_used_at: row.get(5)?,
            })
        })?
        .filter_map(|r| r.ok())
        .map(to_row)
        .collect();
    Ok(vms)
}

/// Docker names allow a narrow character set, and a bot id is free text.
pub fn container_for(bot_id: &str) -> String {
    format!("bullpen-vm-{}", sanitize_for_docker(bot_id))
}

/// The VM's own Chromium profile, cookies and desktop settings.
pub fn config_volume_for(bot_id: &str) -> String {
    format!("bullpen-vmcfg-{}", sanitize_for_docker(bot_id))
}

/// Replaces anything that is not alphanumeric/`_`/`.`/`-` with `_`. Allows `-`
/// at the start of a bot id (widened from earlier versions). Shared by
/// `container_for` and `config_volume_for` so a bot id can never inject a docker
/// flag through either. Matches the sanitiser in `crates/server/src/sandbox.rs:82-89`.
fn sanitize_for_docker(id: &str) -> String {
    id.chars()
        .map(|c| match c {
            'a'..='z' | 'A'..='Z' | '0'..='9' | '_' | '.' | '-' => c,
            _ => '_',
        })
        .collect::<String>()
}

/// The next free slot.
///
/// Lowest free rather than highest used, so a bot deleted at slot 3 hands
/// that slot back instead of marching the range upwards until it runs out.
/// Returns None when every slot is taken, which the caller reports rather than
/// papering over: a VM on a port another VM already owns would answer for the
/// wrong bot, which is the worst failure this file could have.
pub fn next_slot(db: &Db, cfg: &VmConfig) -> rusqlite::Result<Option<i32>> {
    // Get all used CDP ports
    let mut stmt = db.conn().prepare("SELECT cdp_port FROM vms")?;
    let used: std::collections::HashSet<i32> = stmt
        .query_map([], |row| row.get(0))?
        .filter_map(|r| r.ok())
        .collect();

    for slot in 0..cfg.slots {
        if !used.contains(&(cfg.cdp_base + slot)) {
            return Ok(Some(slot));
        }
    }
    Ok(None)
}

/* ---------------------------------------------------------- talking to docker */

#[derive(Clone, Debug)]
pub struct DockerResult {
    pub ok: bool,
    pub stdout: String,
    pub stderr: String,
}

#[derive(Clone, Debug)]
pub struct ContainerState {
    pub exists: bool,
    pub running: bool,
    /// docker's own word: created, running, paused, restarting, removing, exited, dead.
    pub status: String,
}

/// Reads `docker inspect -f "{{.State.Status}} {{.State.Running}}"`.
///
/// Its own function, tested against output CAPTURED FROM MERIDIAN, because
/// the last parser in this repo that was only ever tested against a fake read
/// `split(/s+/)` - the backslash eaten by a heredoc - and reported every
/// finished job as "its container disappeared" behind 1014 green tests. Three
/// real captures are in test/fixtures/docker-inspect-*.txt: a running
/// container, a stopped one, and a name that does not exist.
///
/// "does not exist" MUST NOT read as "stopped". A stopped VM is started
/// again; a missing one is created. Getting that backwards means `docker start`
/// on a name docker has never heard of, forever, with the panel saying
/// "starting".
pub fn parse_container_state(result: &DockerResult) -> ContainerState {
    let text = format!("{} {}", result.stdout, result.stderr).to_lowercase();
    if text.contains("no such object") || text.contains("no such container") {
        return ContainerState {
            exists: false,
            running: false,
            status: "absent".to_string(),
        };
    }

    let parts: Vec<&str> = result.stdout.split_whitespace().collect();
    if parts.is_empty() {
        // Docker answered something this does not understand. Reporting it as
        // absent would have the caller create a SECOND container on the same name.
        return ContainerState {
            exists: false,
            running: false,
            status: "unknown".to_string(),
        };
    }

    let status = parts[0].to_string();
    let running = parts.get(1).map(|s| *s == "true").unwrap_or(false);

    ContainerState {
        exists: true,
        running,
        status,
    }
}
