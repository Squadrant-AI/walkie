//! The per-room daemon: an actor that owns the `Session`, the ticket and (at most) one live
//! link. Everything that can block (accepting, handshakes, dialing, reading, writing, IPC
//! connections) runs in its own task and talks to the core through one event channel, so a
//! silent or hostile peer can never stall the core loop.

use crate::exit::{exit_of, fail, Exit};
use crate::invite::{token_from_b32, tokens_equal, Invite};
use crate::ipc::{check_peer_uid, Client, Req, Resp, StatusInfo};
use crate::model::{PeerState, Role};
use crate::net::{self, Link, PeerAddr};
use crate::paths;
use crate::proto::{read_frame, write_frame, Wire};
use crate::session::Session;
use crate::ticket::{self, now_unix, Ticket};
use anyhow::{Context, Result};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime};
use tokio::net::{UnixListener, UnixStream};
use tokio::sync::{mpsc, oneshot};
use tokio::task::JoinHandle;

const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(10);
const DIAL_ATTEMPT_TIMEOUT: Duration = Duration::from_secs(10);
/// Deadline for `join` / `rejoin` to connect (spec §5). The CLI waits 2 s longer (preflight D16).
pub const CONNECT_DEADLINE: Duration = Duration::from_secs(30);
const ONLINE_TIMEOUT: Duration = Duration::from_secs(10);
const LEAVE_WAIT: Duration = Duration::from_secs(2);
const TERMINAL_EXIT_MAX: Duration = Duration::from_secs(60);
const AFTER_INFO_EXIT: Duration = Duration::from_secs(3);
/// A link with no frame for this long, when a new link from the pinned peer arrives, counts as
/// idle: the newest link wins (spec §6.2, preflight D6).
const IDLE_LINK: Duration = Duration::from_secs(10);
const TOUCH_EVERY: Duration = Duration::from_secs(60);
const TICK: Duration = Duration::from_millis(100);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Mode {
    Open,
    Join,
    Rejoin,
}

impl Mode {
    pub fn parse(s: &str) -> Result<Mode> {
        match s {
            "open" => Ok(Mode::Open),
            "join" => Ok(Mode::Join),
            "rejoin" => Ok(Mode::Rejoin),
            other => Err(fail(Exit::Error, format!("unknown daemon mode {other}"))),
        }
    }
}

/// Pure accept-auth decision (spec §6.1).
pub fn authorize(
    role: Role,
    pinned: Option<&str>,
    stored_token: Option<&[u8; 16]>,
    invite_expires_at: Option<u64>,
    now: u64,
    remote_id: &str,
    presented: Option<&str>,
) -> bool {
    if let Some(p) = pinned {
        return p == remote_id;
    }
    if role != Role::Host {
        return false;
    }
    match (
        stored_token,
        presented.and_then(token_from_b32),
        invite_expires_at,
    ) {
        (Some(s), Some(p), Some(exp)) if now < exp => tokens_equal(s, &p),
        _ => false,
    }
}

/// Auth inputs shared with the spawned accept-handshake tasks. The lock is only ever held for
/// the synchronous decision, never across an await.
struct Auth {
    role: Role,
    pinned: Option<String>,
    token: Option<[u8; 16]>,
    invite_expires_at: Option<u64>,
}

enum CoreEvent {
    Ipc(Req, oneshot::Sender<Resp>),
    Wait {
        id: u64,
        reply: oneshot::Sender<Resp>,
    },
    CancelWait(u64),
    Ready(Result<iroh::Endpoint>),
    /// A completed server-side handshake (accepted connection).
    Accepted(Link, String),
    /// The server-side auth closure accepted a first-time token: pin and persist.
    Pinned(String),
    DialDone(Result<(Link, String)>),
    Wire(u64, Wire),
    LinkDown(u64),
}

struct Cur {
    epoch: u64,
    conn: iroh::endpoint::Connection,
    dialed_by: String,
    wtx: mpsc::UnboundedSender<Wire>,
    awaiting_resume: bool,
    last_rx: Instant,
}

struct Leaving {
    reply: Option<oneshot::Sender<Resp>>,
    deadline: Instant,
}

struct Core {
    room: String,
    mode: Mode,
    session: Session,
    ticket: Ticket,
    finished: bool,
    auth: Arc<Mutex<Auth>>,
    tx: mpsc::UnboundedSender<CoreEvent>,
    ep: Option<iroh::Endpoint>,
    invite: Option<String>,
    current: Option<Cur>,
    epoch: u64,
    waiter: Option<(u64, oneshot::Sender<Resp>)>,
    dialer: Option<JoinHandle<()>>,
    exit_at: Option<Instant>,
    rejected: Option<String>,
    unreachable: bool,
    env_error: Option<String>,
    leaving: Option<Leaving>,
    last_touch: Instant,
}

static WAITER_SEQ: AtomicU64 = AtomicU64::new(1);

pub async fn run(room: String, mode: Mode) -> Result<()> {
    let _ = tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("walkie=info")),
        )
        .with_ansi(false)
        .with_writer(std::io::stderr)
        .try_init();

    let pid_path = paths::pid_path(&room)?;
    std::fs::write(&pid_path, std::process::id().to_string())?;
    let sock_path = paths::socket_path(&room)?;
    let result = run_inner(&room, mode, &sock_path).await;
    let _ = std::fs::remove_file(&sock_path);
    let _ = std::fs::remove_file(&pid_path);
    if let Err(e) = &result {
        tracing::error!("daemon failed: {e:#}");
    }
    result
}

async fn bind_socket(sock_path: &std::path::Path) -> Result<UnixListener> {
    if sock_path.exists() {
        if UnixStream::connect(sock_path).await.is_ok() {
            return Err(fail(Exit::Error, "another daemon already serves this room"));
        }
        let _ = std::fs::remove_file(sock_path);
    }
    UnixListener::bind(sock_path)
        .map_err(|e| fail(Exit::Environment, format!("cannot create socket: {e}")))
}

async fn run_inner(room: &str, mode: Mode, sock_path: &std::path::Path) -> Result<()> {
    let ticket = Ticket::load(room)?;
    let listener = bind_socket(sock_path).await?;
    let now = SystemTime::now();
    let session = match mode {
        Mode::Open => Session::new(Role::Host, PeerState::NotJoined, now),
        Mode::Join => Session::new(Role::Joiner, PeerState::NotJoined, now),
        Mode::Rejoin => Session::new(ticket.role, PeerState::Disconnected, now),
    };
    let (tx, mut rx) = mpsc::unbounded_channel::<CoreEvent>();
    let auth = Arc::new(Mutex::new(Auth {
        role: ticket.role,
        pinned: ticket.peer_id.clone(),
        token: ticket.token.as_deref().and_then(token_from_b32),
        invite_expires_at: ticket.invite_expires_at,
    }));

    // IPC accept loop
    {
        let tx = tx.clone();
        tokio::spawn(async move {
            while let Ok((s, _)) = listener.accept().await {
                tokio::spawn(serve_conn(s, tx.clone()));
            }
        });
    }
    // endpoint startup runs beside the core so `Info` can be served meanwhile
    {
        let tx = tx.clone();
        let secret = ticket.secret_key.clone();
        let relay_only = ticket.relay_only;
        tokio::spawn(async move {
            let r = async {
                let ep = net::bind(&secret, relay_only).await?;
                if mode == Mode::Open {
                    net::wait_online(&ep, ONLINE_TIMEOUT).await?;
                }
                Ok(ep)
            }
            .await;
            let _ = tx.send(CoreEvent::Ready(r));
        });
    }

    let mut core = Core {
        room: room.to_string(),
        mode,
        session,
        ticket,
        finished: false,
        auth,
        tx,
        ep: None,
        invite: None,
        current: None,
        epoch: 0,
        waiter: None,
        dialer: None,
        exit_at: None,
        rejected: None,
        unreachable: false,
        env_error: None,
        leaving: None,
        last_touch: Instant::now(),
    };

    let mut ticker = tokio::time::interval(TICK);
    ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    let mut sigterm = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())?;
    let mut tick_n: u64 = 0;
    loop {
        tokio::select! {
            Some(ev) = rx.recv() => core.handle(ev),
            _ = ticker.tick() => {
                tick_n += 1;
                if tick_n % 10 == 0 { core.slow_tick(); }
            }
            _ = sigterm.recv() => core.begin_leave(None),
        }
        core.poll_leaving();
        if core.exit_at.is_some_and(|t| Instant::now() >= t) {
            break;
        }
    }
    core.shutdown().await;
    Ok(())
}

async fn serve_conn(stream: UnixStream, tx: mpsc::UnboundedSender<CoreEvent>) {
    if check_peer_uid(&stream).is_err() {
        return;
    }
    let mut c = Client::from_stream(stream);
    let gone = || Resp::Err {
        exit: Exit::Error.code(),
        message: "daemon is shutting down".into(),
    };
    while let Ok(req) = c.recv_req().await {
        let resp = match req {
            Req::Wait { timeout_ms } => {
                let id = WAITER_SEQ.fetch_add(1, Ordering::Relaxed);
                let (rtx, rrx) = oneshot::channel();
                if tx.send(CoreEvent::Wait { id, reply: rtx }).is_err() {
                    break;
                }
                match tokio::time::timeout(Duration::from_millis(timeout_ms), rrx).await {
                    Ok(Ok(r)) => r,
                    Ok(Err(_)) => gone(),
                    Err(_) => {
                        let _ = tx.send(CoreEvent::CancelWait(id));
                        Resp::Timeout
                    }
                }
            }
            other => {
                let (rtx, rrx) = oneshot::channel();
                if tx.send(CoreEvent::Ipc(other, rtx)).is_err() {
                    break;
                }
                rrx.await.unwrap_or_else(|_| gone())
            }
        };
        if c.send_resp(&resp).await.is_err() {
            break;
        }
    }
}

impl Core {
    fn handle(&mut self, ev: CoreEvent) {
        match ev {
            CoreEvent::Ipc(req, reply) => self.on_ipc(req, reply),
            CoreEvent::Wait { id, reply } => self.on_wait(id, reply),
            CoreEvent::CancelWait(id) => {
                if self.waiter.as_ref().is_some_and(|(w, _)| *w == id) {
                    self.waiter = None;
                }
            }
            CoreEvent::Ready(r) => self.on_ready(r),
            CoreEvent::Accepted(link, addr) => self.on_new_link(link, addr),
            CoreEvent::Pinned(id) => {
                self.ticket.peer_id = Some(id);
                self.ticket.token = None;
                self.ticket.invite_expires_at = None;
                self.invite = None;
                self.save();
            }
            CoreEvent::DialDone(r) => self.on_dial_done(r),
            CoreEvent::Wire(epoch, w) => self.on_wire(epoch, w),
            CoreEvent::LinkDown(epoch) => self.on_link_down(epoch),
        }
        self.serve_waiter();
    }

    // ---------- persistence ----------

    fn save(&self) {
        if !self.finished {
            if let Err(e) = self.ticket.save() {
                tracing::warn!("cannot save ticket: {e:#}");
            }
        }
    }

    fn touch_save(&mut self) {
        self.ticket.touch(now_unix());
        self.last_touch = Instant::now();
        self.save();
    }

    /// The room is over: tombstone, delete the ticket, never save it again.
    fn finish_room(&mut self) {
        ticket::mark_ended(&self.room);
        Ticket::delete(&self.room);
        self.finished = true;
    }

    // ---------- startup ----------

    fn on_ready(&mut self, r: Result<iroh::Endpoint>) {
        let ep = match r {
            Ok(ep) => ep,
            Err(e) => {
                tracing::error!("endpoint start failed: {e:#}");
                self.env_error = Some(format!("{e:#}"));
                return;
            }
        };
        let my_addr = net::my_addr(&ep);
        self.ep = Some(ep.clone());
        // accept loop: handshakes run in their own tasks with a timeout (preflight D5)
        {
            let tx = self.tx.clone();
            let auth = self.auth.clone();
            tokio::spawn(accept_loop(ep, auth, tx));
        }
        match self.mode {
            Mode::Open => {
                if let Some(tok) = self.ticket.token.as_deref().and_then(token_from_b32) {
                    self.invite = Some(
                        Invite {
                            ticket: my_addr,
                            token: tok,
                            relay_only: self.ticket.relay_only,
                        }
                        .encode(),
                    );
                }
            }
            Mode::Join => {
                let tok = self.ticket.token.clone();
                self.spawn_dialer(tok, Some(Instant::now() + CONNECT_DEADLINE));
            }
            Mode::Rejoin => {
                self.spawn_dialer(None, Some(Instant::now() + CONNECT_DEADLINE));
            }
        }
    }

    fn spawn_dialer(&mut self, token: Option<String>, deadline: Option<Instant>) {
        let Some(ep) = self.ep.clone() else { return };
        if self.dialer.is_some() {
            return;
        }
        let peer = PeerAddr {
            id: self.ticket.peer_id.clone(),
            ticket: self.ticket.peer_addr.clone(),
        };
        let tx = self.tx.clone();
        self.dialer = Some(tokio::spawn(async move {
            let r = dial_loop(ep, peer, token, deadline).await;
            let _ = tx.send(CoreEvent::DialDone(r));
        }));
    }

    fn on_dial_done(&mut self, r: Result<(Link, String)>) {
        self.dialer = None;
        match r {
            Ok((link, addr)) => {
                // preflight D7: the joiner pins the host's id after its first handshake
                if self.ticket.peer_id.is_none() {
                    self.ticket.peer_id = Some(link.remote_id.clone());
                    self.ticket.token = None;
                    self.ticket.invite_expires_at = None;
                    if let Ok(mut a) = self.auth.lock() {
                        a.pinned = Some(link.remote_id.clone());
                        a.token = None;
                    }
                }
                self.on_new_link(link, addr);
            }
            Err(e) if exit_of(&e) == Exit::BadInvite => {
                self.rejected = Some(format!("{e:#}"));
                Ticket::delete(&self.room);
                self.finished = true;
            }
            Err(e) => {
                tracing::warn!("giving up dialing: {e:#}");
                self.unreachable = true;
            }
        }
    }

    // ---------- links ----------

    fn tx_wire(&self, w: Wire) {
        if let Some(cur) = &self.current {
            if !cur.awaiting_resume {
                let _ = cur.wtx.send(w);
            }
        }
    }

    fn on_new_link(&mut self, link: Link, peer_addr: String) {
        self.ticket.peer_addr = Some(peer_addr);
        self.save();
        if self.session.peer == PeerState::Ended {
            link.conn.close(1u32.into(), b"ended");
            return;
        }
        if let Some(cur) = &self.current {
            let dead = cur.conn.close_reason().is_some()
                || matches!(
                    self.session.peer,
                    PeerState::Left | PeerState::Disconnected | PeerState::NotJoined
                )
                || cur.last_rx.elapsed() > IDLE_LINK;
            // both live: keep the one dialed by the lower id (the peer applies the same rule)
            if !dead && net::keep(&cur.dialed_by, &link.dialed_by) {
                link.conn.close(2u32.into(), b"superseded");
                return;
            }
        }
        if let Some(old) = self.current.take() {
            old.conn.close(2u32.into(), b"superseded");
        }
        if let Some(d) = self.dialer.take() {
            d.abort();
        }
        self.epoch += 1;
        let epoch = self.epoch;
        let Link {
            conn,
            send,
            recv,
            dialed_by,
            ..
        } = link;
        // writer task: the core never awaits on the network
        let (wtx, mut wrx) = mpsc::unbounded_channel::<Wire>();
        {
            let tx = self.tx.clone();
            let mut send = send;
            tokio::spawn(async move {
                while let Some(w) = wrx.recv().await {
                    if write_frame(&mut send, &w).await.is_err() {
                        let _ = tx.send(CoreEvent::LinkDown(epoch));
                        break;
                    }
                }
            });
        }
        {
            let tx = self.tx.clone();
            let mut recv = recv;
            tokio::spawn(async move {
                loop {
                    match read_frame(&mut recv).await {
                        Ok(w) => {
                            if tx.send(CoreEvent::Wire(epoch, w)).is_err() {
                                break;
                            }
                        }
                        Err(_) => {
                            let _ = tx.send(CoreEvent::LinkDown(epoch));
                            break;
                        }
                    }
                }
            });
        }
        // first frame is our Resume; nothing else until the peer's Resume arrives
        let _ = wtx.send(self.session.resume_frame());
        self.current = Some(Cur {
            epoch,
            conn,
            dialed_by,
            wtx,
            awaiting_resume: true,
            last_rx: Instant::now(),
        });
    }

    fn on_wire(&mut self, epoch: u64, w: Wire) {
        let now = SystemTime::now();
        let Some(cur) = self.current.as_mut().filter(|c| c.epoch == epoch) else {
            return;
        };
        cur.last_rx = Instant::now();
        match w {
            Wire::Resume {
                recv_highest,
                send_base,
                read_upto,
            } => {
                if !cur.awaiting_resume {
                    return;
                }
                cur.awaiting_resume = false;
                let path = net::path_of(&cur.conn);
                for f in self
                    .session
                    .on_peer_resume(recv_highest, send_base, read_upto)
                {
                    self.tx_wire(f);
                }
                self.session.on_connected(path, now);
                if self.waiter.is_some() {
                    self.tx_wire(Wire::Waiting);
                }
                self.touch_save();
            }
            other => {
                let is_end = matches!(other, Wire::End);
                for f in self.session.on_wire(other, now) {
                    self.tx_wire(f);
                }
                if is_end {
                    self.finish_room();
                    self.schedule_terminal_exit();
                }
            }
        }
    }

    fn on_link_down(&mut self, epoch: u64) {
        if self.current.as_ref().map(|c| c.epoch) != Some(epoch) {
            return;
        }
        if let Some(cur) = self.current.take() {
            cur.conn.close(0u32.into(), b"");
        }
        self.session.on_disconnected(SystemTime::now());
        self.touch_save();
        self.maybe_redial();
    }

    /// Both sides dial while `Disconnected`; the tie-break resolves collisions (spec §6.2).
    fn maybe_redial(&mut self) {
        if self.session.peer == PeerState::Disconnected
            && self.current.is_none()
            && self.leaving.is_none()
            && self.rejected.is_none()
            && !self.unreachable
        {
            self.spawn_dialer(None, None);
        }
    }

    // ---------- waiter ----------

    fn on_wait(&mut self, id: u64, reply: oneshot::Sender<Resp>) {
        if let Some((_, old)) = self.waiter.take() {
            let _ = old.send(Resp::Preempted);
        }
        self.tx_wire(Wire::Waiting);
        if let Some(b) = self.session.take_batch() {
            let _ = reply.send(Resp::Batch {
                batch_id: b.id,
                items: b.items,
            });
        } else {
            self.waiter = Some((id, reply));
        }
    }

    fn serve_waiter(&mut self) {
        if self.waiter.is_none() {
            return;
        }
        if let Some(b) = self.session.take_batch() {
            if let Some((_, w)) = self.waiter.take() {
                let _ = w.send(Resp::Batch {
                    batch_id: b.id,
                    items: b.items,
                });
            }
        }
    }

    // ---------- IPC ----------

    fn on_ipc(&mut self, req: Req, reply: oneshot::Sender<Resp>) {
        let resp = match req {
            Req::Info => {
                if (self.rejected.is_some() || self.unreachable || self.env_error.is_some())
                    && self.exit_at.is_none()
                {
                    self.exit_at = Some(Instant::now() + AFTER_INFO_EXIT);
                }
                Resp::Info {
                    room: self.room.clone(),
                    invite: self.invite.clone(),
                    peer: self.session.peer,
                    rejected: self.rejected.clone(),
                    unreachable: self.unreachable,
                    env_error: self.env_error.clone(),
                }
            }
            Req::Send { body, reply_to } => match self.session.send(&body, reply_to) {
                Ok((id, wire)) => {
                    self.tx_wire(wire);
                    Resp::Sent { id }
                }
                Err(e) => Resp::Err {
                    exit: exit_of(&e).code(),
                    message: format!("{e:#}"),
                },
            },
            Req::Wait { .. } => Resp::Err {
                exit: Exit::Error.code(),
                message: "wait is handled by the connection task".into(),
            },
            Req::AckBatch { batch_id } => {
                if let Some(acked) = self.session.ack_batch(batch_id) {
                    if let Some(w) = acked.read {
                        self.tx_wire(w);
                    }
                    if acked.terminal {
                        // let this reply and the final AckRead flush first
                        let t = Instant::now() + Duration::from_millis(300);
                        self.exit_at = Some(self.exit_at.map_or(t, |e| e.min(t)));
                    }
                }
                Resp::Ok
            }
            Req::Status => Resp::Status(self.status()),
            Req::Leave => {
                self.begin_leave(Some(reply));
                return;
            }
            Req::End => {
                // control frame: sent even before the peer's Resume has arrived
                if let Some(cur) = &self.current {
                    let _ = cur.wtx.send(Wire::End);
                }
                self.finish_room();
                self.session.on_local_end(SystemTime::now());
                self.exit_at = Some(Instant::now() + Duration::from_millis(300));
                Resp::Ok
            }
        };
        let _ = reply.send(resp);
    }

    fn status(&self) -> StatusInfo {
        StatusInfo {
            room: self.room.clone(),
            role: self.session.role,
            peer: self.session.peer,
            peer_since: self.session.peer_since,
            peer_last_waited: self.session.peer_last_waited,
            path: self.current.as_ref().map(|c| {
                match net::path_of(&c.conn) {
                    crate::model::Path::Direct => "direct",
                    crate::model::Path::Relay => "relay",
                }
                .to_string()
            }),
            last_link_drop: self.session.last_link_drop,
            invite_expires_at: if self.session.peer == PeerState::NotJoined {
                self.ticket.invite_expires_at
            } else {
                None
            },
            unread: self.session.unread(),
            outbound: self.session.outbound(),
        }
    }

    // ---------- leave / exit ----------

    fn begin_leave(&mut self, reply: Option<oneshot::Sender<Resp>>) {
        if self.leaving.is_some() {
            if let Some(r) = reply {
                let _ = r.send(Resp::Err {
                    exit: Exit::Error.code(),
                    message: "already leaving".into(),
                });
            }
            return;
        }
        // Leave is sent even if the peer hasn't sent Resume yet: it is a one-off control frame
        if let Some(cur) = &self.current {
            let _ = cur.wtx.send(Wire::Leave);
        }
        if let Some(d) = self.dialer.take() {
            d.abort();
        }
        self.touch_save();
        self.leaving = Some(Leaving {
            reply,
            deadline: Instant::now() + LEAVE_WAIT,
        });
    }

    /// Leave waits until the peer has *stored* everything we sent (not until it has read it),
    /// at most 2 s (preflight D13).
    fn poll_leaving(&mut self) {
        let Some(l) = &self.leaving else { return };
        let acked = self.session.peer_stored_upto >= self.session.last_sent_seq();
        if self.current.is_some() && !acked && Instant::now() < l.deadline {
            return;
        }
        let l = self.leaving.take().unwrap();
        let (delivered_unread, undelivered) = self.session.leave_report();
        if let Some(r) = l.reply {
            let _ = r.send(Resp::LeaveReport {
                delivered_unread,
                undelivered,
            });
        }
        self.exit_at = Some(Instant::now() + Duration::from_millis(300));
    }

    /// Exit after a `wait` client acked the terminal event, or after at most 60 s.
    fn schedule_terminal_exit(&mut self) {
        // test hook so the 60 s bound can be exercised quickly
        let max = std::env::var("WALKIE_TEST_TERMINAL_EXIT_SECS")
            .ok()
            .and_then(|s| s.parse().ok())
            .map(Duration::from_secs)
            .unwrap_or(TERMINAL_EXIT_MAX);
        let t = Instant::now() + max;
        self.exit_at = Some(self.exit_at.map_or(t, |e| e.min(t)));
    }

    async fn shutdown(&mut self) {
        // flush queued writes, then close the endpoint gracefully (bounded)
        if let Some(ep) = self.ep.take() {
            let _ = tokio::time::timeout(Duration::from_secs(2), ep.close()).await;
        }
    }

    // ---------- timers (once a second) ----------

    fn slow_tick(&mut self) {
        let now = now_unix();
        let ended = self.session.peer == PeerState::Ended;
        if !ended
            && self.session.role == Role::Host
            && self.session.peer == PeerState::NotJoined
            && self.ticket.invite_expires_at.is_some_and(|e| now >= e)
            && self.ep.is_some()
        {
            self.session.on_invite_expired(SystemTime::now());
            self.finish_room();
            self.schedule_terminal_exit();
            self.serve_waiter();
            return;
        }
        if !ended
            && !matches!(self.session.peer, PeerState::Connected { .. })
            && self.leaving.is_none()
            && self.ticket.is_expired(now)
        {
            self.session.on_local_end(SystemTime::now());
            self.finish_room();
            self.schedule_terminal_exit();
            self.serve_waiter();
            return;
        }
        if matches!(self.session.peer, PeerState::Connected { .. })
            && self.last_touch.elapsed() >= TOUCH_EVERY
        {
            self.touch_save();
        }
    }
}

async fn accept_loop(
    ep: iroh::Endpoint,
    auth: Arc<Mutex<Auth>>,
    tx: mpsc::UnboundedSender<CoreEvent>,
) {
    while let Some(incoming) = ep.accept().await {
        let ep = ep.clone();
        let auth = auth.clone();
        let tx = tx.clone();
        tokio::spawn(async move {
            let res = tokio::time::timeout(HANDSHAKE_TIMEOUT, async {
                let conn = incoming.await.context("incoming connection failed")?;
                let tx2 = tx.clone();
                let mut check = move |remote: &str, tok: Option<&str>| -> bool {
                    let Ok(mut a) = auth.lock() else { return false };
                    let ok = authorize(
                        a.role,
                        a.pinned.as_deref(),
                        a.token.as_ref(),
                        a.invite_expires_at,
                        now_unix(),
                        remote,
                        tok,
                    );
                    if ok && a.pinned.is_none() {
                        a.pinned = Some(remote.to_string());
                        a.token = None;
                        a.invite_expires_at = None;
                        let _ = tx2.send(CoreEvent::Pinned(remote.to_string()));
                    }
                    ok
                };
                net::server_handshake(&conn, &mut check, net::my_addr(&ep)).await
            })
            .await;
            match res {
                Ok(Ok((link, addr))) => {
                    let _ = tx.send(CoreEvent::Accepted(link, addr));
                }
                Ok(Err(e)) => tracing::debug!("inbound handshake failed: {e:#}"),
                Err(_) => tracing::debug!("inbound handshake timed out"),
            }
        });
    }
}

async fn dial_loop(
    ep: iroh::Endpoint,
    peer: PeerAddr,
    token: Option<String>,
    deadline: Option<Instant>,
) -> Result<(Link, String)> {
    let mut backoff = 1u64;
    loop {
        if deadline.is_some_and(|d| Instant::now() >= d) {
            return Err(fail(Exit::Unreachable, "peer unreachable"));
        }
        let attempt = async {
            let conn = tokio::time::timeout(DIAL_ATTEMPT_TIMEOUT, net::dial(&ep, &peer))
                .await
                .map_err(|_| anyhow::anyhow!("dial timed out"))??;
            tokio::time::timeout(
                HANDSHAKE_TIMEOUT,
                net::client_handshake(&conn, token.clone(), net::my_addr(&ep)),
            )
            .await
            .map_err(|_| anyhow::anyhow!("handshake timed out"))?
        };
        match attempt.await {
            Ok(ok) => return Ok(ok),
            Err(e) if exit_of(&e) == Exit::BadInvite => return Err(e),
            Err(e) => tracing::debug!("dial attempt failed: {e:#}"),
        }
        let mut wait = Duration::from_secs(backoff);
        if let Some(d) = deadline {
            wait = wait.min(d.saturating_duration_since(Instant::now()));
        }
        tokio::time::sleep(wait).await;
        backoff = (backoff * 2).min(30);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::invite::token_b32;
    use crate::model::Role;
    #[test]
    fn authorize_rules() {
        let tok = [9u8; 16];
        let b32 = token_b32(&tok);
        assert!(authorize(
            Role::Host,
            None,
            Some(&tok),
            Some(100),
            50,
            "B",
            Some(&b32)
        ));
        assert!(
            !authorize(
                Role::Host,
                None,
                Some(&tok),
                Some(100),
                150,
                "B",
                Some(&b32)
            ),
            "expired"
        );
        assert!(!authorize(
            Role::Host,
            None,
            Some(&tok),
            Some(100),
            50,
            "B",
            Some("wrong")
        ));
        assert!(
            !authorize(Role::Host, None, None, None, 50, "B", Some(&b32)),
            "token already used"
        );
        assert!(
            authorize(Role::Host, Some("B"), None, None, 50, "B", None),
            "pinned reconnect, no token"
        );
        assert!(
            !authorize(Role::Host, Some("B"), None, None, 50, "C", Some(&b32)),
            "stranger after pin"
        );
        assert!(
            authorize(Role::Joiner, Some("A"), None, None, 50, "A", None),
            "joiner accepts pinned host"
        );
        assert!(!authorize(Role::Joiner, None, None, None, 50, "X", None));
    }

    #[test]
    fn mode_parse() {
        assert_eq!(Mode::parse("open").unwrap(), Mode::Open);
        assert_eq!(Mode::parse("rejoin").unwrap(), Mode::Rejoin);
        assert!(Mode::parse("x").is_err());
    }
}
