//! The agent-facing docs must cover every exit code (0-8) the CLI can return.
fn read(p: &str) -> String {
    std::fs::read_to_string(format!("{}/{p}", env!("CARGO_MANIFEST_DIR"))).unwrap()
}

#[test]
fn skill_lists_every_exit_code() {
    let skill = read("skill/SKILL.md");
    for code in 0..=8 {
        assert!(
            skill.contains(&format!("- `{code}` →")),
            "SKILL.md is missing exit code {code}"
        );
    }
    assert!(!skill.contains("`9` →"));
}

#[test]
fn readme_lists_every_exit_code() {
    let readme = read("README.md");
    for code in 0..=8 {
        assert!(
            readme.contains(&format!("| `{code}` |")),
            "README is missing exit code {code}"
        );
    }
}

#[test]
fn spec_exit_table_matches() {
    let spec = read("docs/specs/2026-10-10-walkie-v1-design.md");
    for code in 0..=8 {
        assert!(spec.contains(&format!("| `{code}` |")), "spec lacks {code}");
    }
}

#[test]
fn skill_keeps_a_background_wait_running() {
    let skill = read("skill/SKILL.md");
    for needle in [
        "walkie wait --until-message",
        "run_in_background: true",
        "Keep listening",
        "Do NOT ask the user \"should I keep listening?\"",
    ] {
        assert!(skill.contains(needle), "SKILL.md is missing {needle:?}");
    }
}

#[test]
fn readme_documents_until_message() {
    assert!(read("README.md").contains("--until-message"));
}
