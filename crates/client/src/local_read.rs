//! S13b-03-04: reading a file a bot asked for, safely, on Josh's own
//! machine. Design `.scratch/bullpen-rs/designs/S13b-03-local-bridge.md`
//! §5.1-5.4 (revision 4) is the security argument this module implements;
//! read it before touching the order below.
//!
//! 🔴 **This module is deliberately inert.** Nothing calls it yet - no UI
//! trigger, no wiring into `approvals.rs`. The slice that WOULD call it
//! depends on an egress-taint design (§4.5) that four review rounds have
//! not settled, and the orchestrator is not enabling a bot-reachable read
//! path on Josh's own machine while that is open. `main.rs` still `mod`s
//! this in (S13b-02's `window_state` precedent: a module nothing calls
//! fails clippy's `dead_code` lint otherwise), and every item below is
//! `pub(crate)` for the same reason - there is no caller yet to make them
//! merely `pub(crate)`-reachable through.
//!
//! **The order is the whole argument.** Every check that exists to prevent
//! an *open* runs before [`LocalFs::canonicalize`], because canonicalize
//! *is* an open: the SMB session that leaks a NetNTLMv2 hash, the
//! named-pipe connect that can block, and the device open all happen
//! inside it (design §5.1). [`LocalFs`] is the seam TS already has as
//! `Fs = { realpath, stat, readFile }` (`readNamedFile.ts`) - without an
//! injectable resolver the refusal suite cannot be tested at all, since
//! creating a Windows symlink needs `SeCreateSymbolicLinkPrivilege` and a
//! test that skips when creation fails is green whether the guard is
//! present or removed.
//!
//! **Two measured facts that decide where the checks run** (design §5.3,
//! confirmed on this machine):
//! - `canonicalize` normalises away both a trailing `::$DATA` stream
//!   selector and a trailing dot, so the colon rule and the DOS-device-name
//!   rule run on the *typed* path (step 2) - running them only on the
//!   resolved path (step 5) tests nothing, because the payload is already
//!   gone by the time step 5 sees it.
//! - On a resolved path the `\\?\` verbatim prefix moves the drive letter's
//!   colon later in the string than it sits on a typed path - this module
//!   derives that offset from the parsed prefix component's own length
//!   ([`shape_allowed`]) rather than hard-coding an index, but it is the
//!   same fact the design measured: strip the prefix before judging where a
//!   stray colon is allowed, or the colon rule refuses every legitimate
//!   resolved path.
//!
//! **The divergence check (step 7) is a residual, not a fix** (design
//! §5.4). An unprivileged `mklink /H notes.md real_secret.key` resolves to
//! its *own* name - `typed` and `resolved` are identical - so steps 3, 6
//! and 7 all pass and a hardlinked credential would be read. This is not
//! bot-reachable (nothing a bot can call creates a hardlink on Josh's
//! workstation; it requires a human to have made the link first), so it is
//! named here rather than closed. Do not let a future reader believe
//! divergence closes the class.
//!
//! **Legitimate divergence is not an accusation.** An 8.3 short path
//! (`C:\PROGRA~1`), a `subst`ed drive, and a mapped network drive all
//! diverge for entirely ordinary reasons. [`Refusal::Diverged`] carries its
//! own message - "that path resolves somewhere else on disk" - never the
//! credential sentence, which would be a lie for any of those three.

use regex::RegexBuilder;
use std::io;
use std::path::{Component, Path, PathBuf, Prefix};
use std::sync::LazyLock;

/// Matches the server's own attachment ceiling (`readNamedFile.ts::MAX_BYTES`).
pub(crate) const MAX_BYTES: u64 = 25 * 1024 * 1024;

/// The filesystem calls, injectable so every refusal can be tested without
/// a disk and without privileges (design §5.1, R1-F6). Mirrors TS's
/// `type Fs = { realpath, stat, readFile }`.
pub(crate) trait LocalFs {
    type Info: FileInfo;

    /// The **first syscall** in the whole pipeline (step 4). Everything
    /// above this trait exists so nothing runs this on a path that has not
    /// already passed the shape allow-list, the credential-shape refusal,
    /// and the absolute/non-empty check.
    fn canonicalize(&self, path: &Path) -> io::Result<PathBuf>;
    fn metadata(&self, path: &Path) -> io::Result<Self::Info>;
    fn read(&self, path: &Path) -> io::Result<Vec<u8>>;
}

/// The slice of `std::fs::Metadata` this module actually needs, so a fake
/// [`LocalFs`] can hand back a plain struct instead of a real
/// `std::fs::Metadata` (which nothing outside `std::fs` can construct).
pub(crate) trait FileInfo {
    fn is_dir(&self) -> bool;
    fn is_file(&self) -> bool;
    fn len(&self) -> u64;
}

impl FileInfo for std::fs::Metadata {
    fn is_dir(&self) -> bool {
        std::fs::Metadata::is_dir(self)
    }
    fn is_file(&self) -> bool {
        std::fs::Metadata::is_file(self)
    }
    fn len(&self) -> u64 {
        std::fs::Metadata::len(self)
    }
}

/// The real filesystem. Nothing in this crate constructs one yet - see this
/// module's top doc.
pub(crate) struct RealFs;

impl LocalFs for RealFs {
    type Info = std::fs::Metadata;

    fn canonicalize(&self, path: &Path) -> io::Result<PathBuf> {
        std::fs::canonicalize(path)
    }
    fn metadata(&self, path: &Path) -> io::Result<Self::Info> {
        std::fs::metadata(path)
    }
    fn read(&self, path: &Path) -> io::Result<Vec<u8>> {
        std::fs::read(path)
    }
}

/// A file read cleanly through every guard.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ReadOutcome {
    /// The resolved path the bytes actually came from (step 4's output) -
    /// deliberately not the typed one, so a caller can never accidentally
    /// present a path the read did not use.
    pub(crate) resolved_path: PathBuf,
    pub(crate) bytes: Vec<u8>,
}

/// Which shape rule (design §5.3) refused a path. Kept distinct from
/// [`Refusal`]'s other variants so a bite can assert *which* guard fired,
/// not merely that one did (design §7 bite 6, and the ticket's "assert
/// which rule refused it").
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ShapeViolation {
    /// The path's `std::path::Prefix` is not `Disk`/`VerbatimDisk` - a UNC
    /// share (`\\host\share\...`, `\\?\UNC\...`), a device namespace
    /// (`\\.\pipe\...`), or a bare verbatim path
    /// (`\\?\GLOBALROOT\Device\...`).
    PrefixNotAllowed,
    /// A `:` appears somewhere other than the drive-letter position -
    /// `C:\certs\server.pem::$DATA` and anything shaped like it.
    ColonOutsideDrive,
    /// A path component's stem (the part before any extension - Windows
    /// treats `NUL.txt` as the device same as bare `NUL`) is a legacy DOS
    /// device name: `CON`, `PRN`, `AUX`, `NUL`, `COM1`-`COM9`, `LPT1`-`LPT9`.
    DosDeviceName,
}

/// Why a path was not read. [`Refusal::message`] is the sentence a caller
/// shows; the variant is what a test asserts against.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Refusal {
    /// Step 1: nothing given, or only whitespace.
    Empty,
    /// Step 1: a relative path.
    NotAbsolute,
    /// Steps 2 and 5: [`ShapeViolation`].
    Shape(ShapeViolation),
    /// Steps 3 and 6: `.ssh`, `.bullpen`, a `.pem`, `id_ed25519`, and the
    /// rest of design §5.2's list. Steps 3 and 6 are not redundant - step 6
    /// only fires for a path that also diverges (design §5.2, R2-G6) - but
    /// they share one variant and one message, since both mean the same
    /// thing to whoever reads the refusal.
    CredentialShaped,
    /// Step 4 or step 8: canonicalize or metadata failed - most commonly
    /// because nothing is there.
    NoSuchFile,
    /// Step 7: resolved and typed disagree, and step 6 already cleared the
    /// resolved path of being credential-shaped - so this is always the
    /// legitimate-divergence message (8.3 short path, `subst`, a mapped
    /// drive, a human-made hardlink), never the credential one (design
    /// §5.4).
    Diverged { resolved: String },
    /// Step 8: a directory, not a file.
    IsDirectory,
    /// Step 8: exists, but is not a regular file (a symlink `stat`
    /// couldn't resolve, a device file, ...).
    NotAFile,
    /// Step 8 or step 9: bigger than [`MAX_BYTES`], caught at metadata time
    /// when possible and again on the actual byte count.
    TooLarge { bytes: u64 },
    /// Step 9: metadata cleared it but the read itself failed (permissions,
    /// the file vanished between steps 8 and 9, ...).
    ReadFailed(String),
}

impl Refusal {
    /// The sentence a caller shows. Never the credential accusation for a
    /// case design §5.4 says is not one.
    pub(crate) fn message(&self) -> String {
        match self {
            Refusal::Empty => "no path given".to_string(),
            Refusal::NotAbsolute => "only absolute paths are read".to_string(),
            Refusal::Shape(ShapeViolation::PrefixNotAllowed) => {
                "that isn't a plain local drive path - network shares and device paths aren't read"
                    .to_string()
            }
            Refusal::Shape(ShapeViolation::ColonOutsideDrive) => {
                "that path has a ':' outside the drive letter, which isn't read".to_string()
            }
            Refusal::Shape(ShapeViolation::DosDeviceName) => {
                "that path names a reserved device, which isn't read".to_string()
            }
            Refusal::CredentialShaped => {
                "that path looks like a credential, so it is not read into a conversation"
                    .to_string()
            }
            Refusal::NoSuchFile => "no such file on this machine".to_string(),
            Refusal::Diverged { resolved } => format!(
                "that path resolves somewhere else on disk; give me the resolved path and I will read that: {resolved}"
            ),
            Refusal::IsDirectory => "that is a folder, not a file".to_string(),
            Refusal::NotAFile => "not a regular file".to_string(),
            Refusal::TooLarge { bytes } => format!(
                "that file is {} MB, over the {} MB limit",
                bytes.div_ceil(1024 * 1024),
                MAX_BYTES / (1024 * 1024)
            ),
            Refusal::ReadFailed(err) => err.clone(),
        }
    }
}

/// Legacy DOS device names (design §5.3). Windows treats these as the
/// device in ANY component, with or without an extension, which is why
/// [`shape_allowed`] compares stems, not whole component names.
const DOS_DEVICE_NAMES: &[&str] = &[
    "CON", "PRN", "AUX", "NUL", "COM1", "COM2", "COM3", "COM4", "COM5", "COM6", "COM7", "COM8",
    "COM9", "LPT1", "LPT2", "LPT3", "LPT4", "LPT5", "LPT6", "LPT7", "LPT8", "LPT9",
];

fn is_dos_device_name(stem: &str) -> bool {
    DOS_DEVICE_NAMES
        .iter()
        .any(|name| name.eq_ignore_ascii_case(stem))
}

/// Steps 2 and 5 (design §5.3): the path's shape must be on the allow-list,
/// not merely absent from the refusal list. Runs on a raw string rather
/// than a `Path` alone because the colon rule needs to know exactly how
/// much of the string the drive prefix consumed - see this module's top
/// doc on why that differs between a typed path and a `\\?\`-resolved one.
fn shape_allowed(path_str: &str) -> Result<(), Refusal> {
    let path = Path::new(path_str);

    let prefix_component = match path.components().next() {
        Some(Component::Prefix(p)) => p,
        _ => return Err(Refusal::Shape(ShapeViolation::PrefixNotAllowed)),
    };
    match prefix_component.kind() {
        Prefix::Disk(_) | Prefix::VerbatimDisk(_) => {}
        // UNC, VerbatimUNC, DeviceNS, or a bare Verbatim (`\\?\GLOBALROOT\...`).
        _ => return Err(Refusal::Shape(ShapeViolation::PrefixNotAllowed)),
    }

    // Colon rule: refuse any `:` after the drive prefix. `prefix_component`
    // is measured, not assumed - `C:` is 2 bytes typed, `\\?\C:` is 6 bytes
    // resolved, and using the parsed length instead of a hard-coded index
    // is what makes this one function correct for both (design §5.3's
    // "index 1 vs index 5" is this same fact, observed rather than encoded).
    let prefix_len = prefix_component.as_os_str().len();
    if path_str
        .get(prefix_len..)
        .is_some_and(|rest| rest.contains(':'))
    {
        return Err(Refusal::Shape(ShapeViolation::ColonOutsideDrive));
    }

    for component in path.components() {
        if let Component::Normal(name) = component {
            let name = name.to_string_lossy();
            let stem = name.split('.').next().unwrap_or(&name);
            if is_dos_device_name(stem) {
                return Err(Refusal::Shape(ShapeViolation::DosDeviceName));
            }
        }
    }

    Ok(())
}

/// Steps 3 and 6 (design §5.2): a direct port of `localPaths.ts`'s
/// `REFUSED` list, kept as real `regex` patterns (case-insensitive, the
/// `(?i)` TS's own `/i` flag maps to) rather than hand-rolled string
/// matching, so the port stays literal instead of merely equivalent.
static CREDENTIAL_SHAPED: LazyLock<Vec<regex::Regex>> = LazyLock::new(|| {
    [
        r"(^|[\\/])\.ssh([\\/]|$)",
        r"(^|[\\/])\.bullpen([\\/]|$)",
        r"(^|[\\/])\.aws([\\/]|$)",
        r"(^|[\\/])\.gnupg([\\/]|$)",
        r"(^|[\\/])\.env(\.|$)",
        r"\.(key|pem|pfx|p12|ppk|keystore|jks)$",
        r"(^|[\\/])id_(rsa|dsa|ecdsa|ed25519)(\.|$)",
        r"(^|[\\/])(credentials|secrets?)\.(json|ya?ml|toml|ini|txt)$",
        r"(^|[\\/])shadow$",
    ]
    .into_iter()
    .map(|pattern| {
        RegexBuilder::new(pattern)
            .case_insensitive(true)
            .build()
            .expect("credential-shape patterns are fixed and tested, not user input")
    })
    .collect()
});

fn is_credential_shaped(path_str: &str) -> bool {
    CREDENTIAL_SHAPED.iter().any(|re| re.is_match(path_str))
}

/// Step 7: strip a leading `\\?\` and lower-case, so a typed path and its
/// resolved form compare equal when they differ only in the verbatim
/// prefix or in case (design §5.4's "normalised for the verbatim prefix
/// and for case" - both, nothing else: separators and `.`/`..` are not
/// touched here, because by step 7 both strings came either from the
/// user's own typed absolute path or from [`LocalFs::canonicalize`], never
/// from unnormalised user input).
fn normalize_for_identity(path_str: &str) -> String {
    path_str
        .strip_prefix(r"\\?\")
        .unwrap_or(path_str)
        .to_lowercase()
}

fn same_file_identity(typed: &str, resolved: &str) -> bool {
    normalize_for_identity(typed) == normalize_for_identity(resolved)
}

/// The nine-step order in design §5.1. Every step that can be answered
/// without touching the filesystem runs before [`LocalFs::canonicalize`]
/// (step 4) - see this module's top doc for why that order is not
/// incidental.
pub(crate) fn read_local_file<F: LocalFs>(fs: &F, requested: &str) -> Result<ReadOutcome, Refusal> {
    // 1. non-empty, absolute.
    let typed = requested.trim();
    if typed.is_empty() {
        return Err(Refusal::Empty);
    }
    if !Path::new(typed).is_absolute() {
        return Err(Refusal::NotAbsolute);
    }

    // 2. shape allow-list on the TYPED path.
    shape_allowed(typed)?;

    // 3. refuse_reason(typed).
    if is_credential_shaped(typed) {
        return Err(Refusal::CredentialShaped);
    }

    // 4. canonicalize - the first syscall. Nothing above this line touches
    // the filesystem or the network.
    let resolved = fs
        .canonicalize(Path::new(typed))
        .map_err(|_| Refusal::NoSuchFile)?;
    let resolved_str = resolved.to_string_lossy().into_owned();

    // 5. shape allow-list again on the RESOLVED path - canonicalize can
    // cross a reparse point into a shape step 2 never saw.
    shape_allowed(&resolved_str)?;

    // 6. refuse_reason(resolved). Not dead: only reachable for a path that
    // also diverges from typed, which step 7 would otherwise be the only
    // guard on (design §5.2, R2-G6).
    if is_credential_shaped(&resolved_str) {
        return Err(Refusal::CredentialShaped);
    }

    // 7. divergence: resolved must equal typed. By construction, resolved
    // is never credential-shaped here (step 6 already returned), so this
    // is always the legitimate-divergence message, never an accusation -
    // and it is a residual, not a fix: see this module's top doc on
    // hardlinks.
    if !same_file_identity(typed, &resolved_str) {
        return Err(Refusal::Diverged {
            resolved: resolved_str,
        });
    }

    // 8. metadata: not a directory, a regular file, under the cap.
    let info = fs.metadata(&resolved).map_err(|_| Refusal::NoSuchFile)?;
    if info.is_dir() {
        return Err(Refusal::IsDirectory);
    }
    if !info.is_file() {
        return Err(Refusal::NotAFile);
    }
    if info.len() > MAX_BYTES {
        return Err(Refusal::TooLarge { bytes: info.len() });
    }

    // 9. read, cap, return.
    let bytes = fs
        .read(&resolved)
        .map_err(|err| Refusal::ReadFailed(err.to_string()))?;
    if bytes.len() as u64 > MAX_BYTES {
        return Err(Refusal::TooLarge {
            bytes: bytes.len() as u64,
        });
    }
    Ok(ReadOutcome {
        resolved_path: resolved,
        bytes,
    })
}

#[cfg(test)]
#[path = "local_read_tests.rs"]
mod local_read_tests;
