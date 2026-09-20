//! Can a Bullpen bot actually follow this skill? Port of `skillFit.ts`.

use regex::Regex;
use std::sync::LazyLock;

/// What a Bullpen bot cannot do, expressed as the marker that gives it away.
struct Marker {
    re: Regex,
    lacks: &'static str,
}

static MARKERS: LazyLock<Vec<Marker>> = LazyLock::new(|| {
    let specs: &[(&str, &str)] = &[
        (
            r"[A-Za-z]:\\[\\\w. -]|[A-Za-z]:/(?:Users|Program Files|rainmade)\b",
            "a path on Josh's own machine",
        ),
        (
            r"(?i)~\/\.claude\b|%APPDATA%|\bStart-Process\b",
            "Claude Code's own files on the workstation",
        ),
        (
            r"\bpython3?\s+[\w./-]+\.py\b|\bpip\s+install\b",
            "python (the sandbox is alpine, it has none)",
        ),
        (
            r"(?im)(?:^|[\s`(])node\s+[\w./-]+\.(?:mjs|cjs|js|ts)\b",
            "node in the sandbox (it is alpine, busybox only)",
        ),
        (r"\bnpm\s+(?:run|install|ci|test|exec)\b|\bnpx\s+\S", "npm"),
        (
            r"\bpuppeteer\b|\bheadless\s+(?:edge|chrome|chromium)\b",
            "a headless browser it can drive from the sandbox",
        ),
        (
            r"\byt-dlp\b|\bwhisper\b|\bffmpeg\b",
            "media tooling (yt-dlp, ffmpeg, whisper)",
        ),
        (
            r"\bgh\s+(?:issue|pr|secret|variable|repo|api|auth)\b",
            "the gh CLI",
        ),
        (
            r"\bgit\s+(?:worktree|clone|rebase|cherry-pick|bisect)\b",
            "a git checkout to work in",
        ),
        (
            r"\b(?:sub-?agents?|Task tool|Agent tool)\b",
            "subagents - it cannot spawn another agent",
        ),
        (
            r"\b(?:TodoWrite|ExitPlanMode|NotebookEdit)\b",
            "that Claude Code tool",
        ),
        (r"\.scratch[\\/]", "the .scratch ticket tracker"),
    ];
    specs
        .iter()
        .map(|(pat, lacks)| Marker {
            re: Regex::new(pat).expect("marker regex"),
            lacks,
        })
        .collect()
});

/// Fit assessment for a skill body against shipped sibling files.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SkillFit {
    /// Files named by the body that did not ship beside the skill.
    pub missing: Vec<String>,
    /// Capabilities the body assumes and a Bullpen bot does not have.
    pub lacks: Vec<String>,
}

pub fn assess_skill(body: &str, siblings: &[String]) -> SkillFit {
    let missing: Vec<String> = siblings
        .iter()
        .filter(|rel| names_file(body, rel))
        .cloned()
        .collect();

    let mut lacks = Vec::new();
    for marker in MARKERS.iter() {
        if marker.re.is_match(body) && !lacks.iter().any(|l| l == marker.lacks) {
            lacks.push(marker.lacks.to_string());
        }
    }

    SkillFit { missing, lacks }
}

fn names_file(body: &str, relative_path: &str) -> bool {
    let base = relative_path.rsplit('/').next().unwrap_or(relative_path);
    let forms: Vec<&str> = if relative_path == base {
        vec![base]
    } else {
        vec![relative_path, base]
    };

    forms.iter().any(|form| body_names_file(body, form))
}

/// Same boundary rules as TS `(?<![\w.])…(?!\w)` without lookbehind (portable).
fn body_names_file(body: &str, needle: &str) -> bool {
    if needle.is_empty() {
        return false;
    }
    let bytes = body.as_bytes();
    let mut start = 0;
    while let Some(rel) = body[start..].find(needle) {
        let at = start + rel;
        let before_ok = at == 0 || !is_word_or_dot_byte(bytes[at - 1]);
        let after = at + needle.len();
        let after_ok = after >= bytes.len() || !is_word_byte(bytes[after]);
        if before_ok && after_ok {
            return true;
        }
        start = at + 1;
    }
    false
}

fn is_word_or_dot_byte(b: u8) -> bool {
    b.is_ascii_alphanumeric() || b == b'_' || b == b'.'
}

fn is_word_byte(b: u8) -> bool {
    b.is_ascii_alphanumeric() || b == b'_'
}

/// Block prepended when the skill assumes capabilities the bot lacks.
pub fn capability_note(lacks: &[String]) -> String {
    if lacks.is_empty() {
        return String::new();
    }

    let mut lines = vec![
        "> **Bullpen note.** This skill was written for Claude Code, not for you.".to_string(),
        "> Following it here, you do NOT have:".to_string(),
    ];
    for l in lacks {
        lines.push(format!("> - {l}"));
    }
    lines.push(">".to_string());
    lines.push(
        "> Do the parts you can do with the tools you actually have. If a step needs".to_string(),
    );
    lines.push(
        "> one of the above, say plainly that you cannot do that step and stop there.".to_string(),
    );
    lines.push("> Never describe having done it.".to_string());
    lines.push(String::new());
    lines.join("\n")
}

/// Body as stored: annotated when the skill assumes too much.
pub fn body_for_bullpen(body: &str, fit: &SkillFit) -> String {
    let note = capability_note(&fit.lacks);
    if note.is_empty() {
        body.to_string()
    } else {
        format!("{note}\n{body}")
    }
}
