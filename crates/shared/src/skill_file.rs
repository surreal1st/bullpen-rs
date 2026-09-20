//! Reading a Claude Code `SKILL.md`. Port of `skillFile.ts`.

/// Parsed front matter and body from a skill file.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParsedSkillFile {
    pub name: String,
    pub description: String,
    pub body: String,
}

const FOLD_MARKERS: &[&str] = &[">", "|", ">-", "|-", ">+", "|+"];

/// Parse YAML front matter between `---` fences. Returns `None` when absent.
pub fn parse_skill_file(text: &str) -> Option<ParsedSkillFile> {
    let text = text.strip_prefix('\u{feff}').unwrap_or(text);
    let (front, body) = split_front_matter(text)?;

    let lines: Vec<&str> = front.split('\n').collect();
    let mut fields: Vec<(String, String)> = Vec::new();
    let mut i = 0;
    while i < lines.len() {
        let line = lines[i].trim_end_matches('\r');
        if let Some((key, rest)) = parse_key_value(line) {
            let mut value = rest.trim().to_string();
            if FOLD_MARKERS.contains(&value.as_str()) {
                let mut folded = Vec::new();
                while i + 1 < lines.len() {
                    let next = lines[i + 1].trim_end_matches('\r');
                    if next.starts_with(' ') || next.starts_with('\t') {
                        if !next.trim().is_empty() {
                            folded.push(next.trim().to_string());
                        }
                        i += 1;
                    } else {
                        break;
                    }
                }
                value = folded.join(" ");
            } else {
                value = strip_quotes(&value);
            }
            fields.push((key, value));
        }
        i += 1;
    }

    let name = fields
        .iter()
        .find(|(k, _)| k == "name")
        .map(|(_, v)| v.clone())
        .unwrap_or_default();
    let description = fields
        .iter()
        .find(|(k, _)| k == "description")
        .map(|(_, v)| v.clone())
        .unwrap_or_default();

    Some(ParsedSkillFile {
        name,
        description,
        body: body.trim().to_string(),
    })
}

fn split_front_matter(text: &str) -> Option<(String, String)> {
    let normalized = text.replace("\r\n", "\n");
    if !normalized.starts_with("---\n") {
        return None;
    }
    let rest = &normalized[4..];
    let end = rest.find("\n---")?;
    let front = rest[..end].to_string();
    let after = &rest[end + 4..];
    let body = after.strip_prefix('\n').unwrap_or(after).to_string();
    Some((front, body))
}

fn parse_key_value(line: &str) -> Option<(String, String)> {
    let line = line.trim_end_matches('\r');
    if line.is_empty() {
        return None;
    }
    let first = line.chars().next()?;
    if !first.is_ascii_alphabetic() {
        return None;
    }
    let colon = line.find(':')?;
    let key = &line[..colon];
    if key.is_empty()
        || !key
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
    {
        return None;
    }
    let value = line[colon + 1..].to_string();
    Some((key.to_string(), value))
}

fn strip_quotes(s: &str) -> String {
    let s = s.trim();
    if (s.starts_with('"') && s.ends_with('"')) || (s.starts_with('\'') && s.ends_with('\'')) {
        s[1..s.len() - 1].to_string()
    } else {
        s.to_string()
    }
}
