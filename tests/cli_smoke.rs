use assert_cmd::Command;

#[test]
fn help_lists_all_commands() {
    let out = Command::cargo_bin("walkie")
        .unwrap()
        .arg("--help")
        .assert()
        .success();
    let s = String::from_utf8(out.get_output().stdout.clone()).unwrap();
    for c in [
        "open", "join", "send", "wait", "status", "rooms", "leave", "rejoin", "end",
    ] {
        assert!(s.contains(c), "missing {c}");
    }
    assert!(!s.contains("__daemon"));
}
