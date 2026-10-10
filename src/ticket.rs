use crate::exit::{fail, Exit};
use crate::model::Role;
use crate::paths::{rooms_dir, ticket_path, tombstone_path};
use anyhow::Result;
use serde::{Deserialize, Serialize};
use std::io::Write;
use std::os::unix::fs::OpenOptionsExt;
use std::time::{SystemTime, UNIX_EPOCH};

#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct Ticket {
    pub room: String,
    pub role: Role,
    pub secret_key: String,
    pub relay_only: bool,
    pub peer_id: Option<String>,
    pub peer_addr: Option<String>,
    pub token: Option<String>,
    pub invite_expires_at: Option<u64>,
    pub idle_ttl_secs: u64,
    pub expires_at: u64,
}

pub fn now_unix() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

impl Ticket {
    pub fn save(&self) -> Result<()> {
        let p = ticket_path(&self.room)?;
        let tmp = p.with_extension("ticket.tmp");
        let mut f = std::fs::OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .mode(0o600)
            .open(&tmp)?;
        f.write_all(&serde_json::to_vec_pretty(self)?)?;
        f.sync_all()?;
        std::fs::rename(&tmp, &p)?;
        Ok(())
    }

    pub fn load(room: &str) -> Result<Ticket> {
        let p = ticket_path(room)?;
        let b = std::fs::read(&p)
            .map_err(|_| fail(Exit::NoRoom, format!("no ticket for room {room}")))?;
        Ok(serde_json::from_slice(&b)?)
    }

    pub fn delete(room: &str) {
        if let Ok(p) = ticket_path(room) {
            let _ = std::fs::remove_file(p);
        }
    }

    pub fn is_expired(&self, now: u64) -> bool {
        now >= self.expires_at
    }
    pub fn touch(&mut self, now: u64) {
        self.expires_at = now + self.idle_ttl_secs;
    }
}

pub fn list_live(now: u64) -> Vec<Ticket> {
    let Ok(dir) = rooms_dir() else { return vec![] };
    let mut out = vec![];
    for e in std::fs::read_dir(dir).into_iter().flatten().flatten() {
        let name = e.file_name().to_string_lossy().to_string();
        let Some(room) = name.strip_suffix(".ticket") else {
            continue;
        };
        match Ticket::load(room) {
            Ok(t) if !t.is_expired(now) => out.push(t),
            _ => Ticket::delete(room),
        }
    }
    out.sort_by(|a, b| a.room.cmp(&b.room));
    out
}

pub fn mark_ended(room: &str) {
    if let Ok(p) = tombstone_path(room) {
        let _ = std::fs::write(p, now_unix().to_string());
    }
}

pub fn is_ended(room: &str) -> bool {
    let Ok(p) = tombstone_path(room) else {
        return false;
    };
    std::fs::read_to_string(p)
        .ok()
        .and_then(|s| s.trim().parse::<u64>().ok())
        .map(|t| now_unix().saturating_sub(t) < 86_400)
        .unwrap_or(false)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::Role;
    use crate::paths::test_support::IsolatedEnv;
    use std::os::unix::fs::PermissionsExt;

    fn sample(room: &str, expires_at: u64) -> Ticket {
        Ticket {
            room: room.into(),
            role: Role::Host,
            secret_key: "sk".into(),
            relay_only: false,
            peer_id: None,
            peer_addr: None,
            token: Some("tok".into()),
            invite_expires_at: Some(10),
            idle_ttl_secs: 86_400,
            expires_at,
        }
    }

    #[test]
    fn save_load_roundtrip_with_0600() {
        let env = IsolatedEnv::new(); // preflight D2: serialized, never the real ~/.walkie
        sample("aaaaaa", 999).save().unwrap();
        let p = crate::paths::ticket_path("aaaaaa").unwrap();
        assert!(p.starts_with(env.dir.path()));
        assert_eq!(
            std::fs::metadata(&p).unwrap().permissions().mode() & 0o777,
            0o600
        );
        assert_eq!(Ticket::load("aaaaaa").unwrap().expires_at, 999);
    }

    #[test]
    fn missing_ticket_is_no_room() {
        let _env = IsolatedEnv::new();
        let e = Ticket::load("zzzzzz").unwrap_err();
        assert_eq!(crate::exit::exit_of(&e), crate::exit::Exit::NoRoom);
    }

    #[test]
    fn list_live_prunes_expired_and_touch_extends() {
        let _env = IsolatedEnv::new();
        sample("live01", 2_000).save().unwrap();
        sample("dead01", 500).save().unwrap();
        let live: Vec<_> = list_live(1_000).into_iter().map(|t| t.room).collect();
        assert_eq!(live, vec!["live01".to_string()]);
        assert!(Ticket::load("dead01").is_err());
        let mut t = Ticket::load("live01").unwrap();
        t.touch(1_500);
        assert_eq!(t.expires_at, 1_500 + 86_400);
    }

    #[test]
    fn tombstones() {
        let _env = IsolatedEnv::new();
        assert!(!is_ended("bbbbbb"));
        mark_ended("bbbbbb");
        assert!(is_ended("bbbbbb"));
    }
}
