//! TypeSafe Jev (Choice / Score) — server-side only. Key from env / key file,
//! never exposed to bots or clients. S9-JEV-01 helper kind gate; S9-JEV-02 grep re-rank.

use std::collections::HashMap;
use std::time::Duration;

use reqwest::header::{AUTHORIZATION, CONTENT_TYPE, HeaderMap, HeaderValue};
use serde_json::{Value, json};

pub const SYSTEMONE_URL: &str = "https://api.typesafe.ai/v1/systemone";
pub const DEFAULT_MODEL: &str = "jev-1.13.0";
pub const KEY_ENV: &str = "BULLPEN_TYPESAFE_KEY";
pub const KEY_FILE_ENV: &str = "BULLPEN_TYPESAFE_KEY_FILE";

/// Override when Jev disagrees with the model's `spawn_helper` kind (S9-JEV-01).
pub const HELPER_KIND_CONFIDENCE: f64 = 0.85;
/// Minimum confidence to reorder grep output (S9-JEV-02).
pub const GREP_RERANK_CONFIDENCE: f64 = 0.75;
pub const GREP_RERANK_MAX_PATHS: usize = 12;

pub fn resolve_api_key() -> Option<String> {
    if let Ok(inline) = std::env::var(KEY_ENV) {
        let trimmed = inline.trim();
        if !trimmed.is_empty() {
            return Some(trimmed.to_string());
        }
    }
    let path = std::env::var(KEY_FILE_ENV).ok()?;
    let raw = std::fs::read_to_string(path).ok()?;
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        None
    } else {
        Some(trimmed.to_string())
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct ChoiceAnswer {
    pub choice: String,
    pub confidence: f64,
}

#[derive(Debug, Clone, PartialEq)]
pub struct ScoreAnswer {
    pub score: f64,
    pub confidence: f64,
}

#[async_trait::async_trait]
pub trait JevPort: Send + Sync {
    async fn system_one(&self, state: &str, questions: Value) -> Result<Value, String>;
}

pub struct HttpJev {
    key: String,
    client: reqwest::Client,
}

impl HttpJev {
    pub fn new(key: String) -> Self {
        let client = reqwest::Client::builder()
            .timeout(Duration::from_secs(20))
            .build()
            .expect("reqwest client for Jev");
        Self { key, client }
    }
}

#[async_trait::async_trait]
impl JevPort for HttpJev {
    async fn system_one(&self, state: &str, questions: Value) -> Result<Value, String> {
        let body = json!({
            "model": DEFAULT_MODEL,
            "state": state,
            "questions": questions,
        });
        let mut headers = HeaderMap::new();
        headers.insert(
            AUTHORIZATION,
            HeaderValue::from_str(&format!("Bearer {}", self.key)).map_err(|e| e.to_string())?,
        );
        headers.insert(CONTENT_TYPE, HeaderValue::from_static("application/json"));
        let response = self
            .client
            .post(SYSTEMONE_URL)
            .headers(headers)
            .body(body.to_string())
            .send()
            .await
            .map_err(|e| e.to_string())?;
        let status = response.status();
        let text = response.text().await.map_err(|e| e.to_string())?;
        if !status.is_success() {
            return Err(format!("Jev HTTP {status}: {text}"));
        }
        serde_json::from_str(&text).map_err(|e| format!("Jev JSON: {e}"))
    }
}

fn parse_choice(payload: &Value, question_id: &str) -> Option<ChoiceAnswer> {
    let answer = payload.get("answers")?.get(question_id)?;
    if answer.get("type")?.as_str()? != "choice" {
        return None;
    }
    Some(ChoiceAnswer {
        choice: answer.get("choice")?.as_str()?.to_string(),
        confidence: answer.get("confidence")?.as_f64().unwrap_or(0.0),
    })
}

fn parse_score(payload: &Value, question_id: &str) -> Option<ScoreAnswer> {
    let answer = payload.get("answers")?.get(question_id)?;
    if answer.get("type")?.as_str()? != "score" {
        return None;
    }
    Some(ScoreAnswer {
        score: answer.get("score")?.as_f64().unwrap_or(0.0),
        confidence: answer.get("confidence")?.as_f64().unwrap_or(0.0),
    })
}

/// S9-JEV-01: Choice over explore / browse / read from brief + model's pick.
pub async fn choose_helper_kind(
    jev: &dyn JevPort,
    brief: &str,
    model_kind: &str,
) -> Option<ChoiceAnswer> {
    let state = format!(
        "Brief for a throwaway helper errand:\n{brief}\n\nThe calling model chose helper kind: {model_kind}"
    );
    let questions = json!({
        "kind": {
            "type": "choice",
            "instructions": "Which helper tool set best matches this brief?",
            "criteria": {
                "explore": "Public web research — search and read URLs",
                "browse": "Shared computer — browse pages, click, type in a browser",
                "read": "Search this bot's memory and held outputs only"
            }
        }
    });
    let payload = jev.system_one(&state, questions).await.ok()?;
    parse_choice(&payload, "kind")
}

/// If Jev is configured and confident, may return a different valid kind.
pub async fn gate_helper_kind(brief: &str, model_kind: &str) -> String {
    let Some(key) = resolve_api_key() else {
        return model_kind.to_string();
    };
    let jev = HttpJev::new(key);
    let Some(answer) = choose_helper_kind(&jev, brief, model_kind).await else {
        return model_kind.to_string();
    };
    if answer.confidence < HELPER_KIND_CONFIDENCE {
        return model_kind.to_string();
    }
    if answer.choice == model_kind {
        return model_kind.to_string();
    }
    if crate::helpers::helper_spec(&answer.choice).is_some() {
        answer.choice
    } else {
        model_kind.to_string()
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GrepLine {
    pub path: String,
    pub line: String,
}

pub fn parse_grep_lines(raw: &str) -> Vec<GrepLine> {
    raw.lines()
        .filter_map(|line| {
            let line = line.trim();
            if line.is_empty() {
                return None;
            }
            let rest = line.strip_prefix("./").unwrap_or(line);
            let mut parts = rest.splitn(3, ':');
            let path = parts.next()?.to_string();
            let _lineno = parts.next()?;
            let content = parts.next().unwrap_or("");
            Some(GrepLine {
                path,
                line: content.to_string(),
            })
        })
        .collect()
}

pub fn unique_paths(lines: &[GrepLine], max: usize) -> Vec<String> {
    let mut seen = std::collections::HashSet::new();
    let mut out = Vec::new();
    for g in lines {
        if seen.insert(g.path.clone()) {
            out.push(g.path.clone());
            if out.len() >= max {
                break;
            }
        }
    }
    out
}

/// S9-JEV-02: Score one path's relevance to a search pattern (0..2 scale).
pub async fn score_path_relevance(
    jev: &dyn JevPort,
    pattern: &str,
    path: &str,
    sample_line: &str,
) -> Option<ScoreAnswer> {
    let state = format!(
        "Code search pattern: {pattern}\nFile path: {path}\nExample matching line:\n{sample_line}"
    );
    let questions = json!({
        "relevance": {
            "type": "score",
            "instructions": "How relevant is this file to the search?",
            "criteria": [
                "Unrelated or accidental match",
                "Somewhat related",
                "Highly relevant — likely what the searcher wants to read first"
            ]
        }
    });
    let payload = jev.system_one(&state, questions).await.ok()?;
    parse_score(&payload, "relevance")
}

fn grep_line_path(line: &str) -> &str {
    let rest = line.trim().strip_prefix("./").unwrap_or(line.trim());
    rest.split(':').next().unwrap_or("")
}

pub fn reorder_grep_output(raw: &str, path_scores: &HashMap<String, f64>) -> String {
    if path_scores.is_empty() {
        return raw.to_string();
    }
    let mut lines: Vec<&str> = raw.lines().collect();
    lines.sort_by(|a, b| {
        let sa = path_scores.get(grep_line_path(a)).copied().unwrap_or(-1.0);
        let sb = path_scores.get(grep_line_path(b)).copied().unwrap_or(-1.0);
        sb.partial_cmp(&sa).unwrap_or(std::cmp::Ordering::Equal)
    });
    lines.join("\n")
}

pub async fn rerank_grep_output(pattern: &str, raw: &str) -> String {
    if raw.trim().is_empty() || raw.trim() == "No matches." {
        return raw.to_string();
    }
    let Some(key) = resolve_api_key() else {
        return raw.to_string();
    };
    let parsed = parse_grep_lines(raw);
    let paths = unique_paths(&parsed, GREP_RERANK_MAX_PATHS);
    if paths.is_empty() {
        return raw.to_string();
    }
    let jev = HttpJev::new(key);
    let mut scores = HashMap::new();
    for path in paths {
        let sample = parsed
            .iter()
            .find(|g| g.path == path)
            .map(|g| g.line.as_str())
            .unwrap_or("");
        let Some(answer) = score_path_relevance(&jev, pattern, &path, sample).await else {
            continue;
        };
        if answer.confidence >= GREP_RERANK_CONFIDENCE {
            scores.insert(path, answer.score);
        }
    }
    if scores.is_empty() {
        return raw.to_string();
    }
    reorder_grep_output(raw, &scores)
}

#[cfg(test)]
mod tests {
    use super::*;

    struct StubJev {
        choice: ChoiceAnswer,
        score: ScoreAnswer,
    }

    #[async_trait::async_trait]
    impl JevPort for StubJev {
        async fn system_one(&self, _state: &str, questions: Value) -> Result<Value, String> {
            if questions.get("kind").is_some() {
                Ok(json!({
                    "answers": {
                        "kind": {
                            "type": "choice",
                            "choice": self.choice.choice,
                            "confidence": self.choice.confidence
                        }
                    }
                }))
            } else {
                Ok(json!({
                    "answers": {
                        "relevance": {
                            "type": "score",
                            "score": self.score.score,
                            "confidence": self.score.confidence
                        }
                    }
                }))
            }
        }
    }

    #[test]
    fn parse_grep_lines_extracts_paths() {
        let lines = parse_grep_lines("./src/a.rs:10:fn main\n./README.md:1:title\n");
        assert_eq!(lines.len(), 2);
        assert_eq!(lines[0].path, "src/a.rs");
    }

    #[test]
    fn reorder_puts_higher_scored_paths_first() {
        let raw = "./low.rs:1:a\n./high.rs:2:b\n./low.rs:3:c\n";
        let mut scores = HashMap::new();
        scores.insert("high.rs".into(), 2.0);
        scores.insert("low.rs".into(), 0.5);
        let out = reorder_grep_output(raw, &scores);
        assert!(out.find("./high.rs").unwrap() < out.find("./low.rs").unwrap());
    }

    #[tokio::test]
    async fn choose_helper_kind_reads_stub() {
        let stub = StubJev {
            choice: ChoiceAnswer {
                choice: "read".into(),
                confidence: 0.99,
            },
            score: ScoreAnswer {
                score: 2.0,
                confidence: 0.99,
            },
        };
        let ans = choose_helper_kind(&stub, "what did Josh say?", "explore")
            .await
            .unwrap();
        assert_eq!(ans.choice, "read");
    }
}
