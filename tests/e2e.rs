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

#[test]
fn joiner_sees_room_ended_without_explicit_room_and_daemon_exits() {
    // Regression: the joiner deleted its ticket on RoomEnded, so room selection found nothing
    // (exit 4) while its daemon still held the undelivered event.
    let (a, b) = (Machine::new(), Machine::new());
    let room = pair(&a, &b);
    let pid = b.pid_of(&room).unwrap();
    assert_eq!(a.run(&["end"]).0, 0);
    let (c, out, err) = b.run(&["wait", "--timeout", "10"]);
    assert_eq!(c, 3, "{err}");
    assert!(out.contains("room-ended"), "{out}");
    assert!(
        Machine::wait_gone(pid, 10),
        "joiner daemon must exit after the ack"
    );
}

#[test]
fn unacked_terminal_event_still_bounds_the_daemon() {
    let (a, b) = (Machine::new(), Machine::new());
    b.set_env("WALKIE_TEST_TERMINAL_EXIT_SECS", "2");
    let room = pair(&a, &b);
    let pid = b.pid_of(&room).unwrap();
    assert_eq!(a.run(&["end"]).0, 0);
    // nobody ever waits on b
    assert!(
        Machine::wait_gone(pid, 12),
        "daemon must exit without an ack"
    );
}

#[test]
fn long_loop_200_round_trips() {
    let (a, b) = (Machine::new(), Machine::new());
    let room = pair(&a, &b);
    a.run(&["--room", &room, "wait", "--timeout", "5"]); // peer-joined
    for i in 0..200 {
        b.run(&["--room", &room, "send", &format!("q{i}")]);
        let out = a.wait_for(&room, &format!("q{i}"), 2);
        assert!(out.contains(&format!("q{i}")), "round {i}");
        a.run(&["--room", &room, "send", &format!("a{i}")]);
        let out = b.wait_for(&room, &format!("a{i}"), 2);
        assert!(out.contains(&format!("a{i}")), "round {i}");
    }
}

#[test]
fn messages_queue_while_not_waiting() {
    let (a, b) = (Machine::new(), Machine::new());
    let room = pair(&a, &b);
    for i in 0..5 {
        b.run(&["--room", &room, "send", &format!("m{i}")]);
    }
    let out = a.wait_for(&room, "m4", 3);
    for i in 0..5 {
        assert_eq!(out.matches(&format!("m{i}\n")).count(), 1, "m{i} once");
    }
}

#[test]
fn killed_wait_is_redelivered() {
    let (a, b) = (Machine::new(), Machine::new());
    let room = pair(&a, &b);
    a.run(&["--room", &room, "wait", "--timeout", "5"]);
    b.run(&["--room", &room, "send", "important"]);
    std::thread::sleep(std::time::Duration::from_millis(500));
    // SKIP_ACK: prints the batch, then "dies" before acking, like a harness killing the tool
    let (c, out, _) = a.run_with(
        &["--room", &room, "wait", "--timeout", "5"],
        &[("WALKIE_TEST_SKIP_ACK", "1")],
    );
    assert_eq!(c, 0);
    assert!(out.contains("important") && out.contains("id: j1"));
    let (c, out, _) = a.run(&["--room", &room, "wait", "--timeout", "5"]);
    assert_eq!(c, 0);
    assert!(
        out.contains("important") && out.contains("id: j1"),
        "same id redelivered: {out}"
    );
    // now it was acked: nothing more
    assert_eq!(a.run(&["--room", &room, "wait", "--timeout", "1"]).0, 2);
}

#[test]
fn leave_rejoin_both_sides_no_dupes_no_gaps() {
    let (a, b) = (Machine::new(), Machine::new());
    let room = pair(&a, &b);
    a.run(&["--room", &room, "wait", "--timeout", "5"]); // peer-joined
    let (c, out, _) = b.run(&["--room", &room, "leave"]);
    assert_eq!(c, 0);
    assert!(out.contains("rejoin"));
    let (c, out, _) = a.run(&["--room", &room, "wait", "--timeout", "5"]);
    assert_eq!(c, 5);
    assert!(out.contains("peer-left"));
    let (c, _, _) = a.run(&["--room", &room, "wait", "--timeout", "1"]);
    assert_eq!(c, 2, "exit 5 only once per transition");
    for i in 0..3 {
        a.run(&["--room", &room, "send", &format!("while-away-{i}")]);
    }
    assert_eq!(b.run(&["rejoin", &room]).0, 0);
    let out = b.wait_for(&room, "while-away-2", 4);
    for i in 0..3 {
        assert_eq!(
            out.matches(&format!("while-away-{i}\n")).count(),
            1,
            "while-away-{i} exactly once"
        );
        assert_eq!(
            out.matches(&format!("id: h{}   ", i + 1)).count(),
            1,
            "no gaps"
        );
    }
    let out = a.wait_for(&room, "peer-rejoined", 2);
    assert!(out.contains("peer-rejoined"));
    // host leaves and rejoins too
    a.run(&["--room", &room, "leave"]);
    b.run(&["--room", &room, "send", "for-host"]);
    assert_eq!(a.run(&["rejoin", &room]).0, 0);
    let out = a.wait_for(&room, "for-host", 3);
    assert_eq!(out.matches("for-host\n").count(), 1);
}

#[test]
fn kill9_receiver_daemon_loses_nothing() {
    let (a, b) = (Machine::new(), Machine::new());
    let room = pair(&a, &b);
    b.run(&["--room", &room, "send", "survive-me"]);
    std::thread::sleep(std::time::Duration::from_millis(500));
    a.kill9(&room); // stale socket is cleaned up by the next daemon
    assert_eq!(a.run(&["rejoin", &room]).0, 0);
    let seen = a.wait_for(&room, "survive-me", 3);
    assert_eq!(seen.matches("survive-me\n").count(), 1);
}

#[test]
fn lost_joined_reply_then_join_retry_reconnects() {
    let (a, b) = (Machine::new(), Machine::new());
    let (_, out, _) = a.run(&["open"]);
    let room = field(&out, "room:").to_string();
    let invite = field(&out, "invite:").to_string();
    a.track(&room);
    assert_eq!(b.run(&["join", &invite]).0, 0);
    b.track(&room);
    // simulate the CLI having missed "joined": kill b's daemon, run join again with the same invite
    b.kill9(&room);
    let (c, _, err) = b.run(&["join", &invite]);
    assert_eq!(c, 0, "retry must reconnect with the pinned key, got: {err}");
}

#[test]
fn wrong_token_refused_then_used_invite_exits_7() {
    let (a, b, c) = (Machine::new(), Machine::new(), Machine::new());
    let (_, out, _) = a.run(&["open"]);
    let room = field(&out, "room:").to_string();
    let invite = field(&out, "invite:").to_string();
    a.track(&room);
    let (ticket, _) = invite.rsplit_once('.').unwrap();
    let forged = format!("{ticket}.aaaaaaaaaaaaaaaaaaaaaaaaaa");
    assert_eq!(c.run(&["join", &forged]).0, 7, "forged token");
    assert_eq!(
        b.run(&["join", &invite]).0,
        0,
        "the real invite still works"
    );
    b.track(&room);
    assert_eq!(c.run(&["join", &invite]).0, 7, "used invite");
}

#[test]
fn expired_invite_exits_7_while_host_is_alive_and_host_sees_invite_expired() {
    let (a, b) = (Machine::new(), Machine::new());
    let (_, out, _) = a.run(&["open", "--ttl", "2s"]);
    let room = field(&out, "room:").to_string();
    let invite = field(&out, "invite:").to_string();
    std::thread::sleep(std::time::Duration::from_secs(3));
    // the host daemon lingers until the event is read, so it answers Reject → exit 7
    assert_eq!(b.run(&["join", &invite]).0, 7);
    let (c, out, _) = a.run(&["--room", &room, "wait", "--timeout", "3"]);
    assert_eq!(c, 3);
    assert!(out.contains("invite-expired"));
}

#[test]
fn join_with_host_gone_exits_6() {
    // the invite's host daemon no longer exists: the joiner cannot tell why → exit 6
    let (a, b) = (Machine::new(), Machine::new());
    let (_, out, _) = a.run(&["open"]);
    let room = field(&out, "room:").to_string();
    let invite = field(&out, "invite:").to_string();
    a.kill9(&room);
    let (c, _, _) = b.run_with(&["join", &invite], &[("WALKIE_TEST_CONNECT_SECS", "3")]);
    assert_eq!(c, 6);
}

#[test]
fn rejoin_after_end() {
    let (a, b) = (Machine::new(), Machine::new());
    let room = pair(&a, &b);
    let b_pid = b.pid_of(&room).unwrap();
    b.run(&["--room", &room, "leave"]);
    // b's daemon lingers ~300 ms after `leave`; a `room-ended` arriving then would still reach it
    assert!(Machine::wait_gone(b_pid, 10));
    a.run(&["--room", &room, "end"]);
    // the side that ended remembers (tombstone): exit 3
    assert_eq!(a.run(&["rejoin", &room]).0, 3);
    // the away side has no way to learn of it: unreachable until its ticket expires
    let (c, out, err) = b.run_with(&["rejoin", &room], &[("WALKIE_TEST_CONNECT_SECS", "3")]);
    assert_eq!(c, 6, "{out} {err}");
}

#[test]
fn rejoin_with_peer_offline_exits_6() {
    let (a, b) = (Machine::new(), Machine::new());
    let room = pair(&a, &b);
    let b_pid = b.pid_of(&room).unwrap();
    b.run(&["--room", &room, "leave"]);
    assert!(Machine::wait_gone(b_pid, 10));
    a.kill9(&room); // the remaining side is gone (crash) → b cannot reach anyone
    let (c, _, _) = b.run_with(&["rejoin", &room], &[("WALKIE_TEST_CONNECT_SECS", "3")]);
    assert_eq!(c, 6);
}

#[test]
fn idle_ttl_ends_room() {
    let (a, b) = (Machine::new(), Machine::new());
    let (_, out, _) = a.run(&["open", "--idle-ttl", "3s"]);
    let room = field(&out, "room:").to_string();
    let invite = field(&out, "invite:").to_string();
    a.track(&room);
    assert_eq!(b.run(&["join", &invite]).0, 0);
    b.run(&["--room", &room, "leave"]);
    std::thread::sleep(std::time::Duration::from_secs(5));
    let (c, out, _) = a.run(&["--room", &room, "wait", "--timeout", "3"]);
    assert_eq!(c, 3);
    assert!(out.contains("room-ended"));
}

#[test]
fn rooms_prunes_expired_tickets_and_rejoin_then_says_ended() {
    // preflight D9/D18: pruning writes a tombstone, so rejoin exits 3, not 4
    let (a, b) = (Machine::new(), Machine::new());
    let (_, out, _) = a.run(&["open", "--idle-ttl", "2s"]);
    let room = field(&out, "room:").to_string();
    let invite = field(&out, "invite:").to_string();
    assert_eq!(b.run(&["join", &invite]).0, 0);
    a.run(&["--room", &room, "leave"]);
    let (_, listed, _) = a.run(&["rooms"]);
    assert!(listed.contains(&room), "still rejoinable: {listed}");
    std::thread::sleep(std::time::Duration::from_secs(3));
    let (c, listed, _) = a.run(&["rooms"]);
    assert_eq!(c, 0);
    assert!(!listed.contains(&room), "pruned: {listed}");
    assert!(!a
        .dir
        .path()
        .join("home/rooms")
        .join(format!("{room}.ticket"))
        .exists());
    assert_eq!(a.run(&["rejoin", &room]).0, 3);
    let _ = b.run(&["--room", &room, "end"]);
}

#[test]
fn send_while_wait_running() {
    let (a, b) = (Machine::new(), Machine::new());
    let room = pair(&a, &b);
    a.run(&["--room", &room, "wait", "--timeout", "5"]); // peer-joined
    let waiter = a.spawn(&["--room", &room, "wait", "--timeout", "5"]);
    std::thread::sleep(std::time::Duration::from_millis(300));
    assert_eq!(a.run(&["--room", &room, "send", "concurrent"]).0, 0);
    b.run(&["--room", &room, "send", "to-waiter"]);
    let (c, out, _) = out_of(waiter.wait_with_output().unwrap());
    assert_eq!(c, 0);
    assert!(out.contains("to-waiter"));
    let out = b.wait_for(&room, "concurrent", 2);
    assert!(out.contains("concurrent"));
}

#[test]
fn second_wait_preempts_first() {
    let (a, b) = (Machine::new(), Machine::new());
    let room = pair(&a, &b);
    a.run(&["--room", &room, "wait", "--timeout", "5"]); // peer-joined
    let first = a.spawn(&["--room", &room, "wait", "--timeout", "10"]);
    std::thread::sleep(std::time::Duration::from_millis(500));
    let second = a.spawn(&["--room", &room, "wait", "--timeout", "10"]);
    // the old wait exits 1, without acking or printing anything
    let (c1, out1, err1) = out_of(first.wait_with_output().unwrap());
    assert_eq!(c1, 1);
    assert!(out1.is_empty());
    assert!(err1.contains("another wait took over"), "{err1}");
    b.run(&["--room", &room, "send", "for-the-second"]);
    let (c2, out2, _) = out_of(second.wait_with_output().unwrap());
    assert_eq!(c2, 0);
    assert!(out2.contains("for-the-second"));
}

#[test]
fn status_shows_peer_state_path_and_last_waited() {
    let (a, b) = (Machine::new(), Machine::new());
    let room = pair(&a, &b);
    b.run(&["--room", &room, "wait", "--timeout", "1"]);
    std::thread::sleep(std::time::Duration::from_millis(300));
    let (c, out, _) = a.run(&["--room", &room, "status"]);
    assert_eq!(c, 0);
    assert!(out.contains(&format!("room: {room} (host)")), "{out}");
    assert!(out.contains("peer: connected"), "{out}");
    assert!(out.contains("path: direct"), "{out}");
    assert!(out.contains("peer last waited: "), "{out}");
    let (_, json, _) = a.run(&["--room", &room, "status", "--json"]);
    let v: serde_json::Value = serde_json::from_str(json.trim()).unwrap();
    assert_eq!(v["peer"]["state"], "connected");
}

#[test]
fn ambiguous_room_without_selection_exits_4() {
    let (a, b) = (Machine::new(), Machine::new());
    let r1 = pair(&a, &b);
    let r2 = pair(&a, &b);
    let (c, _, err) = a.run(&["status"]);
    assert_eq!(c, 4);
    assert!(err.contains("multiple active rooms"), "{err}");
    assert!(err.contains(&r1) && err.contains(&r2), "{err}");
    // an explicit selection still works
    assert_eq!(a.run(&["--room", &r1, "status"]).0, 0);
}

#[test]
fn open_runtime_dir_is_refused_with_exit_8() {
    let a = Machine::new();
    let rt = a.dir.path().join("rt");
    std::fs::create_dir_all(&rt).unwrap();
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(&rt, std::fs::Permissions::from_mode(0o755)).unwrap();
    let (c, _, err) = a.run(&["open"]);
    assert_eq!(c, 8, "{err}");
    assert!(err.contains("0700"), "{err}");
}

#[test]
fn uncreatable_runtime_dir_exits_8() {
    let a = Machine::new();
    // a path below a regular file can never be created
    let file = a.dir.path().join("afile");
    std::fs::write(&file, "x").unwrap();
    a.set_env("WALKIE_RUNTIME_DIR", file.join("rt").to_str().unwrap());
    let (c, _, err) = a.run(&["open"]);
    assert_eq!(c, 8, "{err}");
}

#[test]
fn symlinked_runtime_dir_exits_8() {
    let a = Machine::new();
    let real = a.dir.path().join("real");
    std::fs::create_dir(&real).unwrap();
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(&real, std::fs::Permissions::from_mode(0o700)).unwrap();
    std::os::unix::fs::symlink(&real, a.dir.path().join("rt")).unwrap();
    assert_eq!(a.run(&["open"]).0, 8);
}

#[test]
fn failed_open_leaves_no_phantom_room() {
    let a = Machine::new();
    let rt = a.dir.path().join("rt");
    std::fs::create_dir_all(&rt).unwrap();
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(&rt, std::fs::Permissions::from_mode(0o755)).unwrap();
    assert_eq!(a.run(&["open"]).0, 8);
    let (_, out, _) = a.run(&["rooms"]);
    assert!(out.trim().is_empty(), "{out}");
}
