use std::fmt;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum Exit {
    Ok = 0,
    Error = 1,
    Timeout = 2,
    Ended = 3,
    NoRoom = 4,
    PeerLeft = 5,
    Unreachable = 6,
    BadInvite = 7,
    Environment = 8,
}

impl Exit {
    pub fn code(self) -> u8 {
        self as u8
    }
    pub fn from_code(c: u8) -> Exit {
        match c {
            0 => Exit::Ok,
            2 => Exit::Timeout,
            3 => Exit::Ended,
            4 => Exit::NoRoom,
            5 => Exit::PeerLeft,
            6 => Exit::Unreachable,
            7 => Exit::BadInvite,
            8 => Exit::Environment,
            _ => Exit::Error,
        }
    }
}

#[derive(Debug)]
pub struct WalkieError {
    pub exit: Exit,
    pub msg: String,
}

impl fmt::Display for WalkieError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.msg)
    }
}
impl std::error::Error for WalkieError {}

pub fn fail(exit: Exit, msg: impl Into<String>) -> anyhow::Error {
    WalkieError {
        exit,
        msg: msg.into(),
    }
    .into()
}

pub fn exit_of(err: &anyhow::Error) -> Exit {
    err.downcast_ref::<WalkieError>()
        .map(|e| e.exit)
        .unwrap_or(Exit::Error)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn codes_are_stable() {
        assert_eq!(Exit::Ok.code(), 0);
        assert_eq!(Exit::Timeout.code(), 2);
        assert_eq!(Exit::Ended.code(), 3);
        assert_eq!(Exit::PeerLeft.code(), 5);
        assert_eq!(Exit::Environment.code(), 8);
    }
    #[test]
    fn exit_of_unwraps_walkie_error_and_defaults_to_1() {
        assert_eq!(exit_of(&fail(Exit::BadInvite, "x")), Exit::BadInvite);
        assert_eq!(exit_of(&anyhow::anyhow!("other")), Exit::Error);
    }
}
