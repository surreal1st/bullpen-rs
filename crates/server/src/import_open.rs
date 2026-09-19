//! IMPORT-01: importing a bot from the open formats already on Josh's
//! machine, plus our own export format (`routes/bots.rs::export_bot`). Port
//! of `projects/bullpen-night/src/server/import-open.ts` (whole file) - pure
//! parsing only, no db, no HTTP, no `.await`. `routes/import.rs` wires this
//! to the two routes and does the db work and the model judging.
//!
//! Three real source shapes, all ported verbatim from the TS's own doc
//! comment:
//!
//! - **Agent Skills** (`SKILL.md`): a published spec. YAML frontmatter with
//!   `name` and `description`, then Markdown instructions.
//! - **Claude Code subagents** (`.claude/agents/*.md`): the same shape by
//!   convention, plus an optional `tools` list.
//! - **AGENTS.md**: no frontmatter at all. The whole file is the
//!   instructions.
//!
//! There is no cross-vendor standard that covers instructions, memory, tools
//! and a model pin together, so nothing here pretends to import those.
//!
//! ## Three deliberate divergences from the TS
//!
//! **(a) A frontmatter value that parses as a JSON string is JSON-decoded.**
//! Our own export route (`routes/bots.rs::export_bot`) writes every
//! frontmatter value through `serde_json::to_string`, so a name containing a
//! quote or a newline arrives here as `name: "Ada \"Q\" Lovelace"`. The TS
//! only strips a matching pair of surrounding quotes, which would import
//! that name with the backslash escapes still literally in it. Here,
//! `serde_json::from_str::<String>(value)` is tried FIRST; only on failure
//! does this fall back to the TS's own strip-matching-surrounding-quotes
//! behaviour (`"` or `'`), so hand-written YAML (which never JSON-encodes
//! anything) still reads exactly as it did before.
//!
//! **(b) An explicit frontmatter `name` is used verbatim (trimmed), NOT
//! title-cased.** The TS title-cases every name unconditionally, which would
//! turn our own exported `iOS Watcher` into `IOS Watcher` and collapse
//! multiple internal spaces - a bot's name would silently change every time
//! it round-tripped through a file. Title-casing is kept for a name that had
//! to be DERIVED (from a `# Heading` in the body, or from the filename)
//! because that path never carries a name Bullpen itself chose - a source
//! file called `my-cool-bot.md` should still become `My Cool Bot`, and that
//! is the TS's own behaviour for that case.
//!
//! **(c) A frontmatter `model:` is HONOURED.** The TS deliberately ignores
//! it, because none of the three foreign formats it was written for ever
//! carries a model - but our own export writes one, and silently dropping it
//! on import means a bot would come back running on a different model than
//! its own file names. So `model` is `Some` only when the key is present and
//! its value is a non-empty string, matching `routes/bots.rs::create_bot`'s
//! own `Some(Value::String(s)) if !s.is_empty()` shape. It is judged in
//! `routes/import.rs`, never here - this module is pure parsing with no
//! `.await`.

use std::collections::BTreeMap;
use std::sync::OnceLock;

use regex::Regex;
use serde::Serialize;

/// Which of the three real shapes (or our own export) a file matched.
/// `serde` renames match the TS's own string union exactly:
/// `"skill" | "subagent" | "agents-md" | "unknown"`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub enum OpenFormat {
    #[serde(rename = "skill")]
    Skill,
    #[serde(rename = "subagent")]
    Subagent,
    #[serde(rename = "agents-md")]
    AgentsMd,
    #[serde(rename = "unknown")]
    Unknown,
}

/// What a source file yields, before anything touches the database.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ParsedOpenBot {
    pub format: OpenFormat,
    pub name: String,
    pub purpose: String,
    pub instructions: String,
    /// NEW vs the TS - see divergence (c) in this module's own doc comment.
    /// `None` unless the frontmatter carried a non-empty `model:` value.
    pub model: Option<String>,
    /// Named in the source but not honoured, because Bullpen's tools and
    /// permissions are its own.
    pub declared_tools: Vec<String>,
    pub warnings: Vec<String>,
}

/// The result of splitting a file into its `key: value` header and body.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Frontmatter {
    pub fields: BTreeMap<String, String>,
    pub body: String,
    pub had: bool,
}

fn frontmatter_line_pattern() -> &'static Regex {
    static PATTERN: OnceLock<Regex> = OnceLock::new();
    PATTERN.get_or_init(|| {
        Regex::new(r"^([A-Za-z0-9_-]+)\s*:\s*(.*)$").expect("frontmatter line pattern must compile")
    })
}

fn heading_pattern() -> &'static Regex {
    static PATTERN: OnceLock<Regex> = OnceLock::new();
    PATTERN.get_or_init(|| Regex::new(r"(?m)^#\s+(.+)$").expect("heading pattern must compile"))
}

/// A deliberately small YAML reader: `key: value` and simple inline lists.
///
/// Pulling in a YAML parser to read three known keys is a dependency bought
/// for nothing. Anything it cannot read is reported rather than guessed at.
/// Port of the TS `readFrontmatter` (`import-open.ts:47-75`), byte for byte:
/// a leading BOM is stripped; no leading `---`, or no closing `\n---`, means
/// "no frontmatter, the whole text is the body" (returned WITHOUT trimming,
/// exactly like the TS's own early returns); each header line is trimmed
/// before the `^([A-Za-z0-9_-]+)\s*:\s*(.*)$` match (which is also how CRLF
/// is handled here - `str::trim` strips a trailing `\r` the same way the TS
/// `.trim()` call does), an unparseable line is skipped rather than guessed
/// at, and the key is lowercased. Only when frontmatter IS found is the body
/// trimmed, matching the TS's own `body: body.trim()` on that one path.
pub fn read_frontmatter(text: &str) -> Frontmatter {
    let normalised = text.strip_prefix('\u{FEFF}').unwrap_or(text);
    if !normalised.starts_with("---") {
        return Frontmatter {
            fields: BTreeMap::new(),
            body: normalised.to_string(),
            had: false,
        };
    }

    // `normalised[3..]` is safe: `starts_with("---")` just proved the first
    // three bytes are the ASCII string "---", so byte index 3 is a char
    // boundary.
    let Some(rel_end) = normalised[3..].find("\n---") else {
        return Frontmatter {
            fields: BTreeMap::new(),
            body: normalised.to_string(),
            had: false,
        };
    };
    let end = 3 + rel_end;
    let head = &normalised[3..end];
    let after = &normalised[end + 4..];
    let body = after
        .strip_prefix("\r\n")
        .or_else(|| after.strip_prefix('\n'))
        .unwrap_or(after);

    let mut fields = BTreeMap::new();
    for line in head.split('\n') {
        let trimmed = line.trim();
        if let Some(caps) = frontmatter_line_pattern().captures(trimmed) {
            let key = caps[1].to_lowercase();
            let value = decode_frontmatter_value(caps[2].trim());
            fields.insert(key, value);
        }
    }

    Frontmatter {
        fields,
        body: body.trim().to_string(),
        had: true,
    }
}

/// Divergence (a): try a real JSON string decode first (what our own export
/// writes), and only on failure fall back to the TS's own
/// strip-matching-surrounding-quotes behaviour.
///
/// IMPORT-01a boundary audit: `value[1..bytes_len - 1]` below is safe by
/// construction, unlike `strip_md_extension`'s old bug (see that function's
/// own doc). `starts_with('"')`/`ends_with('"')` (and the `'\''` pair) match
/// a single `char`, and both candidate quote characters are one byte in
/// UTF-8 - so `starts_with` returning true guarantees byte index 1 is a
/// boundary (it sits right after a one-byte first character), and
/// `ends_with` returning true guarantees `bytes_len - 1` is a boundary (it
/// sits right before a one-byte last character), for ANY content in
/// between, multi-byte or not. The `bytes_len >= 2` guard only rules out
/// the single-character case where both checks trivially match the same
/// byte. No slicing here can land inside a multi-byte character; left as-is.
fn decode_frontmatter_value(value: &str) -> String {
    if let Ok(decoded) = serde_json::from_str::<String>(value) {
        return decoded;
    }
    let bytes_len = value.len();
    if bytes_len >= 2
        && ((value.starts_with('"') && value.ends_with('"'))
            || (value.starts_with('\'') && value.ends_with('\'')))
    {
        return value[1..bytes_len - 1].to_string();
    }
    value.to_string()
}

/// Port of the TS `detectFormat` (`import-open.ts:77-90`), rule for rule and
/// in the same order: the basename (lowercased, whichever slash style)
/// decides `agents.md`/`skill.md` outright, BEFORE anything about the
/// frontmatter is even looked at; only then does a `description` in the
/// frontmatter (with or without `tools`) or its total absence decide the
/// rest.
pub fn detect_format(file_name: &str, text: &str) -> OpenFormat {
    let lowered = file_name.to_lowercase();
    let base = lowered.rsplit(['\\', '/']).next().unwrap_or("");
    let front = read_frontmatter(text);

    if base == "agents.md" {
        return OpenFormat::AgentsMd;
    }
    if base == "skill.md" {
        return OpenFormat::Skill;
    }
    if front.had && front.fields.contains_key("description") {
        return if front.fields.contains_key("tools") {
            OpenFormat::Subagent
        } else {
            OpenFormat::Skill
        };
    }
    if !front.had {
        return OpenFormat::AgentsMd;
    }
    OpenFormat::Unknown
}

/// Port of the TS `parseOpenBot` (`import-open.ts:92-124`). See this
/// module's own top doc comment for the three points where this
/// deliberately diverges.
pub fn parse_open_bot(file_name: &str, text: &str) -> ParsedOpenBot {
    let format = detect_format(file_name, text);
    let front = read_frontmatter(text);
    let mut warnings = Vec::new();

    let declared_tools: Vec<String> = front
        .fields
        .get("tools")
        .map(String::as_str)
        .unwrap_or("")
        .trim_start_matches('[')
        .trim_end_matches(']')
        .split(',')
        .map(str::trim)
        .filter(|t| !t.is_empty())
        .map(str::to_string)
        .collect();

    // Divergence (b): an explicit, non-empty `name:` is kept exactly as
    // written (just trimmed) - title-casing only ever applies to a name
    // this function had to DERIVE, matching the TS's own titleCase call on
    // that path.
    let front_name = front.fields.get("name").cloned().unwrap_or_default();
    let name = if !front_name.is_empty() {
        front_name.trim().to_string()
    } else {
        let derived = first_heading(&front.body).unwrap_or_else(|| file_name_to_name(file_name));
        title_case(&derived)
    };

    let purpose = match front.fields.get("description") {
        Some(description) => description.clone(),
        None => first_line(&front.body),
    };
    let instructions = front.body.trim().to_string();

    // Divergence (c): honoured here, judged later (in routes/import.rs,
    // which has the catalogue and can `.await`).
    let model = match front.fields.get("model") {
        Some(m) if !m.is_empty() => Some(m.clone()),
        _ => None,
    };

    if instructions.is_empty() {
        warnings.push("the file has no instructions in it".to_string());
    }
    if !declared_tools.is_empty() {
        warnings.push(format!(
            "it names tools ({}), which are not imported: Bullpen's tools and permissions are its own",
            declared_tools.join(", ")
        ));
    }
    if format == OpenFormat::Unknown {
        warnings.push(
            "the format was not recognised, so the whole file was taken as instructions"
                .to_string(),
        );
    }

    ParsedOpenBot {
        format,
        name,
        purpose,
        instructions,
        model,
        declared_tools,
        warnings,
    }
}

/// Port of the TS `firstLine` (`import-open.ts:171-174`). Deliberately
/// checks the RAW line (not the trimmed one) for a leading `#`, same as the
/// TS `!l.startsWith("#")` - a line indented before its `#` is not excluded
/// by this check, matching the source exactly rather than "fixing" it.
fn first_line(text: &str) -> String {
    let line = text
        .split('\n')
        .find(|l| !l.trim().is_empty() && !l.starts_with('#'))
        .map(|l| l.trim().to_string())
        .unwrap_or_default();
    if line.chars().count() > 120 {
        let truncated: String = line.chars().take(117).collect();
        format!("{truncated}...")
    } else {
        line
    }
}

/// The first `# Heading` line anywhere in the body, trimmed - port of the
/// TS's `/^#\s+(.+)$/m.exec(front.body)?.[1]?.trim()`.
fn first_heading(body: &str) -> Option<String> {
    heading_pattern()
        .captures(body)
        .map(|caps| caps[1].trim().to_string())
}

/// Port of the TS `fileNameToName` (`import-open.ts:176-179`).
fn file_name_to_name(file_name: &str) -> String {
    let base = file_name.rsplit(['\\', '/']).next().unwrap_or("bot");
    let without_ext = strip_md_extension(base);
    collapse_dashes_and_underscores(&without_ext)
}

/// IMPORT-01a/F1: the suffix is tested with `ends_with` (which walks
/// `.chars()`-safe and cannot panic on a non-boundary offset) BEFORE any
/// slicing happens - only once that proves the string truly ends with
/// `.md` is `len - 3` known to land on a char boundary (a valid ASCII
/// suffix can only be preceded by a boundary). The previous version sliced
/// `name[name.len() - 3..]` FIRST to find out whether it equalled `.md`,
/// which panics outright when the third-from-last byte falls inside a
/// multi-byte character instead of at a char boundary - reachable directly
/// from an untrusted `fileName` in the request body (see
/// `crates/server/tests/import_open.rs`'s own regression test for this).
fn strip_md_extension(name: &str) -> String {
    let lowered = name.to_ascii_lowercase();
    if lowered.ends_with(".md") {
        name[..name.len() - 3].to_string()
    } else {
        name.to_string()
    }
}

fn collapse_dashes_and_underscores(s: &str) -> String {
    let mut result = String::new();
    let mut prev_was_sep = false;
    for ch in s.chars() {
        if ch == '-' || ch == '_' {
            if !prev_was_sep {
                result.push(' ');
            }
            prev_was_sep = true;
        } else {
            result.push(ch);
            prev_was_sep = false;
        }
    }
    result
}

/// Port of the TS `titleCase` (`import-open.ts:181-187`): split on
/// whitespace/`-`/`_`, capitalise only the first character of each word
/// (the rest is left exactly as written), join with single spaces.
fn title_case(text: &str) -> String {
    text.split(|c: char| c.is_whitespace() || c == '-' || c == '_')
        .filter(|w| !w.is_empty())
        .map(|w| {
            let mut chars = w.chars();
            match chars.next() {
                Some(first) => first.to_uppercase().collect::<String>() + chars.as_str(),
                None => String::new(),
            }
        })
        .collect::<Vec<_>>()
        .join(" ")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn no_frontmatter_is_the_whole_text_as_body() {
        let front = read_frontmatter("Just plain instructions.\nSecond line.");
        assert!(!front.had);
        assert!(front.fields.is_empty());
        assert_eq!(front.body, "Just plain instructions.\nSecond line.");
    }

    #[test]
    fn a_leading_dashes_with_no_closing_dashes_is_no_frontmatter() {
        let text = "---\nname: Never Closed\nstill going";
        let front = read_frontmatter(text);
        assert!(!front.had);
        assert!(front.fields.is_empty());
        assert_eq!(front.body, text);
    }

    #[test]
    fn a_leading_bom_is_stripped() {
        let text = "\u{FEFF}No frontmatter here.";
        let front = read_frontmatter(text);
        assert!(!front.had);
        assert_eq!(front.body, "No frontmatter here.");
    }

    #[test]
    fn a_bom_before_real_frontmatter_still_parses() {
        let text = "\u{FEFF}---\nname: Bommed\n---\nBody.";
        let front = read_frontmatter(text);
        assert!(front.had);
        assert_eq!(front.fields.get("name").unwrap(), "Bommed");
        assert_eq!(front.body, "Body.");
    }

    #[test]
    fn crlf_line_endings_parse_the_same_as_lf() {
        let text =
            "---\r\nname: CRLF Bot\r\ndescription: has crlf\r\n---\r\n\r\nInstructions here.\r\n";
        let front = read_frontmatter(text);
        assert!(front.had);
        assert_eq!(front.fields.get("name").unwrap(), "CRLF Bot");
        assert_eq!(front.fields.get("description").unwrap(), "has crlf");
        assert_eq!(front.body, "Instructions here.");
    }

    #[test]
    fn single_and_double_quoted_values_are_unwrapped() {
        let text = "---\nname: 'Single Quoted'\ndescription: \"Double Quoted\"\n---\nBody.";
        let front = read_frontmatter(text);
        assert_eq!(front.fields.get("name").unwrap(), "Single Quoted");
        assert_eq!(front.fields.get("description").unwrap(), "Double Quoted");
    }

    #[test]
    fn a_json_escaped_value_with_a_quote_and_a_newline_is_json_decoded() {
        // Divergence (a): what our own export route writes for a name
        // containing a literal quote and an embedded newline.
        let text = "---\nname: \"Ada \\\"Q\\\" Lovelace\\nSecond Line\"\n---\nBody.";
        let front = read_frontmatter(text);
        assert_eq!(
            front.fields.get("name").unwrap(),
            "Ada \"Q\" Lovelace\nSecond Line"
        );
    }

    #[test]
    fn an_unparseable_line_is_skipped_not_guessed_at() {
        let text =
            "---\nname: Real Bot\nthis line has no colon at all\ndescription: fine\n---\nBody.";
        let front = read_frontmatter(text);
        assert_eq!(front.fields.len(), 2);
        assert_eq!(front.fields.get("name").unwrap(), "Real Bot");
        assert_eq!(front.fields.get("description").unwrap(), "fine");
    }
}
