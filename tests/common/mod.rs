#![allow(dead_code)] // each test binary uses a different subset

use std::io::Write;
use std::process::{Command, Stdio};

/// One simulated machine: its own HOME and runtime dir, so two of them can talk over loopback.
pub struct Machine {
    pub dir: tempfile::TempDir,
    rooms: std::cell::RefCell<Vec<String>>,
    env: std::cell::RefCell<Vec<(String, String)>>,
}

impl Machine {
    pub fn new() -> Machine {
        Machine {
            dir: tempfile::tempdir().unwrap(),
            rooms: Default::default(),
            env: Default::default(),
        }
    }
    fn cmd(&self) -> Command {
        let mut c = Command::new(env!("CARGO_BIN_EXE_walkie"));
        c.env("WALKIE_HOME", self.dir.path().join("home"))
            .env("WALKIE_RUNTIME_DIR", self.dir.path().join("rt"))
            .env("WALKIE_TEST_LOCAL", "1")
            .env_remove("WALKIE_ROOM");
        for (k, v) in self.env.borrow().iter() {
            c.env(k, v);
        }
        c
    }
    /// Extra env for every later command on this machine (and the daemons it spawns).
    pub fn set_env(&self, k: &str, v: &str) {
        self.env.borrow_mut().push((k.into(), v.into()));
    }
    pub fn pid_of(&self, room: &str) -> Option<i32> {
        let f = self
            .dir
            .path()
            .join("home/rooms")
            .join(format!("{room}.pid"));
        std::fs::read_to_string(f).ok()?.trim().parse().ok()
    }
    /// Waits up to `secs` for the daemon with `pid` to be gone.
    pub fn wait_gone(pid: i32, secs: u64) -> bool {
        for _ in 0..secs * 10 {
            let alive = unsafe { libc_kill(pid, 0) } == 0;
            if !alive {
                return true;
            }
            std::thread::sleep(std::time::Duration::from_millis(100));
        }
        false
    }
    pub fn run(&self, args: &[&str]) -> (i32, String, String) {
        let o = self.cmd().args(args).output().unwrap();
        (
            o.status.code().unwrap_or(-1),
            String::from_utf8_lossy(&o.stdout).into(),
            String::from_utf8_lossy(&o.stderr).into(),
        )
    }
    pub fn run_stdin(&self, args: &[&str], input: &str) -> (i32, String, String) {
        let mut ch = self
            .cmd()
            .args(args)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        ch.stdin
            .take()
            .unwrap()
            .write_all(input.as_bytes())
            .unwrap();
        let o = ch.wait_with_output().unwrap();
        (
            o.status.code().unwrap_or(-1),
            String::from_utf8_lossy(&o.stdout).into(),
            String::from_utf8_lossy(&o.stderr).into(),
        )
    }
    pub fn track(&self, room: &str) {
        self.rooms.borrow_mut().push(room.to_string());
    }
    /// kill -9 THIS machine's daemon for `room` (both machines share the room id, so never pkill by name)
    pub fn kill9(&self, room: &str) {
        let pid_file = self
            .dir
            .path()
            .join("home")
            .join("rooms")
            .join(format!("{room}.pid"));
        let pid = std::fs::read_to_string(&pid_file).unwrap();
        std::process::Command::new("kill")
            .args(["-9", pid.trim()])
            .status()
            .unwrap();
        std::thread::sleep(std::time::Duration::from_millis(300));
    }
    /// Runs `wait --timeout 5` (at most `tries` times) until the output contains `needle`;
    /// returns everything printed. Events such as peer-joined may come in a batch of their own.
    pub fn wait_for(&self, room: &str, needle: &str, tries: usize) -> String {
        let mut all = String::new();
        for _ in 0..tries {
            let (_, out, _) = self.run(&["--room", room, "wait", "--timeout", "5"]);
            all.push_str(&out);
            if all.contains(needle) {
                return all;
            }
        }
        panic!("never saw {needle:?}; got:\n{all}");
    }
}

impl Drop for Machine {
    fn drop(&mut self) {
        for r in self.rooms.borrow().iter() {
            let _ = self.cmd().args(["--room", r, "end"]).output();
        }
    }
}

extern "C" {
    #[link_name = "kill"]
    fn libc_kill(pid: i32, sig: i32) -> i32;
}

pub fn field<'a>(out: &'a str, key: &str) -> &'a str {
    out.lines()
        .find_map(|l| l.strip_prefix(key))
        .map(str::trim)
        .unwrap()
}

/// open on `host`, join on `guest`; returns room id
pub fn pair(host: &Machine, guest: &Machine) -> String {
    let (c, out, err) = host.run(&["open"]);
    assert_eq!(c, 0, "open failed: {err}");
    let room = field(&out, "room:").to_string();
    let invite = field(&out, "invite:").to_string();
    host.track(&room);
    let (c, _, err) = guest.run(&["join", &invite]);
    assert_eq!(c, 0, "join failed: {err}");
    guest.track(&room);
    room
}
