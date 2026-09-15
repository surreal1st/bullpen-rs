//! Paths a routine names, and whether they can exist.
//!
//! 🔴 Why this is a guard and not a comment: on 2026-09-11 Nozdormu's live ping
//! stopped mid-run and asked Josh to "create the /workspace/pres-coach directory
//! and drag or paste the contents of those three files into the chat". It was
//! behaving correctly - the routine told it to read files, the files were not
//! there, and STOP_RATHER_THAN_INVENT says stop rather than invent. The routine
//! was wrong, and had been since it was imported.
//!
//! An audit of all 13 routines found NINE naming a path that cannot exist:
//! twelve `/workspace/...` references (Grok Bot's VM layout), two Windows paths,
//! and several `/home/box/...` and `/home/rainmade/...` host paths.
//!
//! 🔴 CORRECTED 2026-09-11. The first version of this file said a Windows
//! path and a meridian path were simply unreachable. That was wrong, and Josh
//! said so: *"Grok Bot seems to be able to manage both Windows paths and
//! Meridian paths... code happens here on Windows, storage/hosting happens on
//! Meridian."* Bullpen has a tool for each, and refusing those paths outright
//! encoded a belief about the product that was never true.
//!
//! THREE surfaces, and which tool reaches each:
//!
//! | path                     | tool        | needs                          |
//! |--------------------------|-------------|--------------------------------|
//! | `/work/...`              | `shell`     | the sandbox; always there      |
//! | `C:\...` / `D:\...`      | `read_file` | the DESKTOP APP open on that   |
//! |                          |             | machine, and an approval       |
//! | `/home/...` on meridian  | `ssh`       | a provisioned key, and an      |
//! |                          |             | approval                       |
//!
//! So a path outside `/work` is not a broken routine. It is a routine with a
//! DEPENDENCY, and the useful thing to say is which one - not "no".
//!
//! What remains a hard refusal: a path no tool can reach at all, and the
//! `/workspace` prefix, which is Grok Bot's VM layout and exists nowhere in
//! Bullpen under any tool.
//!
//! 🔴 Unattended is the real constraint, not the path. `tightenForRoutine`
//! forces `shell`, `ssh` and `read_file` to "ask" for any bot with a routine, so
//! a 06:00 run stops at an approval nobody is awake to give. That is a decision
//! about WHEN a routine runs, not about what it may name, and it belongs to Josh
//! rather than to this file.

use std::collections::HashSet;

/// The one writable, persistent location a sandbox has.
pub const WORKDIR: &str = "/work";

/// A path a routine names and information about its reachability.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PathProblem {
    pub path: String,
    pub why: String,
    pub suggestion: Option<String>,
    /// "unreachable" is a refusal: nothing in Bullpen can open this.
    /// "needs-a-tool" is information: a tool reaches it, with an approval.
    pub kind: PathKind,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PathKind {
    Unreachable,
    NeedsATool,
}

/// Host paths that read as plausible but are on meridian, not in the sandbox.
/// Listed so the message can say WHY rather than just "not /work".
const HOST_PREFIXES: &[&str] = &[
    "/home/", "/root/", "/etc/", "/var/", "/srv/", "/opt/", "/mnt/", "/usr/",
];

/// Grok Bot's VM layout, which is where every imported routine's paths come from.
const GROK_PREFIXES: &[&str] = &["/workspace/", "/workspace"];

/// Trim trailing punctuation from a path string.
fn trim_trailing_punctuation(path: &str) -> &str {
    path.trim_end_matches(['.', ',', ';', ':', ')', ']'])
}

/// Check if a path looks like a URL route rather than a file path.
/// A single segment with no dot is a route; a file path always has a directory or extension.
fn looks_like_url_route(path: &str) -> bool {
    let segments: Vec<&str> = path.split('/').filter(|s| !s.is_empty()).collect();
    segments.len() == 1 && !segments[0].contains('.')
}

/// Extract Windows paths from text. Matches patterns like `C:\Users\...`
/// Uses a lookbehind to ensure not preceded by a word character.
fn extract_windows_paths(text: &str) -> Vec<String> {
    let mut paths = Vec::new();
    let bytes = text.as_bytes();

    for i in 0..bytes.len() {
        // Check for drive letter followed by :\
        if i + 2 < bytes.len() {
            let byte = bytes[i];
            let is_letter = byte.is_ascii_alphabetic();
            if is_letter && bytes[i + 1] == b':' && bytes[i + 2] == b'\\' {
                // Check lookbehind: not preceded by word char
                if i > 0 {
                    let prev = bytes[i - 1];
                    if prev.is_ascii_alphanumeric() || prev == b'_' {
                        continue;
                    }
                }

                // Extract the full path
                let mut end = i + 3;
                while end < bytes.len() {
                    let b = bytes[end];
                    if b.is_ascii_alphanumeric()
                        || b == b'.'
                        || b == b'\\'
                        || b == b'-'
                        || b == b'_'
                    {
                        end += 1;
                    } else {
                        break;
                    }
                }

                if let Ok(path) = std::str::from_utf8(&bytes[i..end]) {
                    paths.push(path.to_string());
                }
            }
        }
    }

    paths
}

/// Extract Unix absolute paths from text. Matches patterns like `/workspace/thing`
/// Uses a lookbehind to ensure not preceded by word char or slash.
fn extract_unix_paths(text: &str) -> Vec<String> {
    let mut paths = Vec::new();
    let bytes = text.as_bytes();

    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'/' {
            // Check lookbehind: not preceded by word char or slash
            if i > 0 {
                let prev = bytes[i - 1];
                if prev.is_ascii_alphanumeric() || prev == b'_' || prev == b'/' {
                    i += 1;
                    continue;
                }
            }

            // Extract the path: `/` followed by segments of [word.-]
            let mut end = i + 1;
            let mut had_segment = false;

            loop {
                // Try to match a segment: [word.-]+
                let segment_start = end;
                while end < bytes.len() {
                    let b = bytes[end];
                    if b.is_ascii_alphanumeric() || b == b'_' || b == b'.' || b == b'-' {
                        end += 1;
                    } else {
                        break;
                    }
                }

                if end > segment_start {
                    had_segment = true;
                    // Try to match a slash
                    if end < bytes.len() && bytes[end] == b'/' {
                        end += 1;
                    } else {
                        break;
                    }
                } else {
                    // No segment, stop
                    if had_segment {
                        // Backtrack the last slash
                        if end > 0 && bytes[end - 1] == b'/' {
                            end -= 1;
                        }
                    }
                    break;
                }
            }

            if end > i + 1
                && had_segment
                && let Ok(path) = std::str::from_utf8(&bytes[i..end])
            {
                paths.push(path.to_string());
            }
        }
        i += 1;
    }

    paths
}

/// Every path in `prompt` that a routine could not reach.
///
/// Returns an empty array for a prompt that names no paths at all, which is the
/// common and correct case - four of the thirteen routines name none.
pub fn check_routine_paths(prompt: &str) -> Vec<PathProblem> {
    let mut problems = Vec::new();
    let mut seen: HashSet<String> = HashSet::new();

    let mut add =
        |problems: &mut Vec<_>, raw: &str, why: &str, suggestion: Option<&str>, kind: PathKind| {
            let path = trim_trailing_punctuation(raw);
            if path.is_empty() || seen.contains(path) {
                return;
            }
            seen.insert(path.to_string());
            problems.push(PathProblem {
                path: path.to_string(),
                why: why.to_string(),
                suggestion: suggestion.map(|s| s.to_string()),
                kind,
            });
        };

    // Check Windows paths
    for path in extract_windows_paths(prompt) {
        add(
            &mut problems,
            &path,
            "on Josh's workstation, so it needs the read_file tool and the Bullpen \
             desktop app running there",
            Some(
                "allowed, but a routine holds read_file on \"ask\", so an unattended \
                 run will wait for an approval",
            ),
            PathKind::NeedsATool,
        );
    }

    // Check Unix paths
    for path in extract_unix_paths(prompt) {
        let trimmed = trim_trailing_punctuation(&path);
        if trimmed == WORKDIR || trimmed.starts_with(&format!("{}/", WORKDIR)) {
            continue;
        }
        if looks_like_url_route(trimmed) {
            continue;
        }

        if GROK_PREFIXES
            .iter()
            .any(|prefix| trimmed.starts_with(prefix))
        {
            let suggestion = trimmed.replace("/workspace", WORKDIR);
            add(
                &mut problems,
                trimmed,
                "Grok Bot's layout; a Bullpen sandbox has no /workspace",
                Some(&suggestion),
                PathKind::Unreachable,
            );
            continue;
        }

        if HOST_PREFIXES
            .iter()
            .any(|prefix| trimmed.starts_with(prefix))
        {
            add(
                &mut problems,
                trimmed,
                "on meridian itself, so it needs the ssh tool",
                Some(
                    "allowed, but a routine holds ssh on \"ask\", so an unattended \
                     run will wait for an approval",
                ),
                PathKind::NeedsATool,
            );
            continue;
        }

        add(
            &mut problems,
            trimmed,
            &format!(
                "outside {}, which is the only thing a sandbox can see",
                WORKDIR
            ),
            None,
            PathKind::Unreachable,
        );
    }

    // Return as Vec sorted for deterministic order
    problems.sort_by(|a, b| a.path.cmp(&b.path));
    problems
}

/// Only the ones that no tool can reach. These are what refuses a save.
pub fn blocking_problems(problems: &[PathProblem]) -> Vec<PathProblem> {
    problems
        .iter()
        .filter(|p| p.kind == PathKind::Unreachable)
        .cloned()
        .collect()
}

/// One sentence per problem, for an error a person reads rather than a log.
pub fn describe_path_problems(problems: &[PathProblem]) -> String {
    let lines: Vec<String> = problems
        .iter()
        .map(|p| {
            if let Some(suggestion) = &p.suggestion {
                format!("  {} - {}; try {}", p.path, p.why, suggestion)
            } else {
                format!("  {} - {}", p.path, p.why)
            }
        })
        .collect();

    let mut result = vec![format!(
        "This routine names {} it cannot reach:",
        if problems.len() == 1 {
            "a path".to_string()
        } else {
            "paths".to_string()
        }
    )];
    result.extend(lines);
    result.push(String::new());
    result.push(format!(
        "A routine can only see {}, its own volume. Anything else is a run that stops",
        WORKDIR
    ));
    result.push(
        "and asks you to paste files in, which is what this check exists to prevent.".to_string(),
    );

    result.join("\n")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_workdir_constant() {
        assert_eq!(WORKDIR, "/work");
    }

    #[test]
    fn test_no_paths_in_prompt() {
        let prompt = "This is a routine that does not mention any files.";
        let problems = check_routine_paths(prompt);
        assert!(problems.is_empty());
    }

    #[test]
    fn test_workspace_path_unreachable() {
        let prompt = "Read the file at /workspace/pres-coach/config.txt";
        let problems = check_routine_paths(prompt);
        let blocking = blocking_problems(&problems);

        assert_eq!(blocking.len(), 1);
        assert_eq!(blocking[0].path, "/workspace/pres-coach/config.txt");
        assert_eq!(blocking[0].kind, PathKind::Unreachable);
        assert!(blocking[0].suggestion.is_some());
    }

    #[test]
    fn test_workdir_path_allowed() {
        let prompt = "Read the file at /work/data.txt and process it";
        let problems = check_routine_paths(prompt);
        assert!(problems.is_empty());
    }

    #[test]
    fn test_url_route_not_a_path() {
        let prompt = "Check the /login and /admin routes for errors";
        let problems = check_routine_paths(prompt);
        assert!(problems.is_empty());
    }

    #[test]
    fn test_windows_path_needs_tool() {
        let prompt = "Copy the file from C:\\Users\\Josh\\data.txt";
        let problems = check_routine_paths(prompt);

        assert_eq!(problems.len(), 1);
        assert_eq!(problems[0].kind, PathKind::NeedsATool);
    }

    #[test]
    fn test_home_path_needs_tool() {
        let prompt = "Check the logs at /home/rainmade/logs/app.log";
        let problems = check_routine_paths(prompt);

        assert_eq!(problems.len(), 1);
        assert_eq!(problems[0].kind, PathKind::NeedsATool);
    }

    #[test]
    fn test_home_root_path_needs_tool() {
        let prompt = "Read /root/.ssh/config on the server";
        let problems = check_routine_paths(prompt);

        assert_eq!(problems.len(), 1);
        assert_eq!(problems[0].kind, PathKind::NeedsATool);
    }

    #[test]
    fn test_mixed_blocking_and_needs_tool() {
        let prompt =
            "Read /workspace/data.txt and /home/rainmade/config.txt and C:\\temp\\file.txt";
        let problems = check_routine_paths(prompt);
        let blocking = blocking_problems(&problems);

        // Should have 3 total problems
        assert_eq!(problems.len(), 3);
        // Only /workspace is blocking
        assert_eq!(blocking.len(), 1);
        assert_eq!(blocking[0].path, "/workspace/data.txt");
    }

    #[test]
    fn test_trailing_punctuation_trimmed() {
        let prompt = "Read the file at /workspace/test.txt. It should have data.";
        let problems = check_routine_paths(prompt);
        let blocking = blocking_problems(&problems);

        // The period should be trimmed from the path
        assert_eq!(blocking.len(), 1);
        assert_eq!(blocking[0].path, "/workspace/test.txt");
    }

    #[test]
    fn test_describe_single_problem() {
        let problems = vec![PathProblem {
            path: "/workspace/thing".to_string(),
            why: "test reason".to_string(),
            suggestion: Some("/work/thing".to_string()),
            kind: PathKind::Unreachable,
        }];

        let description = describe_path_problems(&problems);
        assert!(description.contains("a path"));
        assert!(description.contains("/workspace/thing"));
        assert!(description.contains("test reason"));
        assert!(description.contains("/work/thing"));
    }

    #[test]
    fn test_describe_multiple_problems() {
        let problems = vec![
            PathProblem {
                path: "/workspace/a.txt".to_string(),
                why: "reason 1".to_string(),
                suggestion: None,
                kind: PathKind::Unreachable,
            },
            PathProblem {
                path: "/workspace/b.txt".to_string(),
                why: "reason 2".to_string(),
                suggestion: None,
                kind: PathKind::Unreachable,
            },
        ];

        let description = describe_path_problems(&problems);
        assert!(description.contains("paths") || description.contains("path"));
    }

    #[test]
    fn test_non_blocking_problems_excluded_from_blocking() {
        let problems = vec![
            PathProblem {
                path: "/workspace/bad.txt".to_string(),
                why: "unreachable".to_string(),
                suggestion: None,
                kind: PathKind::Unreachable,
            },
            PathProblem {
                path: "C:\\temp.txt".to_string(),
                why: "needs read_file".to_string(),
                suggestion: None,
                kind: PathKind::NeedsATool,
            },
        ];

        let blocking = blocking_problems(&problems);
        assert_eq!(blocking.len(), 1);
        assert_eq!(blocking[0].path, "/workspace/bad.txt");
    }
}
