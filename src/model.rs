use serde::{Deserialize, Serialize};
use std::time::SystemTime;

#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum Role {
    Host,
    Joiner,
}

impl Role {
    pub fn prefix(self) -> char {
        match self {
            Role::Host => 'h',
            Role::Joiner => 'j',
        }
    }
    pub fn peer(self) -> Role {
        match self {
            Role::Host => Role::Joiner,
            Role::Joiner => Role::Host,
        }
    }
}

#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum Path {
    Direct,
    Relay,
}

#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq)]
#[serde(tag = "state", rename_all = "snake_case")]
pub enum PeerState {
    NotJoined,
    Connected { path: Path },
    Disconnected,
    Left,
    Ended,
}

impl PeerState {
    pub fn name(&self) -> &'static str {
        match self {
            PeerState::NotJoined => "not-joined",
            PeerState::Connected { .. } => "connected",
            PeerState::Disconnected => "disconnected",
            PeerState::Left => "left",
            PeerState::Ended => "ended",
        }
    }
}

#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq)]
#[serde(rename_all = "kebab-case")]
pub enum EventKind {
    PeerJoined,
    InviteExpired,
    PeerLeft,
    PeerRejoined,
    RoomEnded,
}

impl EventKind {
    pub fn as_str(self) -> &'static str {
        match self {
            EventKind::PeerJoined => "peer-joined",
            EventKind::InviteExpired => "invite-expired",
            EventKind::PeerLeft => "peer-left",
            EventKind::PeerRejoined => "peer-rejoined",
            EventKind::RoomEnded => "room-ended",
        }
    }
    pub fn note(self) -> &'static str {
        match self {
            EventKind::PeerLeft => " (may rejoin)",
            _ => "",
        }
    }
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Item {
    Message {
        id: String,
        reply_to: Option<String>,
        received: SystemTime,
        body: String,
    },
    Event {
        kind: EventKind,
        at: SystemTime,
    },
}
