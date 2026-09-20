//! Imports Claude Code skills into Bullpen. Port of `scripts/import-skills.mjs`.
//!
//! ```text
//! cargo run -p server --bin import-skills              # live import
//! cargo run -p server --bin import-skills -- --dry     # preview only
//! cargo run -p server --bin import-skills -- --from <dir>
//! ```
//!
//! Env: `BULLPEN_URL` (default `http://127.0.0.1:4381/`), `BULLPEN_PASSWORD` (required unless `--dry`).

use shared::{SkillFit, assess_skill, body_for_bullpen, parse_skill_file};
use std::env;
use std::fs;
use std::path::{Path, PathBuf};
use std::process;

#[derive(Debug)]
struct DiscoveredSkill {
    name: String,
    description: String,
    body: String,
    fit: SkillFit,
    skipped: Option<String>,
}

fn home_skills_dir() -> PathBuf {
    if let Ok(home) = env::var("HOME") {
        return PathBuf::from(home).join(".claude/skills");
    }
    if let Ok(profile) = env::var("USERPROFILE") {
        return PathBuf::from(profile).join(".claude/skills");
    }
    PathBuf::from(".claude/skills")
}

fn parse_args() -> (bool, PathBuf) {
    let args: Vec<String> = env::args().collect();
    let dry = args.iter().any(|a| a == "--dry");
    let mut from = home_skills_dir();
    let mut i = 1;
    while i < args.len() {
        if args[i] == "--from" {
            i += 1;
            if i >= args.len() {
                eprintln!("--from requires a directory");
                process::exit(1);
            }
            from = PathBuf::from(&args[i]);
        }
        i += 1;
    }
    (dry, from)
}

fn siblings_of(dir: &Path) -> Vec<String> {
    let mut out = Vec::new();
    walk_siblings(dir, dir, &mut out);
    out
}

fn walk_siblings(root: &Path, current: &Path, out: &mut Vec<String>) {
    let entries = match fs::read_dir(current) {
        Ok(e) => e,
        Err(_) => return,
    };
    for entry in entries.flatten() {
        let name = entry.file_name();
        let name = name.to_string_lossy();
        if name == ".git" {
            continue;
        }
        let path = entry.path();
        if path.is_dir() {
            walk_siblings(root, &path, out);
        } else if name != "SKILL.md" {
            let rel = path.strip_prefix(root).unwrap_or(&path);
            let rel = rel.to_string_lossy().replace('\\', "/");
            out.push(rel);
        }
    }
}

fn discover(dir: &Path) -> Result<Vec<DiscoveredSkill>, String> {
    if !dir.is_dir() {
        return Err(format!("No skills folder at {}", dir.display()));
    }
    let mut found = Vec::new();
    for entry in fs::read_dir(dir).map_err(|e| e.to_string())? {
        let entry = entry.map_err(|e| e.to_string())?;
        if !entry.file_type().map_err(|e| e.to_string())?.is_dir() {
            continue;
        }
        let folder_name = entry.file_name().to_string_lossy().into_owned();
        let skill_dir = entry.path();
        let skill_file = skill_dir.join("SKILL.md");
        if !skill_file.is_file() {
            continue;
        }
        let text = fs::read_to_string(&skill_file).map_err(|e| e.to_string())?;
        let Some(parsed) = parse_skill_file(&text) else {
            found.push(DiscoveredSkill {
                name: folder_name,
                description: String::new(),
                body: String::new(),
                fit: SkillFit {
                    missing: vec![],
                    lacks: vec![],
                },
                skipped: Some("no front matter".to_string()),
            });
            continue;
        };

        let siblings: Vec<String> = siblings_of(&skill_dir);
        let fit = assess_skill(&parsed.body, &siblings);
        let body = body_for_bullpen(&parsed.body, &fit);
        let skipped = if !fit.missing.is_empty() {
            Some(format!(
                "needs files the import cannot carry: {}",
                fit.missing.join(", ")
            ))
        } else {
            None
        };

        found.push(DiscoveredSkill {
            name: folder_name,
            description: parsed.description,
            body,
            fit,
            skipped,
        });
    }
    Ok(found)
}

fn join_url(base: &str, path: &str) -> String {
    let base = base.trim_end_matches('/');
    format!("{base}/{path}")
}

#[tokio::main]
async fn main() {
    let (dry, skills_dir) = parse_args();
    let server = env::var("BULLPEN_URL").unwrap_or_else(|_| "http://127.0.0.1:4381/".to_string());
    let password = env::var("BULLPEN_PASSWORD").unwrap_or_default();

    let skills = match discover(&skills_dir) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("{e}");
            process::exit(1);
        }
    };

    let usable: Vec<_> = skills
        .iter()
        .filter(|s| s.skipped.is_none() && !s.description.is_empty())
        .collect();
    let thin: Vec<_> = skills
        .iter()
        .filter(|s| s.skipped.is_none() && s.description.is_empty())
        .collect();
    let unfollowable: Vec<_> = skills
        .iter()
        .filter(|s| !s.fit.missing.is_empty())
        .collect();
    let annotated: Vec<_> = usable
        .iter()
        .filter(|s| !s.fit.lacks.is_empty())
        .copied()
        .collect();

    println!("found {} skills in {}", skills.len(), skills_dir.display());

    if !unfollowable.is_empty() {
        println!(
            "\n{} REFUSED - the body needs files the import cannot carry:",
            unfollowable.len()
        );
        for s in &unfollowable {
            println!("  - {:28} {}", s.name, s.fit.missing.join(", "));
        }
    }

    if !annotated.is_empty() {
        println!("\n{} imported WITH a capability note:", annotated.len());
        for s in annotated {
            println!("  - {:28} lacks {}", s.name, s.fit.lacks.join("; "));
        }
    }

    if !thin.is_empty() {
        println!(
            "\n{} have no description and would never be picked:",
            thin.len()
        );
        for s in &thin {
            println!("  - {}", s.name);
        }
    }

    if dry {
        println!("\n--dry: would import {}", usable.len());
        for s in usable {
            let desc = if s.description.len() > 70 {
                &s.description[..70]
            } else {
                &s.description
            };
            println!("  {:28} {desc}", s.name);
        }
        return;
    }

    if password.is_empty() {
        eprintln!("\nSet BULLPEN_PASSWORD (and BULLPEN_URL if not the live server).");
        process::exit(1);
    }

    let client = reqwest::Client::new();
    let login_url = join_url(&server, "api/auth/login");
    let login = client
        .post(&login_url)
        .header("content-type", "application/json")
        .json(&serde_json::json!({ "password": password }))
        .send()
        .await
        .expect("login request");
    if !login.status().is_success() {
        eprintln!("sign in failed: {}", login.status());
        process::exit(1);
    }
    let login_body: serde_json::Value = login.json().await.expect("login json");
    let token = login_body["token"]
        .as_str()
        .expect("token in login response")
        .to_string();

    let mut done = 0usize;
    for skill in &usable {
        let path = format!("api/skills/{}", urlencoding_path_segment(&skill.name));
        let url = join_url(&server, &path);
        let res = client
            .put(&url)
            .header("content-type", "application/json")
            .header("authorization", format!("Bearer {token}"))
            .json(&serde_json::json!({
                "description": skill.description,
                "body": skill.body,
                "source": "claude-code",
            }))
            .send()
            .await
            .expect("put skill");
        if !res.status().is_success() {
            let status = res.status();
            let text = res.text().await.unwrap_or_default();
            eprintln!("  {}: {} {}", skill.name, status, text);
            continue;
        }
        done += 1;
    }

    let mut withdrawn = 0usize;
    for skill in &unfollowable {
        let path = format!("api/skills/{}", urlencoding_path_segment(&skill.name));
        let url = join_url(&server, &path);
        let res = client
            .delete(&url)
            .header("authorization", format!("Bearer {token}"))
            .send()
            .await
            .expect("delete skill");
        if res.status().is_success() || res.status().as_u16() == 404 {
            withdrawn += 1;
        } else {
            eprintln!("  {}: could not withdraw, {}", skill.name, res.status());
        }
    }

    println!("\nimported {done} of {}", usable.len());
    if withdrawn > 0 {
        println!("withdrew {withdrawn} that a bot could not follow");
    }
    println!("Nothing is switched on. Give a bot a skill in its editor.");
}

/// Same encoding as `encodeURIComponent` for skill names (ASCII subset in practice).
fn urlencoding_path_segment(name: &str) -> String {
    let mut out = String::new();
    for b in name.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(b as char);
            }
            _ => {
                out.push('%');
                out.push(hex_digit(b >> 4));
                out.push(hex_digit(b & 0xf));
            }
        }
    }
    out
}

fn hex_digit(n: u8) -> char {
    match n {
        0..=9 => (b'0' + n) as char,
        _ => (b'A' + n - 10) as char,
    }
}
