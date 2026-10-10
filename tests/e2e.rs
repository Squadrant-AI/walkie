mod common;
use common::*;

#[test]
fn open_join_ask_answer_end() {
    let (a, b) = (Machine::new(), Machine::new());
    let room = pair(&a, &b);
    let (c, out, _) = a.run(&["--room", &room, "wait", "--timeout", "5"]);
    assert_eq!(c, 0);
    assert!(out.contains("peer-joined"));

    let (c, id, _) = b.run(&["--room", &room, "send", "what is the shape of /trips?"]);
    assert_eq!((c, id.trim()), (0, "j1"));
    let (c, out, _) = a.run(&["--room", &room, "wait", "--timeout", "5"]);
    assert_eq!(c, 0);
    assert!(out.contains("what is the shape of /trips?") && out.contains("id: j1"));

    a.run(&["--room", &room, "send", "--reply-to", "j1", "{id, name}"]);
    let out = b.wait_for(&room, "{id, name}", 3);
    assert!(out.contains("reply_to: j1"));

    assert_eq!(a.run(&["--room", &room, "end"]).0, 0);
    let (c, out, _) = b.run(&["--room", &room, "wait", "--timeout", "5"]);
    assert_eq!(c, 3);
    assert!(out.contains("room-ended"));
}

#[test]
fn wait_times_out_with_2() {
    let (a, b) = (Machine::new(), Machine::new());
    let room = pair(&a, &b);
    // the host's first wait drains `peer-joined`; after that there is nothing to print
    let (c, out, _) = a.run(&["--room", &room, "wait", "--timeout", "5"]);
    assert_eq!(c, 0);
    assert!(out.contains("peer-joined"));
    let (c, out, _) = a.run(&["--room", &room, "wait", "--timeout", "1"]);
    assert_eq!(c, 2);
    assert!(out.is_empty());
}

#[test]
fn send_reads_stdin() {
    let (a, b) = (Machine::new(), Machine::new());
    let room = pair(&a, &b);
    let (c, _, err) = b.run_stdin(&["--room", &room, "send", "-"], "line1\nline2\n");
    assert_eq!(c, 0, "{err}");
    // `peer-joined` may share a batch with the message or come first, so read until the body shows
    let out = a.wait_for(&room, "line2", 3);
    assert!(out.contains("line1\nline2"), "{out}");
}
