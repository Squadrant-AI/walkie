use crate::exit::{fail, Exit};
use data_encoding::BASE32_NOPAD;
use sha2::{Digest, Sha256};
use subtle::ConstantTimeEq;

pub const PREFIX: &str = "walkie1:";
pub const PREFIX_RELAY_ONLY: &str = "walkie1r:";

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Invite {
    pub ticket: String,
    pub token: [u8; 16],
    pub relay_only: bool,
}

pub fn new_token() -> [u8; 16] {
    rand::random()
}

pub fn token_b32(t: &[u8; 16]) -> String {
    BASE32_NOPAD.encode(t).to_lowercase()
}

pub fn token_from_b32(s: &str) -> Option<[u8; 16]> {
    let bytes = BASE32_NOPAD.decode(s.to_uppercase().as_bytes()).ok()?;
    bytes.try_into().ok()
}

pub fn room_id(token: &[u8; 16]) -> String {
    let h = Sha256::digest(token);
    BASE32_NOPAD.encode(&h)[..6].to_lowercase()
}

pub fn tokens_equal(a: &[u8; 16], b: &[u8; 16]) -> bool {
    a.ct_eq(b).into()
}

impl Invite {
    pub fn encode(&self) -> String {
        let p = if self.relay_only {
            PREFIX_RELAY_ONLY
        } else {
            PREFIX
        };
        format!("{p}{}.{}", self.ticket, token_b32(&self.token))
    }

    pub fn decode(s: &str) -> anyhow::Result<Invite> {
        let bad = || {
            fail(
                Exit::BadInvite,
                "invalid invite (expected walkie1:<ticket>.<token>)",
            )
        };
        let s = s.trim();
        let (relay_only, rest) = if let Some(r) = s.strip_prefix(PREFIX_RELAY_ONLY) {
            (true, r)
        } else if let Some(r) = s.strip_prefix(PREFIX) {
            (false, r)
        } else {
            return Err(bad());
        };
        let (ticket, tok) = rest.rsplit_once('.').ok_or_else(bad)?;
        if ticket.is_empty() {
            return Err(bad());
        }
        let token = token_from_b32(tok).ok_or_else(bad)?;
        Ok(Invite {
            ticket: ticket.to_string(),
            token,
            relay_only,
        })
    }

    pub fn room_id(&self) -> String {
        room_id(&self.token)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::exit::{exit_of, Exit};

    fn sample(relay_only: bool) -> Invite {
        Invite {
            ticket: "endpointabc123xyz".into(),
            token: [7u8; 16],
            relay_only,
        }
    }

    #[test]
    fn roundtrip_normal_and_relay_only() {
        for ro in [false, true] {
            let s = sample(ro).encode();
            assert!(s.starts_with(if ro { "walkie1r:" } else { "walkie1:" }));
            assert_eq!(Invite::decode(&s).unwrap(), sample(ro));
        }
    }

    #[test]
    fn decode_tolerates_surrounding_whitespace() {
        let s = format!("  {}\n", sample(false).encode());
        assert_eq!(Invite::decode(&s).unwrap(), sample(false));
    }

    #[test]
    fn room_id_is_6_lowercase_and_deterministic() {
        let a = room_id(&[1u8; 16]);
        assert_eq!(a.len(), 6);
        assert_eq!(a, a.to_lowercase());
        assert_eq!(a, room_id(&[1u8; 16]));
        assert_ne!(a, room_id(&[2u8; 16]));
    }

    #[test]
    fn bad_invites_exit_7() {
        for s in [
            "",
            "walkie2:x.y",
            "walkie1:",
            "walkie1:ticketonly",
            "walkie1:t.notbase32!!",
            "walkie1:t.aaaa",
        ] {
            let e = Invite::decode(s).unwrap_err();
            assert_eq!(exit_of(&e), Exit::BadInvite, "{s}");
        }
    }

    #[test]
    fn tokens_are_random_and_compare_constant_time() {
        let a = new_token();
        let b = new_token();
        assert_ne!(a, b);
        assert!(tokens_equal(&a, &a));
        assert!(!tokens_equal(&a, &b));
        assert_eq!(token_from_b32(&token_b32(&a)), Some(a));
    }
}
