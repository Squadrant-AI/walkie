mod common;
use common::*;
use std::path::PathBuf;

/// host `a` and guest `b`, with the host's first wait (peer-joined) already drained
fn ready() -> (Machine, Machine, String) {
    let (a, b) = (Machine::new(), Machine::new());
    let room = pair(&a, &b);
    a.run(&["--room", &room, "wait", "--timeout", "5"]);
    (a, b, room)
}

fn write(m: &Machine, name: &str, bytes: &[u8]) -> PathBuf {
    let p = m.dir.path().join(name);
    std::fs::write(&p, bytes).unwrap();
    p
}

fn inbox(m: &Machine, room: &str) -> PathBuf {
    m.dir.path().join("home/rooms").join(room).join("inbox")
}

fn sha(bytes: &[u8]) -> String {
    use sha2::{Digest, Sha256};
    data_encoding::HEXLOWER.encode(&Sha256::digest(bytes))
}

fn binary(n: usize) -> Vec<u8> {
    (0..n).map(|i| (i * 31 % 251) as u8).collect()
}

#[test]
fn send_file_arrives_byte_identical_with_receiver_sha() {
    let (a, b, room) = ready();
    let text = b"first line\nlast line with trailing newline\n";
    let bin = binary(200_000); // several chunks, NULs and high bytes
    let p1 = write(&b, "notes.md", text);
    let p2 = write(&b, "blob.bin", &bin);
    let (c, out, err) = b.run(&[
        "--room",
        &room,
        "send-file",
        p1.to_str().unwrap(),
        p2.to_str().unwrap(),
    ]);
    assert_eq!(c, 0, "{err}");
    assert!(
        out.contains(&sha(text)) && out.contains(&sha(&bin)),
        "{out}"
    );

    let out = a.wait_for(&room, "blob.bin", 3);
    let dir = inbox(&a, &room);
    assert_eq!(std::fs::read(dir.join("notes.md")).unwrap(), text);
    assert_eq!(std::fs::read(dir.join("blob.bin")).unwrap(), bin);
    assert!(out.contains("<<<WALKIE PEER FILE "), "{out}");
    assert!(out.contains(&format!(
        "name: notes.md   size: {} bytes   sha256: {}",
        text.len(),
        sha(text)
    )));
    assert!(out.contains(&format!("sha256: {}", sha(&bin))));
    assert!(out.contains(&format!("saved: {}", dir.join("blob.bin").display())));
    use std::os::unix::fs::PermissionsExt;
    let mode = |p: PathBuf| std::fs::metadata(p).unwrap().permissions().mode() & 0o777;
    assert_eq!(mode(dir.clone()), 0o700);
    assert_eq!(mode(dir.join("notes.md")), 0o600);
    // acked: nothing more, and no staging leftovers
    assert_eq!(a.run(&["--room", &room, "wait", "--timeout", "1"]).0, 2);
    assert_eq!(std::fs::read_dir(&dir).unwrap().count(), 2);
}

#[test]
fn wait_json_has_a_file_object() {
    let (a, b, room) = ready();
    let p = write(&b, "j.txt", b"x\n");
    assert_eq!(
        b.run(&["--room", &room, "send-file", p.to_str().unwrap()])
            .0,
        0
    );
    std::thread::sleep(std::time::Duration::from_millis(500));
    let (c, out, _) = a.run(&["--room", &room, "wait", "--timeout", "5", "--json"]);
    assert_eq!(c, 0);
    let v: serde_json::Value = serde_json::from_str(out.lines().next().unwrap()).unwrap();
    assert_eq!(v["type"], "file");
    assert_eq!(v["sha256"], sha(b"x\n"));
    assert_eq!(v["name"], "j.txt");
}

#[test]
fn oversize_and_non_regular_files_are_refused_and_nothing_is_sent() {
    let (a, b, room) = ready();
    let big = b.dir.path().join("big.bin");
    std::fs::File::create(&big)
        .unwrap()
        .set_len(8 * 1024 * 1024 + 1)
        .unwrap();
    let (c, _, err) = b.run(&["--room", &room, "send-file", big.to_str().unwrap()]);
    assert_eq!(c, 1);
    assert!(err.contains("too large") && err.contains("8 MiB"), "{err}");

    let ok = write(&b, "ok.txt", b"fine");
    let dir = b.dir.path().to_str().unwrap().to_string();
    let (c, _, err) = b.run(&["--room", &room, "send-file", ok.to_str().unwrap(), &dir]);
    assert_eq!(c, 1);
    assert!(err.contains("not a regular file"), "{err}");

    // exactly 8 MiB is allowed
    let max = b.dir.path().join("max.bin");
    std::fs::write(&max, vec![7u8; 8 * 1024 * 1024]).unwrap();
    assert_eq!(
        b.run(&["--room", &room, "send-file", max.to_str().unwrap()])
            .0,
        0
    );
    a.wait_for(&room, "max.bin", 3);
    assert!(
        !inbox(&a, &room).join("ok.txt").exists(),
        "bad batch sent nothing"
    );
    assert_eq!(
        std::fs::metadata(inbox(&a, &room).join("max.bin"))
            .unwrap()
            .len(),
        8 << 20
    );
}

#[test]
fn peer_cannot_pick_the_path_and_collisions_never_overwrite() {
    let (a, b, room) = ready();
    // the sender sanitizes too, but the receiver must hold on its own: use a dir-ish local name
    let sub = b.dir.path().join("sub");
    std::fs::create_dir(&sub).unwrap();
    let p = sub.join("same.txt");
    std::fs::write(&p, b"v1").unwrap();
    b.run(&["--room", &room, "send-file", p.to_str().unwrap()]);
    std::fs::write(&p, b"v2").unwrap();
    b.run(&["--room", &room, "send-file", p.to_str().unwrap()]);
    a.wait_for(&room, "same-1.txt", 3);
    let dir = inbox(&a, &room);
    assert_eq!(std::fs::read(dir.join("same.txt")).unwrap(), b"v1");
    assert_eq!(std::fs::read(dir.join("same-1.txt")).unwrap(), b"v2");
}

#[test]
fn killed_wait_redelivers_the_file_event_without_a_second_file() {
    let (a, b, room) = ready();
    let p = write(&b, "once.txt", b"only once\n");
    b.run(&["--room", &room, "send-file", p.to_str().unwrap()]);
    std::thread::sleep(std::time::Duration::from_millis(500));
    let (_, out1, _) = a.run_with(
        &["--room", &room, "wait", "--timeout", "5"],
        &[("WALKIE_TEST_SKIP_ACK", "1")],
    );
    let (_, out2, _) = a.run(&["--room", &room, "wait", "--timeout", "5"]);
    for o in [&out1, &out2] {
        assert!(o.contains("name: once.txt") && o.contains("id: j1"), "{o}");
    }
    assert_eq!(a.run(&["--room", &room, "wait", "--timeout", "1"]).0, 2);
    assert_eq!(std::fs::read_dir(inbox(&a, &room)).unwrap().count(), 1);
}

#[test]
fn receiver_daemon_restart_redelivers_without_duplicate_file() {
    let (a, b, room) = ready();
    let data = binary(100_000);
    let p = write(&b, "r.bin", &data);
    b.run(&["--room", &room, "send-file", p.to_str().unwrap()]);
    std::thread::sleep(std::time::Duration::from_millis(800));
    a.kill9(&room); // file already saved, event unread and lost with the daemon
    assert_eq!(a.run(&["rejoin", &room]).0, 0);
    let out = a.wait_for(&room, "r.bin", 3);
    assert_eq!(out.matches("name: r.bin").count(), 1, "{out}");
    let dir = inbox(&a, &room);
    assert_eq!(
        std::fs::read_dir(&dir).unwrap().count(),
        1,
        "no duplicate on disk"
    );
    assert_eq!(std::fs::read(dir.join("r.bin")).unwrap(), data);
}

#[test]
fn v012_peer_is_refused_clearly() {
    let (a, b) = (Machine::new(), Machine::new());
    b.set_env("WALKIE_TEST_NO_CAPS", "1"); // b's walkie looks like v0.1.2
    let room = pair(&a, &b);
    let p = write(&a, "f.txt", b"x");
    let (c, _, err) = a.run(&["--room", &room, "send-file", p.to_str().unwrap()]);
    assert_eq!(c, 1);
    assert!(
        err.contains("too old for send-file") && err.contains("walkie update"),
        "{err}"
    );
    // text still works both ways
    assert_eq!(a.run(&["--room", &room, "send", "hi"]).0, 0);
    b.wait_for(&room, "hi", 3);
}
