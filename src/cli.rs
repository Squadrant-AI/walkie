//! Client-side commands. Each one talks to the per-room daemon over its Unix socket.

use crate::exit::{fail, Exit};
use crate::frame::{exit_for, render_json, render_text};
use crate::invite::{self, Invite};
use crate::ipc::{Client, Req, Resp, StatusInfo};
use crate::model::{PeerState, Role};
use crate::net::new_secret;
use crate::paths;
use crate::ticket::{self, now_unix, Ticket};
use anyhow::{Context, Result};
use std::io::Write;
use std::os::unix::process::CommandExt;
use std::time::{Duration, SystemTime};

/// The daemon's own deadlines are 10 s (open) and 30 s (join / rejoin); the CLI waits 2 s longer
/// so the daemon reports `env_error` / `unreachable` before the CLI gives up (preflight D16).
const OPEN_WAIT: Duration = Duration::from_secs(12);
const CONNECT_WAIT: Duration = Duration::from_secs(32);

pub fn choose(explicit: Option<String>, active: &[String]) -> Result<String> {
    if let Some(r) = explicit {
        return Ok(r);
    }
    match active {
        [one] => Ok(one.clone()),
        [] => Err(fail(
            Exit::NoRoom,
            "no active room; use `walkie open`, `walkie join` or `walkie rejoin`",
        )),
        many => Err(fail(
            Exit::NoRoom,
            format!(
                "multiple active rooms: {} — pass --room <id> or set WALKIE_ROOM",
                many.join(", ")
            ),
        )),
    }
}

async fn is_active(room: &str) -> bool {
    Client::connect(room).await.is_ok()
}

async fn active_rooms() -> Vec<String> {
    let mut out = vec![];
    for t in ticket::list_live(now_unix()) {
        if is_active(&t.room).await {
            out.push(t.room);
        }
    }
    out
}

pub async fn select_room(explicit: Option<String>) -> Result<String> {
    if explicit.is_some() {
        return choose(explicit, &[]);
    }
    choose(None, &active_rooms().await)
}

/// Sends one request; a daemon-side `Err` becomes the matching exit code.
async fn call(room: &str, req: &Req) -> Result<Resp> {
    let mut c = Client::connect(room).await?;
    check(c.call(req).await?)
}

fn check(r: Resp) -> Result<Resp> {
    match r {
        Resp::Err { exit, message } => Err(fail(Exit::from_code(exit), message)),
        other => Ok(other),
    }
}

// ---------- daemon spawning ----------

/// Re-executes this binary as a detached daemon. Returns its pid.
fn spawn_daemon(room: &str, mode: &str) -> Result<u32> {
    let env_err = |what: &str, e: &dyn std::fmt::Display| {
        fail(
            Exit::Environment,
            format!("cannot start the walkie daemon ({what}: {e}); the environment may forbid background processes"),
        )
    };
    let exe = std::env::current_exe().map_err(|e| env_err("current_exe", &e))?;
    let log_path = paths::log_path(room)?;
    let log = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&log_path)
        .map_err(|e| env_err("log file", &e))?;
    let mut cmd = std::process::Command::new(exe);
    cmd.args(["__daemon", room, "--mode", mode])
        .stdin(std::process::Stdio::null())
        .stdout(log.try_clone().map_err(|e| env_err("log file", &e))?)
        .stderr(log);
    // SAFETY: setsid is async-signal-safe and touches no Rust state.
    unsafe {
        cmd.pre_exec(|| {
            libc::setsid();
            Ok(())
        });
    }
    let child = cmd.spawn().map_err(|e| env_err("spawn", &e))?;
    Ok(child.id())
}

fn terminate(pid: u32) {
    // SIGTERM = the daemon's leave path, so it cleans up its socket and pid file
    unsafe {
        libc::kill(pid as i32, libc::SIGTERM);
    }
}

/// Polls the daemon's `Info` every 100 ms until `f` decides, or `total` passes.
async fn poll_info<T>(
    room: &str,
    total: Duration,
    f: impl Fn(&Resp) -> Option<Result<T>>,
) -> Option<Result<T>> {
    let deadline = tokio::time::Instant::now() + total;
    while tokio::time::Instant::now() < deadline {
        tokio::time::sleep(Duration::from_millis(100)).await;
        let Ok(mut c) = Client::connect(room).await else {
            continue;
        };
        let Ok(r) = c.call(&Req::Info).await else {
            continue;
        };
        if let Some(out) = f(&r) {
            return Some(out);
        }
    }
    None
}

// ---------- commands ----------

pub async fn open(ttl: Duration, idle_ttl: Duration, relay_only: bool) -> Result<Exit> {
    let token = invite::new_token();
    let room = invite::room_id(&token);
    let now = now_unix();
    let t = Ticket {
        room: room.clone(),
        role: Role::Host,
        secret_key: new_secret(),
        relay_only,
        peer_id: None,
        peer_addr: None,
        token: Some(invite::token_b32(&token)),
        invite_expires_at: Some(now + ttl.as_secs()),
        idle_ttl_secs: idle_ttl.as_secs(),
        expires_at: now + idle_ttl.as_secs(),
    };
    t.save()?;
    let abort = |e: anyhow::Error| {
        // don't leave a phantom rejoinable room behind (preflight C11)
        Ticket::delete(&room);
        e
    };
    let pid = spawn_daemon(&room, "open").map_err(abort)?;
    let found = poll_info(&room, OPEN_WAIT, |r| match r {
        Resp::Info {
            env_error: Some(e), ..
        } => Some(Err(fail(Exit::Environment, e.clone()))),
        Resp::Info {
            invite: Some(i), ..
        } => Some(Ok(i.clone())),
        _ => None,
    })
    .await;
    let invite = match found {
        Some(Ok(i)) => i,
        Some(Err(e)) => return Err(abort(e)),
        None => {
            terminate(pid);
            return Err(abort(fail(
                Exit::Environment,
                format!(
                    "daemon did not start; see {}",
                    paths::log_path(&room)?.display()
                ),
            )));
        }
    };
    println!("room: {room}");
    println!("invite: {invite}");
    println!(
        "(expires in {}, single use — give this to the other person)",
        humantime::format_duration(ttl)
    );
    println!("tip: export WALKIE_ROOM={room}");
    Ok(Exit::Ok)
}

pub async fn join(invite_str: &str) -> Result<Exit> {
    let inv = Invite::decode(invite_str)?;
    let room = inv.room_id();
    // idempotent retry / double start: a live daemon for this room means we're already in it
    if is_active(&room).await {
        println!("already in room {room}");
        return Ok(Exit::Ok);
    }
    if Ticket::load(&room).is_err() {
        let now = now_unix();
        Ticket {
            room: room.clone(),
            role: Role::Joiner,
            secret_key: new_secret(),
            relay_only: inv.relay_only,
            peer_id: None,
            peer_addr: Some(inv.ticket.clone()),
            token: Some(invite::token_b32(&inv.token)),
            invite_expires_at: None,
            idle_ttl_secs: 86_400,
            expires_at: now + 86_400,
        }
        .save()?;
    }
    let pid = spawn_daemon(&room, "join")?;
    connect_outcome(&room, pid, "joined").await
}

async fn connect_outcome(room: &str, pid: u32, verb: &str) -> Result<Exit> {
    let out = poll_info(room, CONNECT_WAIT, |r| match r {
        Resp::Info {
            peer: PeerState::Connected { .. },
            ..
        } => Some(Ok(())),
        Resp::Info {
            rejected: Some(m), ..
        } => Some(Err(fail(Exit::BadInvite, m.clone()))),
        Resp::Info {
            env_error: Some(e), ..
        } => Some(Err(fail(Exit::Environment, e.clone()))),
        Resp::Info {
            unreachable: true, ..
        } => Some(Err(fail(
            Exit::Unreachable,
            "the other side is not reachable right now; retry later",
        ))),
        _ => None,
    })
    .await;
    match out {
        Some(Ok(())) => {
            println!("room: {room} — {verb}");
            Ok(Exit::Ok)
        }
        Some(Err(e)) => Err(e),
        None => {
            terminate(pid);
            Err(fail(Exit::Unreachable, "timed out connecting; retry later"))
        }
    }
}

pub async fn send(room: Option<String>, text: &str, reply_to: Option<String>) -> Result<Exit> {
    let room = select_room(room).await?;
    let body = if text == "-" {
        let mut s = String::new();
        std::io::Read::read_to_string(&mut std::io::stdin(), &mut s).context("reading stdin")?;
        s
    } else {
        text.to_string()
    };
    match call(&room, &Req::Send { body, reply_to }).await? {
        Resp::Sent { id } => {
            println!("{id}");
            Ok(Exit::Ok)
        }
        other => Err(fail(Exit::Error, format!("unexpected reply: {other:?}"))),
    }
}

pub async fn wait(room: Option<String>, timeout: u64, json: bool) -> Result<Exit> {
    let room = select_room(room).await?;
    let mut c = Client::connect(&room).await?;
    c.send(&Req::Wait {
        timeout_ms: timeout * 1000,
    })
    .await?;
    let resp = tokio::time::timeout(Duration::from_secs(timeout + 5), c.recv())
        .await
        .map_err(|_| fail(Exit::Error, "daemon did not answer the wait"))??;
    match check(resp)? {
        Resp::Batch { batch_id, items } => {
            let out = if json {
                render_json(&items)
            } else {
                render_text(&items)
            };
            {
                let mut so = std::io::stdout().lock();
                so.write_all(out.as_bytes())?;
                so.flush()?;
            }
            // only now does the daemon tell the peer the messages were read
            check(c.call(&Req::AckBatch { batch_id }).await?)?;
            Ok(exit_for(&items))
        }
        Resp::Timeout => Ok(Exit::Timeout),
        Resp::Preempted => Err(fail(Exit::Error, "another wait took over")),
        other => Err(fail(Exit::Error, format!("unexpected reply: {other:?}"))),
    }
}

fn ago(t: SystemTime) -> String {
    let secs = SystemTime::now()
        .duration_since(t)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    format!(
        "{} ago",
        humantime::format_duration(Duration::from_secs(secs))
    )
}

fn format_status(s: &StatusInfo) -> String {
    let role = match s.role {
        Role::Host => "host",
        Role::Joiner => "joiner",
    };
    let mut out = format!("room: {} ({role})\n", s.room);
    out.push_str(&format!(
        "peer: {} (since {}, {})",
        s.peer.name(),
        humantime::format_rfc3339_seconds(s.peer_since),
        ago(s.peer_since)
    ));
    if let Some(p) = &s.path {
        out.push_str(&format!(" · path: {p}"));
    }
    out.push('\n');
    if let Some(t) = s.peer_last_waited {
        out.push_str(&format!("peer last waited: {}\n", ago(t)));
    }
    if let Some(t) = s.last_link_drop {
        out.push_str(&format!("last link drop: {}\n", ago(t)));
    }
    if let Some(exp) = s.invite_expires_at {
        let left = exp.saturating_sub(now_unix());
        out.push_str(&format!(
            "invite expires in: {}\n",
            humantime::format_duration(Duration::from_secs(left))
        ));
    }
    out.push_str(&format!(
        "unread: {} · outbound unread-by-peer: {}\n",
        s.unread, s.outbound
    ));
    out
}

pub async fn status(room: Option<String>, json: bool) -> Result<Exit> {
    let room = select_room(room).await?;
    match call(&room, &Req::Status).await? {
        Resp::Status(s) => {
            if json {
                println!("{}", serde_json::to_string(&s)?);
            } else {
                print!("{}", format_status(&s));
            }
            Ok(Exit::Ok)
        }
        other => Err(fail(Exit::Error, format!("unexpected reply: {other:?}"))),
    }
}

pub async fn rooms() -> Result<Exit> {
    for t in ticket::list_live(now_unix()) {
        let role = match t.role {
            Role::Host => "host",
            Role::Joiner => "joiner",
        };
        let state = if is_active(&t.room).await {
            "active"
        } else {
            "rejoinable"
        };
        println!("{}  {role}  {state}", t.room);
    }
    Ok(Exit::Ok)
}

pub async fn leave(room: Option<String>) -> Result<Exit> {
    let room = select_room(room).await?;
    match call(&room, &Req::Leave).await? {
        Resp::LeaveReport {
            delivered_unread,
            undelivered,
        } => {
            println!(
                "left room {room}: {delivered_unread} delivered-unread (safe with the peer), {undelivered} undelivered (dropped). Rejoin with: walkie rejoin {room}"
            );
            Ok(Exit::Ok)
        }
        other => Err(fail(Exit::Error, format!("unexpected reply: {other:?}"))),
    }
}

pub async fn rejoin(room: Option<String>, room_arg: Option<String>) -> Result<Exit> {
    let room = match room_arg.or(room) {
        Some(r) => r,
        None => {
            let mut idle = vec![];
            for t in ticket::list_live(now_unix()) {
                if !is_active(&t.room).await {
                    idle.push(t.room);
                }
            }
            choose(None, &idle)?
        }
    };
    if is_active(&room).await {
        println!("already in room {room}");
        return Ok(Exit::Ok);
    }
    let t = match Ticket::load(&room) {
        Ok(t) => t,
        Err(e) if ticket::is_ended(&room) => {
            let _ = e;
            return Err(fail(Exit::Ended, format!("room {room} has ended")));
        }
        Err(e) => return Err(e),
    };
    if t.is_expired(now_unix()) {
        Ticket::delete(&room);
        ticket::mark_ended(&room);
        return Err(fail(Exit::Ended, format!("room {room} has expired")));
    }
    let pid = spawn_daemon(&room, "rejoin")?;
    connect_outcome(&room, pid, "rejoined").await
}

pub async fn end(room: Option<String>) -> Result<Exit> {
    let room = select_room(room).await?;
    call(&room, &Req::End).await?;
    println!("room {room} ended");
    Ok(Exit::Ok)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::exit::{exit_of, Exit};
    #[test]
    fn ambiguous_room_exits_4_and_lists() {
        let e = choose(None, &["abc123".into(), "def456".into()]).unwrap_err();
        assert_eq!(exit_of(&e), Exit::NoRoom);
        assert!(e.to_string().contains("abc123, def456"));
        assert_eq!(choose(None, &["abc123".into()]).unwrap(), "abc123");
        assert_eq!(choose(Some("x".into()), &[]).unwrap(), "x");
        assert_eq!(exit_of(&choose(None, &[]).unwrap_err()), Exit::NoRoom);
    }

    #[test]
    fn status_format_has_the_spec_lines() {
        let now = SystemTime::now();
        let s = StatusInfo {
            room: "k3f9q2".into(),
            role: Role::Host,
            peer: PeerState::NotJoined,
            peer_since: now,
            peer_last_waited: Some(now),
            path: None,
            last_link_drop: Some(now),
            invite_expires_at: Some(now_unix() + 600),
            unread: 0,
            outbound: 1,
        };
        let out = format_status(&s);
        for needle in [
            "room: k3f9q2 (host)",
            "peer: not-joined (since ",
            "peer last waited: ",
            "last link drop: ",
            "invite expires in: ",
            "unread: 0 · outbound unread-by-peer: 1",
        ] {
            assert!(out.contains(needle), "missing {needle:?} in:\n{out}");
        }
    }
}
