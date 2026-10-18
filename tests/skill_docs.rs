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
fn skill_documents_file_sharing() {
    let skill = read("skill/SKILL.md");
    for needle in [
        "## Sharing files",
        "walkie send-file",
        "NEVER paste a file into `send`",
        "WALKIE PEER FILE",
        "inbox",
        "until the user approves",
        "peer data",
        "walkie update",
        "8 MiB",
    ] {
        assert!(skill.contains(needle), "SKILL.md lacks {needle:?}");
    }
}

#[test]
fn readme_documents_file_sharing() {
    let readme = read("README.md");
    for needle in [
        "## Sending files",
        "send-file",
        "inbox",
        "8 MiB",
        "SHA-256",
        "0.1.2",
    ] {
        assert!(readme.contains(needle), "README lacks {needle:?}");
    }
}
