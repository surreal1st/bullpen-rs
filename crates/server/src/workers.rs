//! Remote docker workers: a second daemon (a `tcp://` or `ssh://` host, over
//! the tailnet) a bot's `shell`/`run_in_background`/`spawn_helper` calls can
//! be routed to instead of meridian, chosen per bot via `bots.worker` (NULL
//! means meridian). Ported from TypeScript `src/server/workers.ts` (838
//! lines).
//!
//! S6-03 scope: the worker registry (CRUD over a JSON blob in `settings`,
//! same shape as the TS `readAll`/`writeAll`), the self-creating
//! `bots.worker` column, the pure routing decision (`resolve_sandbox_source`),
//! `docker version` parsing, and `WorkerSandbox` - the lease/claim/timeout/
//! reclaim lifecycle for a job queued while its worker is asleep.
//!
//! 🔴 **Concurrency is the point of `WorkerSandbox`.** TS's `queued` Map
//! needs no guard at all - node is single-threaded, so "check if a job is
//! already queued" and "mark it queued" can never be interleaved by another
//! call. A tokio multi-thread runtime gives two real OS threads a genuine
//! window between those two steps, so `ClaimRegistry::try_claim` below makes
//! them ONE critical section under a single `std::sync::Mutex`, never split
//! across an `.await`. Removing that atomicity is this ticket's bite (see
//! `tests/workers.rs`).
//!
//! **Judgment call, scoped out on purpose:** TS's `realWorkerDockerRun` /
//! `ensureCertFiles` / `certDirCache` (writing a worker's CA/cert/key to
//! disk once per process, then shelling out to the real `docker`/`ssh` CLI)
//! is real filesystem + process IO, not lease/claim/timeout/reclaim logic,
//! and per S6's own header no ticket in this slice may claim a socket
//! actually worked. `WorkerSandbox` here takes an already-resolved
//! `Arc<dyn DockerRun>` (the same shape `sandbox::DockerSandbox` takes an
//! already-resolved `Arc<dyn CommandRunner>`) rather than re-resolving one
//! per call the way TS's `resolveDocker()` does; wiring a real
//! `DockerRun` impl that writes cert files and shells out is left to
//! whichever ticket wires a `Worker` row to a live daemon.

use rusqlite::{OptionalExtension, params};
use serde_json::{Map, Value, json};
use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use store::Db;

/* --------------------------------------------------------------- the row */

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WorkerKind {
    Tcp,
    Ssh,
}

impl WorkerKind {
    fn as_str(self) -> &'static str {
        match self {
            WorkerKind::Tcp => "tcp",
            WorkerKind::Ssh => "ssh",
        }
    }

    fn from_value(v: Option<&Value>) -> WorkerKind {
        if v.and_then(Value::as_str) == Some("ssh") {
            WorkerKind::Ssh
        } else {
            WorkerKind::Tcp
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WorkerState {
    Unknown,
    Online,
    Asleep,
}

impl WorkerState {
    fn as_str(self) -> &'static str {
        match self {
            WorkerState::Unknown => "unknown",
            WorkerState::Online => "online",
            WorkerState::Asleep => "asleep",
        }
    }

    fn from_value(v: Option<&Value>) -> WorkerState {
        match v.and_then(Value::as_str) {
            Some("online") => WorkerState::Online,
            Some("asleep") => WorkerState::Asleep,
            _ => WorkerState::Unknown,
        }
    }
}

#[derive(Clone, PartialEq)]
pub struct Worker {
    pub id: String,
    pub label: String,
    pub kind: WorkerKind,
    /// tcp only.
    pub host: String,
    pub port: u32,
    pub ca_encrypted: String,
    pub cert_encrypted: String,
    pub key_encrypted: String,
    /// ssh only.
    pub ssh_user: String,
    pub ssh_host: String,
    pub last_state: WorkerState,
    pub last_checked_at: Option<String>,
    pub last_os: Option<String>,
    pub last_arch: Option<String>,
    pub last_version: Option<String>,
    pub last_error: Option<String>,
}

/// Hand-written so `ca_encrypted`/`cert_encrypted`/`key_encrypted` (S8b-05,
/// F13) never reach a `{:?}` - this is "the row" the doc comment on
/// `WorkerInput` below points at: same material, encrypted at rest rather
/// than plaintext, but logging ciphertext is still handing an attacker a
/// fixed target and this file has no legitimate reason to ever print it.
/// Every other field is ordinary metadata and prints normally.
impl std::fmt::Debug for Worker {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Worker")
            .field("id", &self.id)
            .field("label", &self.label)
            .field("kind", &self.kind)
            .field("host", &self.host)
            .field("port", &self.port)
            .field("ca_encrypted", &"[redacted]")
            .field("cert_encrypted", &"[redacted]")
            .field("key_encrypted", &"[redacted]")
            .field("ssh_user", &self.ssh_user)
            .field("ssh_host", &self.ssh_host)
            .field("last_state", &self.last_state)
            .field("last_checked_at", &self.last_checked_at)
            .field("last_os", &self.last_os)
            .field("last_arch", &self.last_arch)
            .field("last_version", &self.last_version)
            .field("last_error", &self.last_error)
            .finish()
    }
}

/// What the client ever sees. No cert, key or CA field exists here at all.
#[derive(Debug, Clone, PartialEq)]
pub struct WorkerPublic {
    pub id: String,
    pub label: String,
    pub kind: WorkerKind,
    pub host: String,
    pub port: u32,
    pub has_cert: bool,
    pub ssh_user: String,
    pub ssh_host: String,
    pub last_state: WorkerState,
    pub last_checked_at: Option<String>,
    pub last_os: Option<String>,
    pub last_arch: Option<String>,
    pub last_version: Option<String>,
    pub last_error: Option<String>,
}

fn to_public(w: &Worker) -> WorkerPublic {
    WorkerPublic {
        id: w.id.clone(),
        label: w.label.clone(),
        kind: w.kind,
        host: w.host.clone(),
        port: w.port,
        has_cert: !w.ca_encrypted.is_empty()
            && !w.cert_encrypted.is_empty()
            && !w.key_encrypted.is_empty(),
        ssh_user: w.ssh_user.clone(),
        ssh_host: w.ssh_host.clone(),
        last_state: w.last_state,
        last_checked_at: w.last_checked_at.clone(),
        last_os: w.last_os.clone(),
        last_arch: w.last_arch.clone(),
        last_version: w.last_version.clone(),
        last_error: w.last_error.clone(),
    }
}

const WORKERS_KEY: &str = "workers.list";

fn str_field(obj: &Map<String, Value>, key: &str) -> String {
    obj.get(key)
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string()
}

fn opt_str_field(obj: &Map<String, Value>, key: &str) -> Option<String> {
    obj.get(key).and_then(Value::as_str).map(str::to_string)
}

/// Defensive: a row written by a future shape, or hand-edited, reads as
/// skipped rather than thrown - same contract as TS's `sanitizeStored`.
fn sanitize_stored(raw: &Value) -> Option<Worker> {
    let obj = raw.as_object()?;
    let id = str_field(obj, "id");
    let label = str_field(obj, "label");
    if id.is_empty() || label.is_empty() {
        return None;
    }
    let port = obj
        .get("port")
        .and_then(Value::as_f64)
        .filter(|p| p.is_finite())
        .map(|p| p as u32)
        .unwrap_or(2376);
    Some(Worker {
        id,
        label,
        kind: WorkerKind::from_value(obj.get("kind")),
        host: str_field(obj, "host"),
        port,
        ca_encrypted: str_field(obj, "caEncrypted"),
        cert_encrypted: str_field(obj, "certEncrypted"),
        key_encrypted: str_field(obj, "keyEncrypted"),
        ssh_user: str_field(obj, "sshUser"),
        ssh_host: str_field(obj, "sshHost"),
        last_state: WorkerState::from_value(obj.get("lastState")),
        last_checked_at: opt_str_field(obj, "lastCheckedAt"),
        last_os: opt_str_field(obj, "lastOs"),
        last_arch: opt_str_field(obj, "lastArch"),
        last_version: opt_str_field(obj, "lastVersion"),
        last_error: opt_str_field(obj, "lastError"),
    })
}

fn worker_to_value(w: &Worker) -> Value {
    json!({
        "id": w.id,
        "label": w.label,
        "kind": w.kind.as_str(),
        "host": w.host,
        "port": w.port,
        "caEncrypted": w.ca_encrypted,
        "certEncrypted": w.cert_encrypted,
        "keyEncrypted": w.key_encrypted,
        "sshUser": w.ssh_user,
        "sshHost": w.ssh_host,
        "lastState": w.last_state.as_str(),
        "lastCheckedAt": w.last_checked_at,
        "lastOs": w.last_os,
        "lastArch": w.last_arch,
        "lastVersion": w.last_version,
        "lastError": w.last_error,
    })
}

fn read_all(db: &Db) -> Vec<Worker> {
    let Ok(Some(raw)) = db.settings_get(WORKERS_KEY) else {
        return Vec::new();
    };
    let Ok(Value::Array(items)) = serde_json::from_str::<Value>(&raw) else {
        return Vec::new();
    };
    items.iter().filter_map(sanitize_stored).collect()
}

fn write_all(db: &Db, workers: &[Worker]) -> rusqlite::Result<()> {
    let values: Vec<Value> = workers.iter().map(worker_to_value).collect();
    let json = serde_json::to_string(&values).unwrap_or_else(|_| "[]".to_string());
    db.settings_set(WORKERS_KEY, &json)
}

pub fn list_workers(db: &Db) -> Vec<WorkerPublic> {
    read_all(db).iter().map(to_public).collect()
}

/// JSON shape the settings UI and iOS client expect (`WorkerPublic` in TS).
pub fn worker_public_to_value(w: &WorkerPublic) -> Value {
    json!({
        "id": w.id,
        "label": w.label,
        "kind": w.kind.as_str(),
        "host": w.host,
        "port": w.port,
        "hasCert": w.has_cert,
        "sshUser": w.ssh_user,
        "sshHost": w.ssh_host,
        "lastState": w.last_state.as_str(),
        "lastCheckedAt": w.last_checked_at,
        "lastOs": w.last_os,
        "lastArch": w.last_arch,
        "lastVersion": w.last_version,
        "lastError": w.last_error,
    })
}

/// Internal only - carries the encrypted secrets. Never returned from a route.
pub fn get_worker(db: &Db, id: &str) -> Option<Worker> {
    read_all(db).into_iter().find(|w| w.id == id)
}

#[derive(Clone, Default)]
pub struct WorkerInput {
    pub id: Option<String>,
    pub label: String,
    pub kind: WorkerKind,
    pub host: Option<String>,
    pub port: Option<u32>,
    /// Plaintext PEM, from the settings form. Blank/absent on an update keeps
    /// whatever was saved before.
    pub ca: Option<String>,
    pub cert: Option<String>,
    pub key: Option<String>,
    pub ssh_user: Option<String>,
    pub ssh_host: Option<String>,
}

/// Hand-written (S8b-05, F13): a derived `Debug` here would write a
/// plaintext PEM private key wherever `{:?}` output goes - `RUST_LOG=debug`
/// in `deploy/bullpen-rs.service` since 2026-09-16 means that is journald,
/// not nowhere. `ca`/`cert`/`key` always print the same fixed placeholder
/// regardless of whether they are `Some` or `None`, so the output reveals
/// neither the secret, its length, its prefix, nor even whether a cert is
/// present at all. Every other field is non-secret and prints normally.
impl std::fmt::Debug for WorkerInput {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("WorkerInput")
            .field("id", &self.id)
            .field("label", &self.label)
            .field("kind", &self.kind)
            .field("host", &self.host)
            .field("port", &self.port)
            .field("ca", &"[redacted]")
            .field("cert", &"[redacted]")
            .field("key", &"[redacted]")
            .field("ssh_user", &self.ssh_user)
            .field("ssh_host", &self.ssh_host)
            .finish()
    }
}

/// `WorkerInput` derives `Default` for tests that only care about a couple
/// of fields (see `tests` below); "anything but ssh" already means tcp
/// everywhere else in this file (`WorkerKind::from_value`), so `Tcp` is the
/// only sensible default here too.
impl Default for WorkerKind {
    fn default() -> Self {
        WorkerKind::Tcp
    }
}

pub fn parse_worker_input(body: &Value) -> WorkerInput {
    let empty = Map::new();
    let obj = body.as_object().unwrap_or(&empty);
    WorkerInput {
        id: opt_str_field(obj, "id"),
        label: str_field(obj, "label"),
        kind: WorkerKind::from_value(obj.get("kind")),
        host: opt_str_field(obj, "host"),
        port: obj.get("port").and_then(Value::as_f64).map(|p| p as u32),
        ca: opt_str_field(obj, "ca"),
        cert: opt_str_field(obj, "cert"),
        key: opt_str_field(obj, "key"),
        ssh_user: opt_str_field(obj, "sshUser"),
        ssh_host: opt_str_field(obj, "sshHost"),
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct UpsertResult {
    pub ok: bool,
    pub worker: Option<WorkerPublic>,
    pub error: Option<String>,
}

pub fn upsert_worker(db: &Db, input: &WorkerInput) -> UpsertResult {
    let label = input.label.trim().to_string();
    if label.is_empty() {
        return UpsertResult {
            ok: false,
            worker: None,
            error: Some("A label is required.".to_string()),
        };
    }

    let kind = input.kind;
    let mut all = read_all(db);
    let existing_index = input
        .id
        .as_ref()
        .and_then(|id| all.iter().position(|w| &w.id == id));
    let existing = existing_index.map(|i| all[i].clone());
    let id = input
        .id
        .clone()
        .or_else(|| existing.as_ref().map(|w| w.id.clone()))
        .unwrap_or_else(|| uuid::Uuid::new_v4().to_string());

    let mut host = String::new();
    let mut port = 2376u32;
    let mut ca_encrypted = existing
        .as_ref()
        .map(|w| w.ca_encrypted.clone())
        .unwrap_or_default();
    let mut cert_encrypted = existing
        .as_ref()
        .map(|w| w.cert_encrypted.clone())
        .unwrap_or_default();
    let mut key_encrypted = existing
        .as_ref()
        .map(|w| w.key_encrypted.clone())
        .unwrap_or_default();
    let mut ssh_user = String::new();
    let mut ssh_host = String::new();

    if matches!(kind, WorkerKind::Tcp) {
        host = input
            .host
            .clone()
            .or_else(|| existing.as_ref().map(|w| w.host.clone()))
            .unwrap_or_default()
            .trim()
            .to_string();
        port = input
            .port
            .or(existing.as_ref().map(|w| w.port))
            .unwrap_or(2376);
        if host.is_empty() {
            return UpsertResult {
                ok: false,
                worker: None,
                error: Some("A host is required for a tcp worker.".to_string()),
            };
        }
        if let Some(ca) = input
            .ca
            .as_ref()
            .map(|s| s.trim())
            .filter(|s| !s.is_empty())
            && let Ok(enc) = crate::settings_secrets::encrypt_for_storage(db, ca)
        {
            ca_encrypted = enc;
        }
        if let Some(cert) = input
            .cert
            .as_ref()
            .map(|s| s.trim())
            .filter(|s| !s.is_empty())
            && let Ok(enc) = crate::settings_secrets::encrypt_for_storage(db, cert)
        {
            cert_encrypted = enc;
        }
        if let Some(key) = input
            .key
            .as_ref()
            .map(|s| s.trim())
            .filter(|s| !s.is_empty())
            && let Ok(enc) = crate::settings_secrets::encrypt_for_storage(db, key)
        {
            key_encrypted = enc;
        }
        if ca_encrypted.is_empty() || cert_encrypted.is_empty() || key_encrypted.is_empty() {
            return UpsertResult {
                ok: false,
                worker: None,
                error: Some(
                    "A tcp worker needs a CA certificate, a client certificate and a client key."
                        .to_string(),
                ),
            };
        }
    } else {
        ssh_user = input
            .ssh_user
            .clone()
            .or_else(|| existing.as_ref().map(|w| w.ssh_user.clone()))
            .unwrap_or_default()
            .trim()
            .to_string();
        ssh_host = input
            .ssh_host
            .clone()
            .or_else(|| existing.as_ref().map(|w| w.ssh_host.clone()))
            .unwrap_or_default()
            .trim()
            .to_string();
        if ssh_user.is_empty() || ssh_host.is_empty() {
            return UpsertResult {
                ok: false,
                worker: None,
                error: Some("An ssh worker needs a user and a host.".to_string()),
            };
        }
        // No cert/key for ssh - the server's own key is used, same as the `ssh` tool.
        ca_encrypted = String::new();
        cert_encrypted = String::new();
        key_encrypted = String::new();
    }

    let worker = Worker {
        id,
        label,
        kind,
        host,
        port,
        ca_encrypted,
        cert_encrypted,
        key_encrypted,
        ssh_user,
        ssh_host,
        last_state: existing
            .as_ref()
            .map(|w| w.last_state)
            .unwrap_or(WorkerState::Unknown),
        last_checked_at: existing.as_ref().and_then(|w| w.last_checked_at.clone()),
        last_os: existing.as_ref().and_then(|w| w.last_os.clone()),
        last_arch: existing.as_ref().and_then(|w| w.last_arch.clone()),
        last_version: existing.as_ref().and_then(|w| w.last_version.clone()),
        last_error: existing.as_ref().and_then(|w| w.last_error.clone()),
    };
    match existing_index {
        Some(i) => all[i] = worker.clone(),
        None => all.push(worker.clone()),
    }
    if let Err(err) = write_all(db, &all) {
        return UpsertResult {
            ok: false,
            worker: None,
            error: Some(format!("Failed to save worker: {err}")),
        };
    }
    UpsertResult {
        ok: true,
        worker: Some(to_public(&worker)),
        error: None,
    }
}

pub fn remove_worker(db: &Db, id: &str) -> Vec<WorkerPublic> {
    let all: Vec<Worker> = read_all(db).into_iter().filter(|w| w.id != id).collect();
    let _ = write_all(db, &all);
    all.iter().map(to_public).collect()
}

pub struct TestResult {
    pub state: WorkerState,
    pub os: Option<String>,
    pub arch: Option<String>,
    pub version: Option<String>,
    pub error: Option<String>,
    pub checked_at: String,
}

fn persist_test_result(db: &Db, worker: &Worker, result: TestResult) -> WorkerPublic {
    let mut all = read_all(db);
    let Some(index) = all.iter().position(|w| w.id == worker.id) else {
        return to_public(worker);
    };
    all[index].last_state = result.state;
    all[index].last_checked_at = Some(result.checked_at);
    all[index].last_os = result.os;
    all[index].last_arch = result.arch;
    all[index].last_version = result.version;
    all[index].last_error = result.error;
    let updated = all[index].clone();
    let _ = write_all(db, &all);
    to_public(&updated)
}

/// `docker version` through this worker's daemon, and the row it leaves
/// behind. Mirrors TS `testWorker`; `docker: None` is TS's `docker === null`
/// branch (a tcp worker with no client certificate saved yet - a
/// configuration gap, not a network failure). Which `DockerRun` to build for
/// a given `Worker` is the same out-of-scope judgment call this file's
/// header explains for `WorkerSandbox`.
/// HTTP `POST /api/workers/:id/test` when real `DockerRun` is not wired yet.
pub fn test_worker_without_docker(
    db: &Db,
    id: &str,
    checked_at: impl Into<String>,
) -> Option<WorkerPublic> {
    let worker = get_worker(db, id)?;
    let checked_at = checked_at.into();
    Some(persist_test_result(
        db,
        &worker,
        TestResult {
            state: WorkerState::Asleep,
            os: None,
            arch: None,
            version: None,
            error: Some("No client certificate is saved for this worker yet.".to_string()),
            checked_at,
        },
    ))
}

pub async fn test_worker(
    db: &Db,
    id: &str,
    docker: Option<&dyn DockerRun>,
    checked_at: impl Into<String>,
) -> Option<WorkerPublic> {
    let worker = get_worker(db, id)?;
    let checked_at = checked_at.into();
    let Some(docker) = docker else {
        return test_worker_without_docker(db, id, checked_at);
    };
    let result = docker
        .run(
            vec![
                "version".to_string(),
                "--format".to_string(),
                "{{json .}}".to_string(),
            ],
            8_000,
        )
        .await;
    let info = parse_docker_version(result.ok, &result.stdout, &result.stderr);
    Some(persist_test_result(
        db,
        &worker,
        TestResult {
            state: if info.ok {
                WorkerState::Online
            } else {
                WorkerState::Asleep
            },
            os: info.os,
            arch: info.arch,
            version: info.version,
            error: info.error,
            checked_at,
        },
    ))
}

/* -------------------------------------------------- the self-creating column */

/// The self-creating `bots.worker` column. NULL means meridian. Same
/// `PRAGMA table_info` + conditional `ALTER TABLE` pattern every other
/// self-creating column in this codebase uses (`store::Db::ensure_column`,
/// `routines::ensure_routine_columns`) - never a numbered migration.
pub fn ensure_bot_worker_column(db: &Db) -> rusqlite::Result<()> {
    let exists = {
        let mut stmt = db.conn().prepare("PRAGMA table_info(bots)")?;
        stmt.query_map([], |row| row.get::<_, String>(1))?
            .filter_map(Result::ok)
            .any(|name| name == "worker")
    };
    if !exists {
        db.conn()
            .execute_batch("ALTER TABLE bots ADD COLUMN worker TEXT")?;
    }
    Ok(())
}

pub fn get_bot_worker_id(db: &Db, bot_id: &str) -> rusqlite::Result<Option<String>> {
    let raw: Option<String> = db
        .conn()
        .query_row(
            "SELECT worker FROM bots WHERE id = ?1",
            params![bot_id],
            |row| row.get(0),
        )
        .optional()?
        .flatten();
    Ok(raw.filter(|s| !s.trim().is_empty()))
}

pub fn set_bot_worker_id(
    db: &Db,
    bot_id: &str,
    worker_id: Option<&str>,
) -> rusqlite::Result<Option<String>> {
    let value = worker_id.map(str::trim).filter(|s| !s.is_empty());
    db.conn().execute(
        "UPDATE bots SET worker = ?1 WHERE id = ?2",
        params![value, bot_id],
    )?;
    get_bot_worker_id(db, bot_id)
}

/* -------------------------------------------------------- routing decision */

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SandboxSource {
    Meridian,
    Worker,
    Missing { worker_id: String },
}

/// Which daemon a bot's `shell`/`run_in_background`/`spawn_helper` calls
/// should run against - pure, so "a deleted worker never silently falls back
/// to meridian" is a fact this proves without a database, a docker daemon,
/// or a network. The caller looks the worker up itself (a real row, with its
/// secrets - this function never sees or needs them) and hands in only
/// whether that lookup found anything.
pub fn resolve_sandbox_source(bot_worker_id: Option<&str>, worker_exists: bool) -> SandboxSource {
    match bot_worker_id {
        None => SandboxSource::Meridian,
        Some(id) => {
            if worker_exists {
                SandboxSource::Worker
            } else {
                SandboxSource::Missing {
                    worker_id: id.to_string(),
                }
            }
        }
    }
}

/* ------------------------------------------------------------ docker version */

#[derive(Debug, Clone, PartialEq)]
pub struct DockerVersionInfo {
    pub ok: bool,
    pub os: Option<String>,
    pub arch: Option<String>,
    pub version: Option<String>,
    pub error: Option<String>,
}

fn first_line(text: &str) -> String {
    text.trim().lines().next().unwrap_or("").trim().to_string()
}

/// Reads `docker version --format '{{json .}}'` output. `Server` missing
/// entirely (the CLI answered but no daemon did - what an asleep or
/// unreachable host looks like) is checked explicitly and must NEVER read as
/// online, same as the TS original's own comment on this function.
pub fn parse_docker_version(ok: bool, stdout: &str, stderr: &str) -> DockerVersionInfo {
    if !ok {
        let err = first_line(stderr);
        return DockerVersionInfo {
            ok: false,
            os: None,
            arch: None,
            version: None,
            error: Some(if err.is_empty() {
                "docker did not answer".to_string()
            } else {
                err
            }),
        };
    }

    let parsed: Value = match serde_json::from_str(stdout) {
        Ok(v) => v,
        Err(_) => {
            return DockerVersionInfo {
                ok: false,
                os: None,
                arch: None,
                version: None,
                error: Some("docker version did not return readable JSON".to_string()),
            };
        }
    };

    let server = parsed.get("Server");
    let server_obj = server.and_then(Value::as_object);
    match server_obj {
        None => DockerVersionInfo {
            ok: false,
            os: None,
            arch: None,
            version: None,
            error: Some("docker answered with no server - the daemon is not reachable".to_string()),
        },
        Some(server) => DockerVersionInfo {
            ok: true,
            os: server.get("Os").and_then(Value::as_str).map(str::to_string),
            arch: server
                .get("Arch")
                .and_then(Value::as_str)
                .map(str::to_string),
            version: server
                .get("Version")
                .and_then(Value::as_str)
                .map(str::to_string),
            error: None,
        },
    }
}

/* ------------------------------------------------------- reaching the daemon */

/// Single-quotes a value for a POSIX shell, escaping any embedded single
/// quote. Same rule as `repo.ts`'s `shQuote`.
pub fn sh_quote(value: &str) -> String {
    format!("'{}'", value.replace('\'', r"'\''"))
}

/// The env a `tcp://` worker's docker CLI call needs: TLS on, pointed at a
/// directory holding `ca.pem`/`cert.pem`/`key.pem`.
pub fn tcp_docker_env(host: &str, port: u32, cert_dir: &str) -> Vec<(String, String)> {
    vec![
        ("DOCKER_HOST".to_string(), format!("tcp://{host}:{port}")),
        ("DOCKER_TLS_VERIFY".to_string(), "1".to_string()),
        ("DOCKER_CERT_PATH".to_string(), cert_dir.to_string()),
    ]
}

/// The argv for `execFile("ssh", ...)` that runs one docker command on an
/// `ssh://` worker. The docker command is built as ONE quoted string (not
/// separate ssh argv entries) because ssh joins its trailing arguments with a
/// bare space before handing them to the remote shell - each token has to be
/// quoted BEFORE ssh ever sees it, not after.
pub fn ssh_argv(
    ssh_user: &str,
    ssh_host: &str,
    identity: &str,
    docker_args: &[String],
) -> Vec<String> {
    let quoted = docker_args
        .iter()
        .map(|a| sh_quote(a))
        .collect::<Vec<_>>()
        .join(" ");
    vec![
        "-i".to_string(),
        identity.to_string(),
        "-o".to_string(),
        "IdentitiesOnly=yes".to_string(),
        "-o".to_string(),
        "StrictHostKeyChecking=accept-new".to_string(),
        "-o".to_string(),
        "BatchMode=yes".to_string(),
        "-o".to_string(),
        "ConnectTimeout=8".to_string(),
        format!("{ssh_user}@{ssh_host}"),
        format!("docker {quoted}"),
    ]
}

/// The result of one docker CLI invocation against a worker's daemon.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct DockerResult {
    pub ok: bool,
    pub stdout: String,
    pub stderr: String,
}

/// One worker's docker daemon, reached however its `kind` dictates (`tcp://`
/// with client certs, or `ssh://` through the server's own key). The ONLY
/// seam `WorkerSandbox` touches a process or a socket through - every test
/// injects a fake instead. Mirrors `sandbox::CommandRunner`'s role for
/// `DockerSandbox`.
#[async_trait::async_trait]
pub trait DockerRun: Send + Sync {
    async fn run(&self, args: Vec<String>, timeout_ms: u64) -> DockerResult;
}

/* --------------------------------------------------------------- job events */

/// Stand-in for the two `jobs.ts` calls `workers.ts`'s `workerSandbox` makes
/// (`appendJobOutput`, `finishJob(..., "failed", ..., null)`) - `jobs.rs`
/// does not exist in this crate yet, and porting its full signature is out
/// of this ticket's owned files. Scoped to exactly the two shapes this file
/// calls, so a future `jobs.rs` port can implement this trait for the real
/// thing without `WorkerSandbox` itself changing.
pub trait JobEvents: Send + Sync {
    fn append_output(&self, job_id: &str, text: &str);
    fn finish_failed(&self, job_id: &str, message: &str);
}

/* -------------------------------------------------------------- the Sandbox */

const RETRY_EVERY_MS: u64 = 5 * 60_000;
pub const WORKER_WAKE_TIMEOUT_MS: u64 = 12 * 60 * 60_000;
const JOB_PREFIX: &str = "bullpen-job-";

// `sandbox.ts`'s `DEFAULT_SANDBOX`, the fields `workerSandbox` actually
// reads (network/proxy/proxyHost are never read there - every worker run
// is unconditionally `--network none`, see the doc on `docker_args` below).
const DEFAULT_IMAGE: &str = "alpine:3.20";
const DEFAULT_MEMORY: &str = "256m";
const DEFAULT_CPUS: &str = "0.5";
const DEFAULT_PIDS_LIMIT: u32 = 128;
const DEFAULT_TIMEOUT_MS: u64 = 30_000;
const DEFAULT_MAX_OUTPUT_BYTES: usize = 32_000;

fn sanitize_docker_name(id: &str) -> String {
    id.chars()
        .map(|c| match c {
            'a'..='z' | 'A'..='Z' | '0'..='9' | '_' | '.' | '-' => c,
            _ => '_',
        })
        .collect()
}

/// One volume per bot. The only writable thing a sandbox can see.
fn volume_for(bot_id: &str) -> String {
    format!("bullpen-bot-{}", sanitize_docker_name(bot_id))
}

/// The container name for a job. Docker names allow a narrow character set.
fn container_for(job_id: &str) -> String {
    format!("{JOB_PREFIX}{}", sanitize_docker_name(job_id))
}

/// Where a background command's output lands inside the bot's own volume.
fn job_log_path(job_id: &str) -> String {
    format!("/work/.jobs/{}.log", sanitize_docker_name(job_id))
}

/// The inverse of `container_for` - safe because a job id is a UUID,
/// untouched by that function's sanitizer.
fn job_id_from_handle(handle: &str) -> Option<&str> {
    handle.strip_prefix(JOB_PREFIX)
}

#[derive(Debug, Clone, PartialEq)]
pub struct ExecResult {
    pub stdout: String,
    pub stderr: String,
    pub exit_code: i32,
    pub timed_out: bool,
    pub truncated: bool,
}

#[derive(Debug, Clone, PartialEq)]
pub struct SpawnResult {
    pub ok: bool,
    /// The container's name, which is how it is polled and stopped.
    pub handle: String,
    pub detail: String,
}

#[derive(Debug, Clone, PartialEq)]
pub struct ProbeResult {
    pub running: bool,
    pub exit_code: Option<i64>,
    pub detail: String,
}

/// Cuts `text` to at most `max` bytes without splitting a multi-byte UTF-8
/// character - a byte-index slice panics when `max` lands mid-char.
fn truncate_char_boundary(text: &str, max: usize) -> String {
    if text.len() <= max {
        return text.to_string();
    }
    let end = (0..=max)
        .rev()
        .find(|i| text.is_char_boundary(*i))
        .unwrap_or(0);
    text[..end].to_string()
}

fn cap_exec(result: &DockerResult, exit_code: i32, timed_out: bool, max: usize) -> ExecResult {
    ExecResult {
        stdout: truncate_char_boundary(&result.stdout, max),
        stderr: truncate_char_boundary(&result.stderr, max),
        exit_code,
        timed_out,
        truncated: result.stdout.len() > max || result.stderr.len() > max,
    }
}

fn unreachable(message: &str) -> DockerResult {
    DockerResult {
        ok: false,
        stdout: String::new(),
        stderr: message.to_string(),
    }
}

fn parse_probe(stdout: &str) -> ProbeResult {
    let text = stdout.trim();
    let mut parts = text.split_whitespace();
    let running_text = parts.next();
    let code_text = parts.next();
    let running = running_text == Some("true");
    let parsed_code = code_text.and_then(|c| c.parse::<i64>().ok());
    ProbeResult {
        running,
        exit_code: if running { None } else { parsed_code },
        detail: text.to_string(),
    }
}

/// Guards `WorkerSandbox`'s queued-job wait loop: at most one background
/// `wait_then_spawn` task may ever run for a given job id. This is the
/// "claim guard" this ticket's bite removes. `try_claim`'s check-then-insert
/// is ONE critical section under a single lock - the correctness property a
/// naive two-step "check, then separately insert" port would lose under
/// real OS-thread concurrency (TS never has to worry about this at all).
#[derive(Default)]
struct ClaimRegistry {
    claims: Mutex<HashMap<String, Arc<ClaimState>>>,
}

struct ClaimState {
    cancelled: AtomicBool,
}

impl ClaimRegistry {
    /// Attempts to claim `job_id` for a new wait loop. `Some` means this
    /// call is the WINNER - the only caller allowed to start a
    /// `wait_then_spawn` task. `None` means someone else already holds the
    /// claim; the job is already queued, so this caller must NOT start a
    /// second loop.
    fn try_claim(&self, job_id: &str) -> Option<Arc<ClaimState>> {
        let mut claims = self.claims.lock().unwrap_or_else(|p| p.into_inner());
        if claims.contains_key(job_id) {
            return None;
        }
        let state = Arc::new(ClaimState {
            cancelled: AtomicBool::new(false),
        });
        claims.insert(job_id.to_string(), Arc::clone(&state));
        Some(state)
    }

    fn get(&self, job_id: &str) -> Option<Arc<ClaimState>> {
        self.claims
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .get(job_id)
            .cloned()
    }

    fn release(&self, job_id: &str) {
        self.claims
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .remove(job_id);
    }
}

/// Test/production seam for `WorkerSandbox`'s own clock and sleep - so the
/// wait-then-spawn loop's timeout and retry cadence are testable without a
/// real 12-hour wait. Mirrors TS's `deps.now`/`deps.sleep`.
#[async_trait::async_trait]
pub trait Clock: Send + Sync {
    fn now_ms(&self) -> u64;
    async fn sleep(&self, ms: u64);
}

pub struct RealClock;

#[async_trait::async_trait]
impl Clock for RealClock {
    fn now_ms(&self) -> u64 {
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_millis() as u64)
            .unwrap_or(0)
    }

    async fn sleep(&self, ms: u64) {
        tokio::time::sleep(std::time::Duration::from_millis(ms)).await;
    }
}

struct Inner {
    worker_id: String,
    worker_label: String,
    docker: Arc<dyn DockerRun>,
    jobs: Arc<dyn JobEvents>,
    clock: Arc<dyn Clock>,
    retry_every_ms: u64,
    max_wait_ms: u64,
    claims: ClaimRegistry,
}

/// One bot's `shell`/`run_in_background` traffic, aimed at a worker instead
/// of meridian - a second sandbox implementation built around an injected
/// `DockerRun`, the way `vm.ts`'s `ensureVm` is built around one.
///
/// 🔴 No network join at all - `--network none` on every run,
/// unconditionally. S0's per-bot egress proxy is meridian-local
/// infrastructure; extending it to a remote daemon is out of scope here, so
/// a worker-routed sandbox has exactly the pre-S0 property: no network,
/// full stop.
#[derive(Clone)]
pub struct WorkerSandbox {
    inner: Arc<Inner>,
}

impl WorkerSandbox {
    pub fn new(
        worker_id: impl Into<String>,
        worker_label: impl Into<String>,
        docker: Arc<dyn DockerRun>,
        jobs: Arc<dyn JobEvents>,
    ) -> Self {
        Self::with_deps(
            worker_id,
            worker_label,
            docker,
            jobs,
            Arc::new(RealClock),
            RETRY_EVERY_MS,
            WORKER_WAKE_TIMEOUT_MS,
        )
    }

    pub fn with_deps(
        worker_id: impl Into<String>,
        worker_label: impl Into<String>,
        docker: Arc<dyn DockerRun>,
        jobs: Arc<dyn JobEvents>,
        clock: Arc<dyn Clock>,
        retry_every_ms: u64,
        max_wait_ms: u64,
    ) -> Self {
        Self {
            inner: Arc::new(Inner {
                worker_id: worker_id.into(),
                worker_label: worker_label.into(),
                docker,
                jobs,
                clock,
                retry_every_ms,
                max_wait_ms,
                claims: ClaimRegistry::default(),
            }),
        }
    }

    async fn ping(&self) -> bool {
        let result = self
            .inner
            .docker
            .run(
                vec![
                    "version".to_string(),
                    "--format".to_string(),
                    "{{json .}}".to_string(),
                ],
                8_000,
            )
            .await;
        parse_docker_version(result.ok, &result.stdout, &result.stderr).ok
    }

    pub async fn check(&self) -> (bool, String) {
        if self.ping().await {
            (true, format!("{} is online", self.inner.worker_label))
        } else {
            (
                false,
                format!("{} is asleep or unreachable", self.inner.worker_label),
            )
        }
    }

    fn exec_args(&self, volume: &str, command: &str) -> Vec<String> {
        vec![
            "run".to_string(),
            "--rm".to_string(),
            "--network".to_string(),
            "none".to_string(),
            "--memory".to_string(),
            DEFAULT_MEMORY.to_string(),
            "--memory-swap".to_string(),
            DEFAULT_MEMORY.to_string(),
            "--cpus".to_string(),
            DEFAULT_CPUS.to_string(),
            "--pids-limit".to_string(),
            DEFAULT_PIDS_LIMIT.to_string(),
            "--read-only".to_string(),
            "--tmpfs".to_string(),
            "/tmp:rw,noexec,nosuid,size=64m".to_string(),
            "--cap-drop".to_string(),
            "ALL".to_string(),
            "--security-opt".to_string(),
            "no-new-privileges".to_string(),
            "--mount".to_string(),
            format!("type=volume,source={volume},target=/work"),
            "--workdir".to_string(),
            "/work".to_string(),
            "--label".to_string(),
            format!("bullpen.worker={}", self.inner.worker_id),
            DEFAULT_IMAGE.to_string(),
            "sh".to_string(),
            "-c".to_string(),
            command.to_string(),
        ]
    }

    fn spawn_args(&self, volume: &str, handle: &str, job_id: &str, command: &str) -> Vec<String> {
        vec![
            "run".to_string(),
            "-d".to_string(),
            "--name".to_string(),
            handle.to_string(),
            "--network".to_string(),
            "none".to_string(),
            "--memory".to_string(),
            DEFAULT_MEMORY.to_string(),
            "--memory-swap".to_string(),
            DEFAULT_MEMORY.to_string(),
            "--cpus".to_string(),
            DEFAULT_CPUS.to_string(),
            "--pids-limit".to_string(),
            DEFAULT_PIDS_LIMIT.to_string(),
            "--read-only".to_string(),
            "--tmpfs".to_string(),
            "/tmp:rw,noexec,nosuid,size=64m".to_string(),
            "--cap-drop".to_string(),
            "ALL".to_string(),
            "--security-opt".to_string(),
            "no-new-privileges".to_string(),
            "--mount".to_string(),
            format!("type=volume,source={volume},target=/work"),
            "--workdir".to_string(),
            "/work".to_string(),
            "--label".to_string(),
            format!("bullpen.worker={}", self.inner.worker_id),
            "--label".to_string(),
            format!("bullpen.job={job_id}"),
            DEFAULT_IMAGE.to_string(),
            "sh".to_string(),
            "-c".to_string(),
            format!(
                "mkdir -p /work/.jobs && {{ {command} ; }} >{} 2>&1",
                job_log_path(job_id)
            ),
        ]
    }

    pub async fn exec(&self, bot_id: &str, command: &str, timeout_ms: Option<u64>) -> ExecResult {
        let timeout_ms = timeout_ms.unwrap_or(DEFAULT_TIMEOUT_MS);
        if !self.ping().await {
            return cap_exec(
                &unreachable(&format!(
                    "{} is asleep. A one-shot command cannot wait for it - use run_in_background instead, which queues and retries.",
                    self.inner.worker_label
                )),
                127,
                false,
                DEFAULT_MAX_OUTPUT_BYTES,
            );
        }
        let result = self
            .inner
            .docker
            .run(
                self.exec_args(&volume_for(bot_id), command),
                timeout_ms + 5_000,
            )
            .await;
        let exit_code = if result.ok { 0 } else { 1 };
        cap_exec(&result, exit_code, false, DEFAULT_MAX_OUTPUT_BYTES)
    }

    async fn real_spawn(
        &self,
        bot_id: &str,
        job_id: &str,
        handle: &str,
        command: &str,
    ) -> SpawnResult {
        let result = self
            .inner
            .docker
            .run(
                self.spawn_args(&volume_for(bot_id), handle, job_id, command),
                30_000,
            )
            .await;
        if result.ok {
            SpawnResult {
                ok: true,
                handle: handle.to_string(),
                detail: "started".to_string(),
            }
        } else {
            let detail = first_line(&result.stderr);
            SpawnResult {
                ok: false,
                handle: handle.to_string(),
                detail: if detail.is_empty() {
                    "failed".to_string()
                } else {
                    detail
                },
            }
        }
    }

    /// Runs while a job's worker is asleep: retries every `retry_every_ms`
    /// until the worker answers or `max_wait_ms` elapses. Only the task
    /// holding `state` (the winner of `ClaimRegistry::try_claim`) may ever
    /// run this for a given job id.
    async fn wait_then_spawn(
        &self,
        bot_id: &str,
        job_id: &str,
        handle: &str,
        command: &str,
        state: Arc<ClaimState>,
    ) {
        let deadline = self
            .inner
            .clock
            .now_ms()
            .saturating_add(self.inner.max_wait_ms);
        while self.inner.clock.now_ms() < deadline {
            self.inner.clock.sleep(self.inner.retry_every_ms).await;
            if state.cancelled.load(Ordering::SeqCst) {
                return;
            }
            if self.ping().await {
                self.inner.claims.release(job_id);
                let started = self.real_spawn(bot_id, job_id, handle, command).await;
                if !started.ok {
                    self.inner.jobs.finish_failed(
                        job_id,
                        &format!(
                            "{} woke up, but it did not start: {}",
                            self.inner.worker_label, started.detail
                        ),
                    );
                }
                return;
            }
        }
        self.inner.claims.release(job_id);
        if !state.cancelled.load(Ordering::SeqCst) {
            let hours = (self.inner.max_wait_ms as f64 / 3_600_000.0).round() as i64;
            self.inner.jobs.finish_failed(
                job_id,
                &format!(
                    "{} never woke up within {hours} hours. Nothing ran.",
                    self.inner.worker_label
                ),
            );
        }
    }

    /// Starts a command and returns immediately. If the worker is asleep,
    /// the job is queued: `try_claim` decides whether THIS call starts the
    /// background wait loop, or whether a concurrent call already owns it -
    /// this ticket's bite is removing that guard (see `tests/workers.rs`).
    pub async fn spawn(&self, bot_id: &str, job_id: &str, command: &str) -> SpawnResult {
        let handle = container_for(job_id);
        if self.ping().await {
            return self.real_spawn(bot_id, job_id, &handle, command).await;
        }

        self.inner.jobs.append_output(
            job_id,
            &format!("Waiting for {} to wake…", self.inner.worker_label),
        );
        if let Some(state) = self.inner.claims.try_claim(job_id) {
            let this = self.clone();
            let bot_id = bot_id.to_string();
            let job_id_owned = job_id.to_string();
            let handle_owned = handle.clone();
            let command = command.to_string();
            tokio::spawn(async move {
                this.wait_then_spawn(&bot_id, &job_id_owned, &handle_owned, &command, state)
                    .await;
            });
        }
        // Either this call just became the sole winner (task spawned above)
        // or a concurrent call already owns the wait loop - either way, the
        // caller sees the same "queued" answer.
        SpawnResult {
            ok: true,
            handle,
            detail: format!("queued - waiting for {} to wake", self.inner.worker_label),
        }
    }

    /// Test seam: attempts the SAME claim `spawn` takes before starting a
    /// background wait loop, without going through `ping`/`spawn` at all.
    /// Same precedent as TS `workers.ts`'s own `clearCertDirCacheForTest` -
    /// a narrow hook that exists only so a test can drive the exact guard
    /// this ticket's bite targets (`ClaimRegistry::try_claim`) directly,
    /// instead of racing it indirectly through `spawn`'s docker calls, whose
    /// ORDER two genuinely concurrent tasks make no promise about (a
    /// background loop's own retry ping can land before the other
    /// claimant's first ping - real concurrency, not a bug, but it makes
    /// "count the docker calls" an unreliable signal for "who won the
    /// claim"). A winning call here does NOT release the claim - only
    /// `spawn`'s real wait-loop lifecycle does that.
    #[doc(hidden)]
    pub fn try_claim_for_test(&self, job_id: &str) -> bool {
        self.inner.claims.try_claim(job_id).is_some()
    }

    pub async fn probe(&self, handle: &str) -> ProbeResult {
        if let Some(job_id) = job_id_from_handle(handle)
            && let Some(state) = self.inner.claims.get(job_id)
            && !state.cancelled.load(Ordering::SeqCst)
        {
            return ProbeResult {
                running: true,
                exit_code: None,
                detail: format!("queued - waiting for {} to wake", self.inner.worker_label),
            };
        }
        let result = self
            .inner
            .docker
            .run(
                vec![
                    "inspect".to_string(),
                    "-f".to_string(),
                    "{{.State.Running}} {{.State.ExitCode}}".to_string(),
                    handle.to_string(),
                ],
                15_000,
            )
            .await;
        if !result.ok {
            let detail = first_line(&result.stderr);
            return ProbeResult {
                running: false,
                exit_code: None,
                detail: if detail.is_empty() {
                    "gone".to_string()
                } else {
                    detail
                },
            };
        }
        parse_probe(&result.stdout)
    }

    pub async fn kill(&self, handle: &str) -> bool {
        if let Some(job_id) = job_id_from_handle(handle)
            && let Some(state) = self.inner.claims.get(job_id)
        {
            state.cancelled.store(true, Ordering::SeqCst);
            self.inner.claims.release(job_id);
            return true;
        }
        let result = self
            .inner
            .docker
            .run(
                vec!["rm".to_string(), "-f".to_string(), handle.to_string()],
                20_000,
            )
            .await;
        result.ok
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_db() -> Db {
        Db::open(":memory:").expect("open in-memory db")
    }

    #[test]
    fn resolve_sandbox_source_no_worker_is_meridian() {
        assert_eq!(resolve_sandbox_source(None, false), SandboxSource::Meridian);
        assert_eq!(resolve_sandbox_source(None, true), SandboxSource::Meridian);
    }

    #[test]
    fn resolve_sandbox_source_deleted_worker_never_falls_back() {
        assert_eq!(
            resolve_sandbox_source(Some("w1"), false),
            SandboxSource::Missing {
                worker_id: "w1".to_string()
            }
        );
    }

    #[test]
    fn resolve_sandbox_source_existing_worker() {
        assert_eq!(
            resolve_sandbox_source(Some("w1"), true),
            SandboxSource::Worker
        );
    }

    #[test]
    fn parse_docker_version_missing_server_reads_as_asleep_not_online() {
        let info = parse_docker_version(true, r#"{"Client":{}}"#, "");
        assert!(!info.ok);
        assert!(info.error.unwrap().contains("no server"));
    }

    #[test]
    fn parse_docker_version_ok() {
        let info = parse_docker_version(
            true,
            r#"{"Server":{"Os":"linux","Arch":"amd64","Version":"27.0.0"}}"#,
            "",
        );
        assert!(info.ok);
        assert_eq!(info.os.as_deref(), Some("linux"));
        assert_eq!(info.arch.as_deref(), Some("amd64"));
        assert_eq!(info.version.as_deref(), Some("27.0.0"));
    }

    #[test]
    fn parse_docker_version_cli_failed() {
        let info = parse_docker_version(false, "", "connection refused\nextra");
        assert!(!info.ok);
        assert_eq!(info.error.as_deref(), Some("connection refused"));
    }

    #[test]
    fn parse_docker_version_bad_json() {
        let info = parse_docker_version(true, "not json", "");
        assert!(!info.ok);
        assert!(info.error.unwrap().contains("readable JSON"));
    }

    #[test]
    fn sh_quote_escapes_embedded_quotes() {
        assert_eq!(sh_quote("it's"), r"'it'\''s'");
    }

    #[test]
    fn ssh_argv_quotes_each_docker_token_before_joining() {
        let argv = ssh_argv(
            "bullpen",
            "host",
            "/id",
            &["run".to_string(), "a b".to_string()],
        );
        assert_eq!(argv.last().unwrap(), "docker 'run' 'a b'");
    }

    #[test]
    fn worker_crud_roundtrips_through_settings() {
        let db = test_db();
        let input = WorkerInput {
            label: "Workstation".to_string(),
            kind: WorkerKind::Ssh,
            ssh_user: Some("bullpen".to_string()),
            ssh_host: Some("100.1.2.3".to_string()),
            ..Default::default()
        };
        let result = upsert_worker(&db, &input);
        assert!(result.ok, "{:?}", result.error);
        let public = result.worker.unwrap();
        assert_eq!(public.label, "Workstation");
        assert!(!public.has_cert);

        let listed = list_workers(&db);
        assert_eq!(listed.len(), 1);
        assert_eq!(listed[0].id, public.id);

        let removed = remove_worker(&db, &public.id);
        assert!(removed.is_empty());
    }

    #[test]
    fn upsert_tcp_worker_requires_all_three_cert_fields() {
        let db = test_db();
        let input = WorkerInput {
            label: "Meridian".to_string(),
            kind: WorkerKind::Tcp,
            host: Some("100.1.1.1".to_string()),
            ca: Some("ca-pem".to_string()),
            // cert/key missing
            ..Default::default()
        };
        let result = upsert_worker(&db, &input);
        assert!(!result.ok);
        assert!(result.error.unwrap().contains("CA certificate"));
    }

    #[test]
    fn sanitize_stored_skips_rows_missing_id_or_label() {
        let raw: Value = serde_json::from_str(
            r#"[{"id":"","label":"x"},{"id":"a","label":""},{"id":"a","label":"b"}]"#,
        )
        .unwrap();
        let items: Vec<Worker> = raw
            .as_array()
            .unwrap()
            .iter()
            .filter_map(sanitize_stored)
            .collect();
        assert_eq!(items.len(), 1);
        assert_eq!(items[0].id, "a");
    }

    #[test]
    fn ensure_bot_worker_column_is_idempotent() {
        let db = test_db();
        db.conn()
            .execute(
                "INSERT INTO bots (id, name, purpose, instructions, model, created_at) VALUES ('b1','b','','i',NULL,'2026-01-01T00:00:00Z')",
                [],
            )
            .unwrap();
        ensure_bot_worker_column(&db).unwrap();
        ensure_bot_worker_column(&db).unwrap(); // second call must not error
        assert_eq!(get_bot_worker_id(&db, "b1").unwrap(), None);
        set_bot_worker_id(&db, "b1", Some("w1")).unwrap();
        assert_eq!(
            get_bot_worker_id(&db, "b1").unwrap(),
            Some("w1".to_string())
        );
        set_bot_worker_id(&db, "b1", Some("  ")).unwrap();
        assert_eq!(get_bot_worker_id(&db, "b1").unwrap(), None);
    }

    #[test]
    fn parse_probe_running_never_reports_an_exit_code() {
        let p = parse_probe("true 0\n");
        assert!(p.running);
        assert_eq!(p.exit_code, None);
    }

    #[test]
    fn parse_probe_unparseable_code_reads_as_unknown_not_success() {
        let p = parse_probe("false garbage");
        assert!(!p.running);
        assert_eq!(p.exit_code, None);
    }

    #[test]
    fn parse_probe_stopped_with_code() {
        let p = parse_probe("false 3");
        assert!(!p.running);
        assert_eq!(p.exit_code, Some(3));
    }

    /// **S8b-05 F13 bite.** With the hand-written `Debug`, formatting a
    /// `WorkerInput` whose `key` is a real-shaped PEM never puts the key's
    /// bytes into the output - proven by asserting the secret's absence AND
    /// a non-secret field's presence, so this cannot pass by formatting
    /// nothing at all. See this ticket's Result for the guard-removed
    /// (`#[derive(Debug)]` restored) literal output.
    #[test]
    fn worker_input_debug_never_prints_the_private_key_or_cert_or_ca() {
        let input = WorkerInput {
            label: "Meridian".to_string(),
            host: Some("100.1.1.1".to_string()),
            ca: Some("-----BEGIN CERTIFICATE-----AAAA".to_string()),
            cert: Some("-----BEGIN CERTIFICATE-----BBBB".to_string()),
            key: Some("-----BEGIN PRIVATE KEY-----AAAA".to_string()),
            ..Default::default()
        };
        let out = format!("{input:?}");
        assert!(
            !out.contains("AAAA") && !out.contains("BBBB") && !out.contains("BEGIN PRIVATE KEY"),
            "Debug output must not contain any secret material, got: {out}"
        );
        assert!(
            out.contains("Meridian") && out.contains("100.1.1.1"),
            "Debug output must still contain non-secret fields, got: {out}"
        );
    }

    /// **S8b-05 F13, the row beside it.** `Worker` carries the same material
    /// encrypted at rest (`ca_encrypted`/`cert_encrypted`/`key_encrypted`);
    /// its hand-written `Debug` must not print the ciphertext either.
    #[test]
    fn worker_debug_never_prints_the_encrypted_cert_material() {
        let worker = Worker {
            id: "w1".to_string(),
            label: "Meridian".to_string(),
            kind: WorkerKind::Tcp,
            host: "100.1.1.1".to_string(),
            port: 2376,
            ca_encrypted: "enc:AAAA".to_string(),
            cert_encrypted: "enc:BBBB".to_string(),
            key_encrypted: "enc:CCCC".to_string(),
            ssh_user: String::new(),
            ssh_host: String::new(),
            last_state: WorkerState::Unknown,
            last_checked_at: None,
            last_os: None,
            last_arch: None,
            last_version: None,
            last_error: None,
        };
        let out = format!("{worker:?}");
        assert!(
            !out.contains("AAAA") && !out.contains("BBBB") && !out.contains("CCCC"),
            "Debug output must not contain any encrypted cert material, got: {out}"
        );
        assert!(
            out.contains("Meridian") && out.contains("w1"),
            "Debug output must still contain non-secret fields, got: {out}"
        );
    }
}
