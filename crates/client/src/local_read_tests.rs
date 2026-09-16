//! S13b-03-04's tests for [`super::read_local_file`] and the guards it
//! chains together. Nested into `local_read.rs` via
//! `#[path = "local_read_tests.rs"] mod local_read_tests;`, matching
//! S13b-02's `window_state.rs`/`window_state_tests.rs` split - `use
//! super::*` reaches every `pub(crate)`/private item the same way.
//!
//! [`FakeFs`] is the injected [`super::LocalFs`] design §5.1 calls for by
//! name (R1-F6): every refusal below is proven without a disk and without
//! the privilege a real Windows symlink/junction would need to create.
//!
//! **Proof rule (ticket header, design §7):** for design §7 bites 4, 5, 6
//! (its second half) and 12, this file's `## Results` entry records the
//! literal red output from breaking each guard and the literal green from
//! restoring it - the assertions here are what the mutation flips.

use super::*;
use std::cell::Cell;

/// A `std::fs::Metadata`-shaped stand-in a test can construct directly.
#[derive(Clone, Copy)]
struct FakeInfo {
    dir: bool,
    file: bool,
    len: u64,
}

impl FileInfo for FakeInfo {
    fn is_dir(&self) -> bool {
        self.dir
    }
    fn is_file(&self) -> bool {
        self.file
    }
    fn len(&self) -> u64 {
        self.len
    }
}

fn ordinary_file(len: u64) -> FakeInfo {
    FakeInfo {
        dir: false,
        file: true,
        len,
    }
}

/// A resolver that maps exactly one typed path to one resolved path (or
/// resolves nothing at all), records how many times `canonicalize` was
/// called, and hands back one fixed metadata/content pair regardless of
/// which path `metadata`/`read` are asked about.
///
/// One mapping is enough: every test here drives `read_local_file` exactly
/// once, and `canonicalize` is step 4 of nine - nothing later in the order
/// calls it again.
struct FakeFs {
    canonicalize_calls: Cell<u32>,
    resolves_to: Option<(String, PathBuf)>,
    info: FakeInfo,
    content: Vec<u8>,
}

impl FakeFs {
    /// `typed` resolves to `resolved`, and a read of it returns `content`.
    fn mapping(typed: &str, resolved: &str, content: &[u8]) -> Self {
        FakeFs {
            canonicalize_calls: Cell::new(0),
            resolves_to: Some((typed.to_string(), PathBuf::from(resolved))),
            info: ordinary_file(content.len() as u64),
            content: content.to_vec(),
        }
    }

    /// No path resolves to anything - `canonicalize` always fails
    /// `NotFound`. Used both for "this path must be refused before
    /// `canonicalize` runs at all" (assert `canonicalize_calls() == 0`) and
    /// for "this path reaches `canonicalize`, which then reports no such
    /// file" (assert `canonicalize_calls() == 1`) - the two are told apart
    /// by the call count a test checks, not by anything this fake does
    /// differently.
    fn no_mapping() -> Self {
        FakeFs {
            canonicalize_calls: Cell::new(0),
            resolves_to: None,
            info: ordinary_file(0),
            content: Vec::new(),
        }
    }

    fn canonicalize_calls(&self) -> u32 {
        self.canonicalize_calls.get()
    }
}

impl LocalFs for FakeFs {
    type Info = FakeInfo;

    fn canonicalize(&self, path: &Path) -> io::Result<PathBuf> {
        self.canonicalize_calls
            .set(self.canonicalize_calls.get() + 1);
        match &self.resolves_to {
            Some((typed, resolved)) if typed.as_str() == path.to_string_lossy() => {
                Ok(resolved.clone())
            }
            _ => Err(io::Error::new(
                io::ErrorKind::NotFound,
                "no mapping for this test",
            )),
        }
    }

    fn metadata(&self, _path: &Path) -> io::Result<Self::Info> {
        Ok(self.info)
    }

    fn read(&self, _path: &Path) -> io::Result<Vec<u8>> {
        Ok(self.content.clone())
    }
}

// ---------------------------------------------------------------------
// Step 1: non-empty, absolute.
// ---------------------------------------------------------------------

#[test]
fn empty_path_is_refused_before_any_check() {
    let fs = FakeFs::no_mapping();
    assert_eq!(read_local_file(&fs, "   "), Err(Refusal::Empty));
    assert_eq!(fs.canonicalize_calls(), 0);
}

#[test]
fn relative_path_is_refused_before_any_check() {
    let fs = FakeFs::no_mapping();
    assert_eq!(read_local_file(&fs, r"notes.md"), Err(Refusal::NotAbsolute));
    assert_eq!(fs.canonicalize_calls(), 0);
}

// ---------------------------------------------------------------------
// Design §7 bite 6 (second half only - the render half belongs to
// `approvals.rs`, out of this ticket's scope; see this crate's
// `local_read.rs` top doc). Each literal must be refused by the SHAPE
// guard specifically, before `canonicalize` ever runs.
// ---------------------------------------------------------------------

#[test]
fn bite6_shape_refused_literals_never_reach_canonicalize() {
    let cases: &[(&str, ShapeViolation)] = &[
        (
            r"\\45.13.1.1\share\notes.txt",
            ShapeViolation::PrefixNotAllowed,
        ),
        (
            r"\\?\UNC\45.13.1.1\share\notes.txt",
            ShapeViolation::PrefixNotAllowed,
        ),
        (r"\\.\pipe\x", ShapeViolation::PrefixNotAllowed),
        (r"\\?\GLOBALROOT\Device\x", ShapeViolation::PrefixNotAllowed),
        (r"C:\Users\rain\COM1", ShapeViolation::DosDeviceName),
        (
            r"C:\certs\server.pem::$DATA",
            ShapeViolation::ColonOutsideDrive,
        ),
    ];

    for (literal, expected_violation) in cases {
        // Identity-mapped, not `no_mapping()`: if step 2 ever moves
        // downstream, step 5 (the SAME shape check, re-run on the resolved
        // path) still catches an identity-resolved literal - so the
        // refusal variant alone would look unchanged, and only the call
        // count exposes that the open already happened. That is exactly
        // the regression design §7 bite 6 names ("move the check back
        // downstream and it goes red while the refusal itself still
        // passes") - a fake that instead fails to resolve would also flip
        // the refusal variant (to `NoSuchFile`), which would catch the
        // regression for the wrong reason and stop being a test of the
        // call-count claim specifically.
        let fs = FakeFs::mapping(literal, literal, b"");
        let result = read_local_file(&fs, literal);
        assert_eq!(
            result,
            Err(Refusal::Shape(*expected_violation)),
            "{literal} should be refused by {expected_violation:?}, got {result:?}"
        );
        assert_eq!(
            fs.canonicalize_calls(),
            0,
            "{literal} must never reach canonicalize - that call is the open \
             (SMB auth, the pipe connect, the device open) this guard exists to prevent"
        );
    }
}

/// A resolved path is checked against the SAME shape allow-list a second
/// time (step 5) - not covered by bite 6 (which is entirely pre-canonicalize),
/// so a distinct case: an innocent typed path whose resolver maps it into a
/// UNC share. `canonicalize` DOES run here - step 5 is after it.
#[test]
fn shape_check_runs_again_on_the_resolved_path() {
    let fs = FakeFs::mapping(
        r"C:\Users\rain\Documents\link.md",
        r"\\45.13.1.1\share\notes.txt",
        b"payload",
    );
    let result = read_local_file(&fs, r"C:\Users\rain\Documents\link.md");
    assert_eq!(
        result,
        Err(Refusal::Shape(ShapeViolation::PrefixNotAllowed))
    );
    assert_eq!(
        fs.canonicalize_calls(),
        1,
        "step 5 runs AFTER canonicalize - unlike bite 6, this one is expected to call it once"
    );
}

// ---------------------------------------------------------------------
// Steps 3 and 6: the credential-shape refusal list (design §5.2).
// ---------------------------------------------------------------------

#[test]
fn credential_shaped_typed_paths_are_refused_before_canonicalize() {
    let cases = [
        r"C:\Users\rain\.ssh\id_ed25519",
        r"C:\Users\rain\.bullpen\openrouter.key",
        r"C:\Users\rain\.aws\credentials",
        r"C:\Users\rain\.gnupg\secring.gpg",
        r"C:\rainmade\.env",
        r"C:\rainmade\.env.local",
        r"C:\certs\server.pem",
        r"C:\certs\server.pfx",
        r"C:\Users\rain\id_rsa",
        r"C:\Users\rain\id_dsa.pub",
        r"C:\configs\credentials.json",
        r"C:\configs\secret.yaml",
        r"C:\configs\secrets.toml",
        r"C:\etc\shadow",
    ];
    for path in cases {
        let fs = FakeFs::no_mapping();
        let result = read_local_file(&fs, path);
        assert_eq!(
            result,
            Err(Refusal::CredentialShaped),
            "{path} should be refused as credential-shaped, got {result:?}"
        );
        assert_eq!(
            fs.canonicalize_calls(),
            0,
            "{path} must be refused before canonicalize"
        );
    }
}

#[test]
fn ordinary_looking_paths_are_not_credential_shaped() {
    let cases = [
        r"C:\rainmade\notes.md",
        r"C:\rainmade\shadowfax.md",
        r"C:\rainmade\keystone.txt",
    ];
    for path in cases {
        assert!(
            !is_credential_shaped(path),
            "{path} should not be treated as a credential"
        );
    }
}

// ---------------------------------------------------------------------
// Design §7 bite 4: escape via the resolver, isolated to steps 6+7.
// ---------------------------------------------------------------------

/// The fake resolver maps `...\Documents\notes.md` to
/// `C:\Users\rain\.ssh\id_ed25519` - a credential-shaped RESOLVED path
/// reached from an innocent-looking typed one. With the guard chain
/// intact, step 6 (credential check on the resolved path) fires - it runs
/// before step 7, so this is the variant a healthy build returns. The
/// proof that this bite bites removes steps 6 AND 7 together (see this
/// ticket's `## Results` for the literal red/green): removing step 6 alone
/// leaves step 7 (divergence) still refusing it, which is exactly how
/// revision 2's version of this bite stayed green in both worlds (design
/// §7 bite 4).
#[test]
fn bite4_escape_via_resolver_is_refused() {
    let fs = FakeFs::mapping(
        r"C:\Users\rain\Documents\notes.md",
        r"C:\Users\rain\.ssh\id_ed25519",
        b"-----BEGIN OPENSSH PRIVATE KEY-----",
    );
    let result = read_local_file(&fs, r"C:\Users\rain\Documents\notes.md");
    assert_eq!(result, Err(Refusal::CredentialShaped));
    assert_eq!(fs.canonicalize_calls(), 1);
}

// ---------------------------------------------------------------------
// Design §7 bite 5: divergence, isolated to step 7.
// ---------------------------------------------------------------------

/// The fake resolver maps `...\Documents\notes.md` to
/// `D:\rainmade\projects\acme\contract.md` - nothing credential-shaped
/// about that resolved path (steps 3 and 6 both pass it), so only step 7
/// can refuse it.
#[test]
fn bite5_divergence_is_refused() {
    let fs = FakeFs::mapping(
        r"C:\Users\rain\Documents\notes.md",
        r"D:\rainmade\projects\acme\contract.md",
        b"the client's contract",
    );
    let result = read_local_file(&fs, r"C:\Users\rain\Documents\notes.md");
    assert_eq!(
        result,
        Err(Refusal::Diverged {
            resolved: r"D:\rainmade\projects\acme\contract.md".to_string()
        })
    );
}

// ---------------------------------------------------------------------
// Design §7 bite 12: legitimate divergence gets its own message.
// ---------------------------------------------------------------------

/// An 8.3 short path (`C:\PROGRA~1` -> `C:\Program Files`, this design's
/// own measured example) diverges for an entirely ordinary reason and must
/// not read as an accusation.
#[test]
fn bite12_legitimate_divergence_is_not_the_credential_message() {
    let fs = FakeFs::mapping(
        r"C:\PROGRA~1\acme\notes.md",
        r"C:\Program Files\acme\notes.md",
        b"ordinary file contents",
    );
    let result = read_local_file(&fs, r"C:\PROGRA~1\acme\notes.md");
    let Err(refusal) = result else {
        panic!("legitimate divergence must still be refused: {result:?}");
    };
    assert_eq!(
        refusal,
        Refusal::Diverged {
            resolved: r"C:\Program Files\acme\notes.md".to_string()
        }
    );
    let message = refusal.message();
    assert!(
        message.contains("resolves somewhere else on disk"),
        "message was: {message}"
    );
    assert!(
        !message.to_lowercase().contains("credential"),
        "legitimate divergence must not read as an accusation: {message}"
    );
    assert!(
        message.contains(r"C:\Program Files\acme\notes.md"),
        "the resolved path must be shown so Josh can retype it: {message}"
    );
}

// ---------------------------------------------------------------------
// Steps 8-9: metadata and the read itself.
// ---------------------------------------------------------------------

#[test]
fn ordinary_local_file_is_read() {
    let fs = FakeFs::mapping(
        r"C:\rainmade\notes.md",
        r"C:\rainmade\notes.md",
        b"hello from disk",
    );
    let outcome =
        read_local_file(&fs, r"C:\rainmade\notes.md").expect("an ordinary local file must be read");
    assert_eq!(outcome.bytes, b"hello from disk");
    assert_eq!(
        outcome.resolved_path,
        PathBuf::from(r"C:\rainmade\notes.md")
    );
}

#[test]
fn directory_is_refused() {
    let mut fs = FakeFs::mapping(r"C:\rainmade", r"C:\rainmade", b"");
    fs.info = FakeInfo {
        dir: true,
        file: false,
        len: 0,
    };
    assert_eq!(
        read_local_file(&fs, r"C:\rainmade"),
        Err(Refusal::IsDirectory)
    );
}

#[test]
fn non_regular_file_is_refused() {
    let mut fs = FakeFs::mapping(r"C:\dev\thing", r"C:\dev\thing", b"");
    fs.info = FakeInfo {
        dir: false,
        file: false,
        len: 0,
    };
    assert_eq!(
        read_local_file(&fs, r"C:\dev\thing"),
        Err(Refusal::NotAFile)
    );
}

#[test]
fn oversized_file_is_refused_at_metadata() {
    let mut fs = FakeFs::mapping(r"C:\big.bin", r"C:\big.bin", b"");
    fs.info = FakeInfo {
        dir: false,
        file: true,
        len: MAX_BYTES + 1,
    };
    assert_eq!(
        read_local_file(&fs, r"C:\big.bin"),
        Err(Refusal::TooLarge {
            bytes: MAX_BYTES + 1
        })
    );
}

#[test]
fn missing_file_is_refused_after_canonicalize_fails() {
    let fs = FakeFs::no_mapping();
    assert_eq!(
        read_local_file(&fs, r"C:\rainmade\ghost.md"),
        Err(Refusal::NoSuchFile)
    );
    assert_eq!(
        fs.canonicalize_calls(),
        1,
        "this path clears every pre-canonicalize guard, so it DOES reach canonicalize - it just finds nothing there"
    );
}

// ---------------------------------------------------------------------
// `RealFs` itself - every test above injects `FakeFs` so the guards can be
// proven without touching a disk, but `RealFs` is what actually ships.
// Nothing outside this file constructs one yet (this module is inert - see
// its top doc), so this is also what keeps it out of clippy's `dead_code`
// per the ticket's own instruction ("make the items `pub(crate)` ... rather
// than inventing a caller") - a real end-to-end read, not a UI caller.
// ---------------------------------------------------------------------

#[test]
fn real_fs_reads_an_actual_file_end_to_end() {
    let path = std::env::temp_dir().join(format!(
        "bullpen-rs-local-read-{pid}-{nanos}.txt",
        pid = std::process::id(),
        nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("the clock is after 1970")
            .as_nanos()
    ));
    std::fs::write(&path, b"real disk, real bytes").expect("write the throwaway fixture");

    let result = read_local_file(&RealFs, &path.to_string_lossy());

    // A single named file, never recursive - the workspace rule this
    // repo's own CLAUDE.md and .scratch rails both carry.
    let _ = std::fs::remove_file(&path);

    let outcome = result.expect("RealFs must read a real, ordinary file it just wrote");
    assert_eq!(outcome.bytes, b"real disk, real bytes");
}
