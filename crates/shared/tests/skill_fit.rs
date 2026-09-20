//! Port of `test/skillFit.test.ts` vectors (fixture strings copied verbatim).

use shared::{SkillFit, assess_skill, body_for_bullpen, capability_note};

fn s(v: &str) -> String {
    v.to_string()
}

#[test]
fn zenith_path_form() {
    let body = "Run: `node <this-skill>/reference/generator.mjs show.json <outBasename>`";
    let fit = assess_skill(body, &[s("reference/generator.mjs")]);
    assert_eq!(fit.missing, vec!["reference/generator.mjs"]);
}

#[test]
fn wizard_bare_basename() {
    let body = "The delightful UX is already solved by [template.sh](template.sh).";
    assert_eq!(
        assess_skill(body, &[s("template.sh")]).missing,
        vec!["template.sh"]
    );
}

#[test]
fn sentiment_fetch_py() {
    let body = r#"python "C:/Users/rain/.claude/skills/sentiment/sentiment_fetch.py" "<q1>""#;
    assert_eq!(
        assess_skill(body, &[s("sentiment_fetch.py")]).missing,
        vec!["sentiment_fetch.py"]
    );
}

#[test]
fn ignores_unmentioned_sibling() {
    let body = "Mine raw fragments. No structure yet.";
    assert!(
        assess_skill(body, &[s("agents/openai.yaml")])
            .missing
            .is_empty()
    );
}

#[test]
fn basename_not_inside_longer_word() {
    let body = "Run the counterextract.python pipeline.";
    assert!(assess_skill(body, &[s("extract.py")]).missing.is_empty());
}

#[test]
fn reports_every_missing_file() {
    let body = "See reference/adapt.md then reference/animate.md.";
    let fit = assess_skill(
        body,
        &[
            s("reference/adapt.md"),
            s("reference/animate.md"),
            s("unused.md"),
        ],
    );
    assert_eq!(
        fit.missing,
        vec!["reference/adapt.md", "reference/animate.md"]
    );
}

#[test]
fn names_node_for_shell_script() {
    let fit = assess_skill("Then run `node scripts/build.mjs`.", &[]);
    assert!(fit.missing.is_empty());
    assert!(fit.lacks.join(" ").contains("node"));
}

#[test]
fn names_python_for_py_invocation() {
    assert!(
        assess_skill("python tools/scan.py --all", &[])
            .lacks
            .join(" ")
            .contains("python")
    );
}

#[test]
fn names_subagents_anti_slop_line() {
    let body = "So: **spawn a subagent that gets the deliverable and nothing else.**";
    let fit = assess_skill(body, &[]);
    assert!(fit.lacks.join(" ").contains("subagents"));
}

#[test]
fn names_workstation_path() {
    let body = r"Read `D:\rainmade\knowledge\wiki\patio11.md` for grounding.";
    assert!(
        assess_skill(body, &[])
            .lacks
            .join(" ")
            .contains("Josh's own machine")
    );
}

#[test]
fn pure_judgement_skill_trips_nothing() {
    let body = [
        "Read the deliverable. Does the first paragraph state the conclusion?",
        "Cut any sentence that does not change what the reader does next.",
        "Prefer bullets. Name the strongest counter-argument.",
    ]
    .join("\n");
    assert_eq!(
        assess_skill(&body, &[]),
        SkillFit {
            missing: vec![],
            lacks: vec![]
        }
    );
}

#[test]
fn prose_about_git_does_not_trip() {
    let body = "Summarise what changed, the way a git history would read.";
    assert!(assess_skill(body, &[]).lacks.is_empty());
}

#[test]
fn git_worktree_instruction_trips() {
    let body = "Verify it from a throwaway `git worktree` of the commit itself.";
    assert!(
        assess_skill(body, &[])
            .lacks
            .join(" ")
            .contains("git checkout")
    );
}

#[test]
fn node_in_prose_does_not_trip() {
    let body = "Treat each decision as a node in the dependency graph.";
    assert!(assess_skill(body, &[]).lacks.is_empty());
}

#[test]
fn does_not_repeat_capability() {
    let body = "node a.mjs\nnode b.mjs\nnode c.mjs";
    let lacks = assess_skill(body, &[]).lacks;
    assert_eq!(
        lacks.len(),
        lacks.iter().collect::<std::collections::HashSet<_>>().len()
    );
}

#[test]
fn capability_note_empty_when_clean() {
    assert!(capability_note(&[]).is_empty());
    assert_eq!(
        body_for_bullpen(
            "Do the thing.",
            &SkillFit {
                missing: vec![],
                lacks: vec![]
            }
        ),
        "Do the thing."
    );
}

#[test]
fn capability_note_names_gaps_and_forbids_faking() {
    let note = capability_note(&[s("python"), s("subagents")]);
    assert!(note.contains("python"));
    assert!(note.contains("subagents"));
    assert!(note.contains("Never describe having done it."));
}

#[test]
fn body_for_bullpen_keeps_original_under_note() {
    let out = body_for_bullpen(
        "STEP ONE",
        &SkillFit {
            missing: vec![],
            lacks: vec![s("python")],
        },
    );
    assert!(out.contains("STEP ONE"));
    assert!(out.find("Bullpen note").unwrap() < out.find("STEP ONE").unwrap());
}
