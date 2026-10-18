use crate::exit::{fail, Exit};
use crate::file::{Chunk, Outgoing, Receiver, CHUNK_BYTES};
use crate::frame::{cost, is_valid_msg_id, sanitize, truncate_body, BATCH_BUDGET, BODY_CAP};
use crate::model::{EventKind, Item, Path, PeerState, Role};
use crate::proto::Wire;
use std::collections::VecDeque;
use std::time::SystemTime;

const OUTBOX_MAX_MSGS: usize = 1000;
const OUTBOX_MAX_BYTES: usize = 1 << 20;
/// File bytes the peer has not read yet, across all queued transfers (text has its own cap).
const OUTBOX_MAX_FILE_BYTES: usize = 16 << 20;

#[derive(Clone, Debug)]
enum OutKind {
    Text {
        reply_to: Option<String>,
        body: String,
    },
    FileChunk {
        xfer: u64,
        idx: u32,
        last: bool,
        name: String,
        size: u64,
        sha256: String,
        data: Vec<u8>,
    },
}

#[derive(Clone, Debug)]
struct OutMsg {
    seq: u64,
    kind: OutKind,
}

#[derive(Clone, Debug)]
struct InEntry {
    seq: Option<u64>,
    item: Item,
}

#[derive(Clone, Debug, PartialEq)]
pub struct Batch {
    pub id: u64,
    pub items: Vec<Item>,
}

/// Result of acking a delivered batch (preflight D14).
#[derive(Clone, Debug, PartialEq)]
pub struct Acked {
    /// Network `AckRead` to send, if the batch contained peer messages.
    pub read: Option<Wire>,
    /// The batch contained `RoomEnded` or `InviteExpired`: the daemon may now exit.
    pub terminal: bool,
}

pub struct Session {
    pub role: Role,
    next_seq: u64,
    outbox: VecDeque<OutMsg>,
    pub peer_stored_upto: u64,
    peer_read_upto: u64,
    recv_highest: u64,
    read_upto: u64,
    inbox: VecDeque<InEntry>,
    pending: Option<(u64, Vec<InEntry>)>,
    next_batch_id: u64,
    next_xfer: u64,
    files: Option<Receiver>,
    pub peer: PeerState,
    pub peer_since: SystemTime,
    pub peer_last_waited: Option<SystemTime>,
    pub last_link_drop: Option<SystemTime>,
}

impl Session {
    pub fn new(role: Role, peer: PeerState, now: SystemTime) -> Session {
        Session {
            role,
            next_seq: 1,
            outbox: VecDeque::new(),
            peer_stored_upto: 0,
            peer_read_upto: 0,
            recv_highest: 0,
            read_upto: 0,
            inbox: VecDeque::new(),
            pending: None,
            next_batch_id: 1,
            next_xfer: 1,
            files: None,
            peer,
            peer_since: now,
            peer_last_waited: None,
            last_link_drop: None,
        }
    }

    /// Where incoming files are saved; without it incoming files are reported as rejected.
    pub fn set_inbox_dir(&mut self, dir: std::path::PathBuf) {
        self.files = Some(Receiver::new(dir));
    }

    fn to_wire(m: &OutMsg) -> Wire {
        match &m.kind {
            OutKind::Text { reply_to, body } => Wire::Msg {
                seq: m.seq,
                reply_to: reply_to.clone(),
                body: body.clone(),
            },
            OutKind::FileChunk {
                xfer,
                idx,
                last,
                name,
                size,
                sha256,
                data,
            } => Wire::File {
                seq: m.seq,
                xfer: *xfer,
                idx: *idx,
                last: *last,
                name: name.clone(),
                size: *size,
                sha256: sha256.clone(),
                data: data_encoding::BASE64.encode(data),
            },
        }
    }

    pub fn send(&mut self, body: &str, reply_to: Option<String>) -> anyhow::Result<(String, Wire)> {
        if self.peer == PeerState::Ended {
            return Err(fail(Exit::Ended, "room has ended"));
        }
        if body.len() > BODY_CAP {
            return Err(fail(Exit::Error, "message too large (max 16 KiB)"));
        }
        let (count, bytes) = self
            .outbox
            .iter()
            .filter_map(|m| match &m.kind {
                OutKind::Text { body, .. } => Some(body.len()),
                _ => None,
            })
            .fold((0usize, 0usize), |(c, b), n| (c + 1, b + n));
        if count >= OUTBOX_MAX_MSGS || bytes + body.len() > OUTBOX_MAX_BYTES {
            return Err(fail(
                Exit::Error,
                "outbox full (peer has not read 1000 messages / 1 MiB); wait for the peer",
            ));
        }
        let m = OutMsg {
            seq: self.next_seq,
            kind: OutKind::Text {
                reply_to,
                body: body.to_string(),
            },
        };
        self.next_seq += 1;
        let id = format!("{}{}", self.role.prefix(), m.seq);
        let w = Self::to_wire(&m);
        self.outbox.push_back(m);
        Ok((id, w))
    }

    /// Queues a file as consecutive chunks in the normal seq space, so ordering, dedupe, acks and
    /// resend-after-reconnect are exactly those of text. The id is the last chunk's seq.
    pub fn send_file(&mut self, f: Outgoing) -> anyhow::Result<(String, Vec<Wire>)> {
        if self.peer == PeerState::Ended {
            return Err(fail(Exit::Ended, "room has ended"));
        }
        let queued: usize = self
            .outbox
            .iter()
            .map(|m| match &m.kind {
                OutKind::FileChunk { data, .. } => data.len(),
                _ => 0,
            })
            .sum();
        if queued + f.data.len() > OUTBOX_MAX_FILE_BYTES {
            return Err(fail(
                Exit::Error,
                "outbox full (peer has not read 16 MiB of files); wait for the peer",
            ));
        }
        let xfer = self.next_xfer;
        self.next_xfer += 1;
        let size = f.data.len() as u64;
        let chunks: Vec<&[u8]> = if f.data.is_empty() {
            vec![&[][..]]
        } else {
            f.data.chunks(CHUNK_BYTES).collect()
        };
        let n = chunks.len();
        let mut wires = Vec::with_capacity(n);
        for (i, data) in chunks.into_iter().enumerate() {
            let m = OutMsg {
                seq: self.next_seq,
                kind: OutKind::FileChunk {
                    xfer,
                    idx: i as u32,
                    last: i + 1 == n,
                    name: f.name.clone(),
                    size,
                    sha256: f.sha256.clone(),
                    data: data.to_vec(),
                },
            };
            self.next_seq += 1;
            wires.push(Self::to_wire(&m));
            self.outbox.push_back(m);
        }
        Ok((
            format!("{}{}", self.role.prefix(), self.next_seq - 1),
            wires,
        ))
    }

    pub fn resume_frame(&self) -> Wire {
        Wire::Resume {
            recv_highest: self.recv_highest,
            send_base: self.peer_read_upto,
            read_upto: self.read_upto,
        }
    }

    /// Peer's view: it holds my messages up to `recv_highest` (stored) and has read them up to
    /// `read_upto`; as far as it knows I have read its messages up to `send_base`.
    pub fn on_peer_resume(
        &mut self,
        recv_highest: u64,
        send_base: u64,
        read_upto: u64,
    ) -> Vec<Wire> {
        // as sender
        self.next_seq = self.next_seq.max(recv_highest + 1);
        self.peer_stored_upto = recv_highest;
        // a lost AckRead is recovered here (spec §6.2)
        self.peer_read_upto = self.peer_read_upto.max(read_upto);
        self.outbox.retain(|m| m.seq > read_upto);
        // as receiver
        self.recv_highest = self.recv_highest.max(send_base);
        self.read_upto = self.read_upto.max(send_base);
        self.outbox
            .iter()
            .filter(|m| m.seq > recv_highest)
            .map(Self::to_wire)
            .collect()
    }

    fn push_event(&mut self, kind: EventKind, at: SystemTime) {
        self.inbox.push_back(InEntry {
            seq: None,
            item: Item::Event { kind, at },
        });
    }

    fn set_peer(&mut self, p: PeerState, now: SystemTime) {
        if std::mem::discriminant(&self.peer) != std::mem::discriminant(&p) {
            self.peer_since = now;
        }
        self.peer = p;
    }

    pub fn on_wire(&mut self, w: Wire, now: SystemTime) -> Vec<Wire> {
        match w {
            Wire::Msg {
                seq,
                reply_to,
                body,
            } => {
                if seq != self.recv_highest + 1 {
                    return vec![];
                }
                self.recv_highest = seq;
                let id = format!("{}{}", self.role.peer().prefix(), seq);
                let body = truncate_body(sanitize(&body));
                // peer-supplied: only a well-formed message id may reach the frame header (D10)
                let reply_to = reply_to.filter(|r| is_valid_msg_id(r));
                self.inbox.push_back(InEntry {
                    seq: Some(seq),
                    item: Item::Message {
                        id,
                        reply_to,
                        received: now,
                        body,
                    },
                });
                vec![Wire::AckStored { seq }]
            }
            Wire::File {
                seq,
                xfer,
                idx,
                last,
                name,
                size,
                sha256,
                data,
            } => {
                if seq != self.recv_highest + 1 {
                    return vec![];
                }
                self.recv_highest = seq;
                let data = data_encoding::BASE64
                    .decode(data.as_bytes())
                    .unwrap_or_default();
                let chunk = Chunk {
                    xfer,
                    idx,
                    last,
                    name: &name,
                    size,
                    sha256: &sha256,
                    data: &data,
                };
                let done = match self.files.as_mut() {
                    Some(r) => r.on_chunk(&chunk),
                    None => last.then(|| crate::file::Received {
                        name: crate::file::sanitize_name(&name),
                        size,
                        sha256: "-".into(),
                        path: None,
                        problem: Some("this daemon has no inbox".into()),
                    }),
                };
                if let Some(r) = done {
                    self.inbox.push_back(InEntry {
                        seq: Some(seq),
                        item: Item::File {
                            id: format!("{}{}", self.role.peer().prefix(), seq),
                            received: now,
                            name: r.name,
                            size: r.size,
                            sha256: r.sha256,
                            path: r.path,
                            problem: r.problem,
                        },
                    });
                }
                vec![Wire::AckStored { seq }]
            }
            Wire::AckStored { seq } => {
                self.peer_stored_upto = self.peer_stored_upto.max(seq);
                vec![]
            }
            Wire::AckRead { seq } => {
                self.peer_read_upto = self.peer_read_upto.max(seq);
                self.peer_stored_upto = self.peer_stored_upto.max(seq);
                self.outbox.retain(|m| m.seq > seq);
                vec![]
            }
            Wire::Waiting => {
                self.peer_last_waited = Some(now);
                vec![]
            }
            Wire::Leave => {
                self.set_peer(PeerState::Left, now);
                self.push_event(EventKind::PeerLeft, now);
                vec![]
            }
            Wire::End => {
                self.on_local_end(now);
                vec![]
            }
            _ => vec![],
        }
    }

    pub fn on_connected(&mut self, path: Path, now: SystemTime) {
        let prev = self.peer;
        self.set_peer(PeerState::Connected { path }, now);
        match prev {
            // only the host is told someone joined (spec §5.3)
            PeerState::NotJoined if self.role == Role::Host => {
                self.push_event(EventKind::PeerJoined, now)
            }
            PeerState::Left => self.push_event(EventKind::PeerRejoined, now),
            _ => {}
        }
    }

    pub fn on_disconnected(&mut self, now: SystemTime) {
        if matches!(self.peer, PeerState::Connected { .. }) {
            self.set_peer(PeerState::Disconnected, now);
            self.last_link_drop = Some(now);
        }
    }

    pub fn on_local_end(&mut self, now: SystemTime) {
        if self.peer != PeerState::Ended {
            self.set_peer(PeerState::Ended, now);
            self.push_event(EventKind::RoomEnded, now);
        }
    }

    pub fn on_invite_expired(&mut self, now: SystemTime) {
        self.set_peer(PeerState::Ended, now);
        self.push_event(EventKind::InviteExpired, now);
    }

    pub fn take_batch(&mut self) -> Option<Batch> {
        if let Some((id, entries)) = &self.pending {
            return Some(Batch {
                id: *id,
                items: entries.iter().map(|e| e.item.clone()).collect(),
            });
        }
        if self.inbox.is_empty() {
            return None;
        }
        let mut used = 0;
        let mut entries = Vec::new();
        while let Some(e) = self.inbox.front() {
            let c = cost(&e.item);
            if !entries.is_empty() && used + c > BATCH_BUDGET {
                break;
            }
            used += c;
            entries.push(self.inbox.pop_front().unwrap());
        }
        let id = self.next_batch_id;
        self.next_batch_id += 1;
        let items = entries.iter().map(|e| e.item.clone()).collect();
        self.pending = Some((id, entries));
        Some(Batch { id, items })
    }

    /// `None` when `id` is not the pending batch.
    pub fn ack_batch(&mut self, id: u64) -> Option<Acked> {
        match self.pending.take() {
            Some((pid, entries)) if pid == id => {
                let terminal = entries.iter().any(|e| {
                    matches!(
                        e.item,
                        Item::Event {
                            kind: EventKind::RoomEnded | EventKind::InviteExpired,
                            ..
                        }
                    )
                });
                let read = entries.iter().filter_map(|e| e.seq).max().map(|max| {
                    self.read_upto = self.read_upto.max(max);
                    Wire::AckRead {
                        seq: self.read_upto,
                    }
                });
                Some(Acked { read, terminal })
            }
            other => {
                self.pending = other;
                None
            }
        }
    }

    pub fn leave_report(&self) -> (usize, usize) {
        let delivered = self
            .outbox
            .iter()
            .filter(|m| m.seq <= self.peer_stored_upto)
            .count();
        (delivered, self.outbox.len() - delivered)
    }

    /// Highest seq I have sent (0 if none).
    pub fn last_sent_seq(&self) -> u64 {
        self.next_seq - 1
    }

    pub fn unread(&self) -> usize {
        self.inbox.len() + self.pending.as_ref().map(|p| p.1.len()).unwrap_or(0)
    }
    pub fn outbound(&self) -> usize {
        self.outbox.len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::exit::{exit_of, Exit};
    use crate::model::{EventKind, Item, Path, PeerState, Role};
    use crate::proto::Wire;
    use std::time::{Duration, SystemTime};

    fn now() -> SystemTime {
        SystemTime::UNIX_EPOCH + Duration::from_secs(1_760_000_000)
    }
    fn connected(role: Role) -> Session {
        let mut s = Session::new(role, PeerState::Disconnected, now());
        s.on_connected(Path::Direct, now());
        s
    }
    fn msg(seq: u64, body: &str) -> Wire {
        Wire::Msg {
            seq,
            reply_to: None,
            body: body.into(),
        }
    }
    fn ids(b: &Batch) -> Vec<String> {
        b.items
            .iter()
            .filter_map(|i| match i {
                Item::Message { id, .. } => Some(id.clone()),
                _ => None,
            })
            .collect()
    }

    #[test]
    fn send_assigns_role_prefixed_ids_and_keeps_outbox_until_read_ack() {
        let mut h = connected(Role::Host);
        let (id1, w1) = h.send("a", None).unwrap();
        let (id2, _) = h.send("b", Some("j1".into())).unwrap();
        assert_eq!((id1.as_str(), id2.as_str()), ("h1", "h2"));
        assert_eq!(
            w1,
            Wire::Msg {
                seq: 1,
                reply_to: None,
                body: "a".into()
            }
        );
        h.on_wire(Wire::AckStored { seq: 2 }, now());
        assert_eq!(h.outbound(), 2, "stored ack must NOT release the outbox");
        h.on_wire(Wire::AckRead { seq: 1 }, now());
        assert_eq!(h.outbound(), 1);
    }

    #[test]
    fn receive_dedupes_and_ignores_gaps_and_acks_stored() {
        let mut j = connected(Role::Joiner);
        assert_eq!(
            j.on_wire(msg(1, "x"), now()),
            vec![Wire::AckStored { seq: 1 }]
        );
        assert!(
            j.on_wire(msg(1, "x"), now()).is_empty(),
            "duplicate dropped"
        );
        assert!(j.on_wire(msg(5, "gap"), now()).is_empty(), "gap ignored");
        assert_eq!(j.unread(), 1);
    }

    #[test]
    fn read_ack_only_after_batch_ack() {
        let mut j = connected(Role::Joiner);
        j.on_wire(msg(1, "x"), now());
        j.on_wire(msg(2, "y"), now());
        let b = j.take_batch().unwrap();
        assert_eq!(ids(&b), vec!["h1", "h2"]);
        assert_eq!(
            j.ack_batch(b.id),
            Some(Acked {
                read: Some(Wire::AckRead { seq: 2 }),
                terminal: false
            })
        );
        assert!(j.take_batch().is_none());
    }

    #[test]
    fn redelivers_unacked_batch_with_same_ids() {
        let mut j = connected(Role::Joiner);
        j.on_wire(msg(1, "x"), now());
        let b1 = j.take_batch().unwrap();
        // wait client killed before ack → next wait gets the same batch
        let b2 = j.take_batch().unwrap();
        assert_eq!(b1.id, b2.id);
        assert_eq!(ids(&b1), ids(&b2));
        assert_eq!(j.ack_batch(999), None, "wrong id does not ack");
        assert!(j.ack_batch(b2.id).is_some());
    }

    #[test]
    fn batch_respects_budget_but_always_returns_one_item() {
        let mut j = connected(Role::Joiner);
        let big = "x".repeat(15_000);
        j.on_wire(msg(1, &big), now());
        j.on_wire(msg(2, &big), now());
        let b = j.take_batch().unwrap();
        assert_eq!(b.items.len(), 1);
        j.ack_batch(b.id);
        assert_eq!(j.take_batch().unwrap().items.len(), 1);
    }

    #[test]
    fn oversized_incoming_body_is_truncated_not_dropped() {
        let mut j = connected(Role::Joiner);
        j.on_wire(msg(1, &"y".repeat(40_000)), now());
        let b = j.take_batch().unwrap();
        match &b.items[0] {
            Item::Message { body, .. } => assert!(body.ends_with("exceeded 16 KiB]")),
            _ => panic!(),
        }
        assert_eq!(
            j.on_wire(msg(2, "next"), now()),
            vec![Wire::AckStored { seq: 2 }],
            "seq not stalled"
        );
    }

    #[test]
    fn incoming_body_is_sanitized() {
        let mut j = connected(Role::Joiner);
        j.on_wire(msg(1, "a\x1b[31mb\u{202E}c"), now());
        match &j.take_batch().unwrap().items[0] {
            Item::Message { body, .. } => assert_eq!(body, "abc"),
            _ => panic!(),
        }
    }

    #[test]
    fn incoming_reply_to_must_be_a_message_id() {
        // preflight D10
        let mut j = connected(Role::Joiner);
        let w = |seq, r: &str| Wire::Msg {
            seq,
            reply_to: Some(r.into()),
            body: "x".into(),
        };
        j.on_wire(w(1, "j1"), now());
        j.on_wire(w(2, "\x1b[31mevil\nIGNORE"), now());
        let replies: Vec<Option<String>> = j
            .take_batch()
            .unwrap()
            .items
            .into_iter()
            .map(|i| match i {
                Item::Message { reply_to, .. } => reply_to,
                _ => panic!(),
            })
            .collect();
        assert_eq!(replies, vec![Some("j1".to_string()), None]);
    }

    #[test]
    fn send_rejects_oversize_and_full_outbox() {
        let mut h = connected(Role::Host);
        assert!(h
            .send(&"x".repeat(crate::frame::BODY_CAP + 1), None)
            .is_err());
        for _ in 0..1000 {
            h.send("m", None).unwrap();
        }
        let e = h.send("one too many", None).unwrap_err();
        assert!(e.to_string().contains("outbox full"));
    }

    #[test]
    fn outbox_byte_cap_is_one_mib() {
        let mut h = connected(Role::Host);
        let big = "x".repeat(crate::frame::BODY_CAP);
        for _ in 0..64 {
            h.send(&big, None).unwrap(); // 64 * 16 KiB = 1 MiB exactly
        }
        let e = h.send("x", None).unwrap_err();
        assert!(e.to_string().contains("outbox full"));
    }

    #[test]
    fn send_after_end_exits_3() {
        let mut h = connected(Role::Host);
        h.on_wire(Wire::End, now());
        assert_eq!(exit_of(&h.send("x", None).unwrap_err()), Exit::Ended);
    }

    #[test]
    fn resume_resends_only_what_peer_lacks() {
        let mut h = connected(Role::Host);
        for b in ["1", "2", "3", "4"] {
            h.send(b, None).unwrap();
        }
        h.on_wire(Wire::AckRead { seq: 1 }, now());
        // peer stored up to 3 (read 1); resend only 4
        let out = h.on_peer_resume(3, 0, 1);
        assert_eq!(
            out,
            vec![Wire::Msg {
                seq: 4,
                reply_to: None,
                body: "4".into()
            }]
        );
    }

    #[test]
    fn resume_read_upto_recovers_a_lost_ack_read() {
        // preflight D3: the AckRead never arrived, but the peer's Resume says it read up to 2.
        let mut h = connected(Role::Host);
        for b in ["1", "2", "3"] {
            h.send(b, None).unwrap();
        }
        assert_eq!(h.outbound(), 3);
        h.on_peer_resume(3, 0, 2);
        assert_eq!(h.outbound(), 1, "read messages pruned from the outbox");
        assert_eq!(h.leave_report(), (1, 0));
    }

    #[test]
    fn restarted_sender_never_reuses_seq_peer_holds_unread() {
        // fresh (restarted) host with empty outbox; peer holds h1..h5 stored, unread
        let mut h = connected(Role::Host);
        h.on_peer_resume(5, 0, 0);
        let (id, w) = h.send("after restart", None).unwrap();
        assert_eq!(id, "h6");
        assert_eq!(
            w,
            Wire::Msg {
                seq: 6,
                reply_to: None,
                body: "after restart".into()
            }
        );
    }

    #[test]
    fn restarted_receiver_accepts_from_senders_base() {
        // fresh (restarted) joiner; host says joiner already READ up to 6
        let mut j = connected(Role::Joiner);
        j.on_peer_resume(0, 6, 0);
        assert!(
            j.on_wire(msg(6, "old"), now()).is_empty(),
            "already read → dropped"
        );
        assert_eq!(
            j.on_wire(msg(7, "new"), now()),
            vec![Wire::AckStored { seq: 7 }]
        );
        assert_eq!(
            j.resume_frame(),
            Wire::Resume {
                recv_highest: 7,
                send_base: 0,
                read_upto: 6
            }
        );
    }

    fn outgoing(data: &[u8]) -> crate::file::Outgoing {
        crate::file::Outgoing {
            name: "f.bin".into(),
            data: data.to_vec(),
            sha256: crate::file::sha256_hex(data),
        }
    }
    fn with_inbox(role: Role) -> (tempfile::TempDir, Session) {
        let t = tempfile::tempdir().unwrap();
        let mut s = connected(role);
        s.set_inbox_dir(t.path().join("room").join("inbox"));
        (t, s)
    }

    #[test]
    fn file_chunks_roundtrip_dedupe_and_redeliver_once() {
        let data: Vec<u8> = (0..CHUNK_BYTES * 2 + 5).map(|i| (i % 256) as u8).collect();
        let mut h = connected(Role::Host);
        let (id, wires) = h.send_file(outgoing(&data)).unwrap();
        assert_eq!((id.as_str(), wires.len()), ("h3", 3));
        let (_t, mut j) = with_inbox(Role::Joiner);
        for w in &wires {
            j.on_wire(w.clone(), now());
        }
        // a resend after reconnect: every chunk is a duplicate, nothing new is stored
        for w in &wires {
            assert!(j.on_wire(w.clone(), now()).is_empty());
        }
        let b = j.take_batch().unwrap();
        assert_eq!(b.items.len(), 1);
        match &b.items[0] {
            Item::File {
                id,
                size,
                sha256,
                path,
                problem,
                ..
            } => {
                assert!(problem.is_none(), "{problem:?}");
                assert_eq!((id.as_str(), *size), ("h3", data.len() as u64));
                assert_eq!(sha256, &crate::file::sha256_hex(&data));
                assert_eq!(std::fs::read(path.as_ref().unwrap()).unwrap(), data);
                assert!(problem.is_none());
            }
            other => panic!("{other:?}"),
        }
        // the read ack covers the whole file (last chunk's seq)
        assert_eq!(
            j.ack_batch(b.id).unwrap().read,
            Some(Wire::AckRead { seq: 3 })
        );
    }

    #[test]
    fn resume_resends_only_the_chunks_the_peer_lacks() {
        let mut h = connected(Role::Host);
        let data = vec![1u8; CHUNK_BYTES * 3];
        h.send_file(outgoing(&data)).unwrap();
        let out = h.on_peer_resume(1, 0, 0);
        let seqs: Vec<u64> = out
            .iter()
            .map(|w| match w {
                Wire::File { seq, .. } => *seq,
                _ => panic!(),
            })
            .collect();
        assert_eq!(seqs, vec![2, 3]);
    }

    #[test]
    fn tampered_file_is_reported_not_saved() {
        let mut h = connected(Role::Host);
        let mut f = outgoing(b"hello");
        f.sha256 = crate::file::sha256_hex(b"something else");
        let (_, wires) = h.send_file(f).unwrap();
        let (t, mut j) = with_inbox(Role::Joiner);
        for w in wires {
            j.on_wire(w, now());
        }
        match &j.take_batch().unwrap().items[0] {
            Item::File { path, problem, .. } => {
                assert!(path.is_none());
                assert!(problem.as_ref().unwrap().contains("sha256 mismatch"));
            }
            other => panic!("{other:?}"),
        }
        assert_eq!(
            std::fs::read_dir(t.path().join("room/inbox"))
                .unwrap()
                .count(),
            0
        );
    }

    #[test]
    fn file_outbox_is_capped_separately_from_text() {
        let mut h = connected(Role::Host);
        let big = vec![0u8; 8 << 20];
        h.send_file(outgoing(&big)).unwrap();
        h.send_file(outgoing(&big)).unwrap();
        let e = h.send_file(outgoing(b"x")).unwrap_err();
        assert!(e.to_string().contains("outbox full"));
        h.send("text is unaffected", None).unwrap();
        h.on_wire(Wire::AckRead { seq: 1000 }, now());
        h.send_file(outgoing(b"x")).unwrap();
    }

    #[test]
    fn file_after_end_exits_3() {
        let mut h = connected(Role::Host);
        h.on_wire(Wire::End, now());
        assert_eq!(
            exit_of(&h.send_file(outgoing(b"x")).unwrap_err()),
            Exit::Ended
        );
    }

    #[test]
    fn presence_events() {
        let mut h = Session::new(Role::Host, PeerState::NotJoined, now());
        h.on_connected(Path::Relay, now());
        h.on_disconnected(now());
        assert_eq!(h.peer, PeerState::Disconnected);
        h.on_connected(Path::Direct, now()); // auto reconnect: no event
        h.on_wire(Wire::Leave, now());
        h.on_connected(Path::Direct, now()); // rejoin after leave
        h.on_wire(Wire::End, now());
        let kinds: Vec<EventKind> = h
            .take_batch()
            .unwrap()
            .items
            .iter()
            .filter_map(|i| match i {
                Item::Event { kind, .. } => Some(*kind),
                _ => None,
            })
            .collect();
        assert_eq!(
            kinds,
            vec![
                EventKind::PeerJoined,
                EventKind::PeerLeft,
                EventKind::PeerRejoined,
                EventKind::RoomEnded
            ]
        );
        assert!(h.last_link_drop.is_some());
        assert_eq!(h.peer, PeerState::Ended);
    }

    #[test]
    fn joiner_gets_no_peer_joined_event() {
        // preflight D22
        let mut j = Session::new(Role::Joiner, PeerState::NotJoined, now());
        j.on_connected(Path::Direct, now());
        assert!(j.take_batch().is_none());
    }

    #[test]
    fn waiting_signal_updates_peer_last_waited() {
        let mut h = connected(Role::Host);
        assert!(h.peer_last_waited.is_none());
        h.on_wire(Wire::Waiting, now());
        assert_eq!(h.peer_last_waited, Some(now()));
    }

    #[test]
    fn leave_report_splits_delivered_unread_from_undelivered() {
        let mut h = connected(Role::Host);
        for b in ["1", "2", "3"] {
            h.send(b, None).unwrap();
        }
        h.on_wire(Wire::AckStored { seq: 2 }, now());
        assert_eq!(h.leave_report(), (2, 1));
        assert_eq!(h.last_sent_seq(), 3);
        assert_eq!(h.peer_stored_upto, 2);
    }

    #[test]
    fn invite_expired_event() {
        let mut h = Session::new(Role::Host, PeerState::NotJoined, now());
        h.on_invite_expired(now());
        assert_eq!(h.peer, PeerState::Ended);
        let b = h.take_batch().unwrap();
        assert!(matches!(
            b.items[0],
            Item::Event {
                kind: EventKind::InviteExpired,
                ..
            }
        ));
        // preflight D14: acking an events-only terminal batch reports terminal, no AckRead
        assert_eq!(
            h.ack_batch(b.id),
            Some(Acked {
                read: None,
                terminal: true
            })
        );
    }

    #[test]
    fn non_terminal_events_only_batch_is_not_terminal() {
        let mut h = Session::new(Role::Host, PeerState::NotJoined, now());
        h.on_connected(Path::Direct, now());
        let b = h.take_batch().unwrap();
        assert_eq!(
            h.ack_batch(b.id),
            Some(Acked {
                read: None,
                terminal: false
            })
        );
    }
}
