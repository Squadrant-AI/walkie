use crate::exit::{fail, Exit};
use crate::model::{Item, PeerState, Role};
use anyhow::Result;
use serde::{Deserialize, Serialize};
use std::time::SystemTime;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::unix::{OwnedReadHalf, OwnedWriteHalf};
use tokio::net::UnixStream;

#[derive(Serialize, Deserialize, Debug, Clone)]
#[serde(tag = "op", rename_all = "snake_case")]
pub enum Req {
    Info,
    Send {
        body: String,
        reply_to: Option<String>,
    },
    /// `path` is absolute (the CLI resolves it); the daemon reads and checks the file.
    SendFile {
        path: String,
    },
    Wait {
        timeout_ms: u64,
    },
    AckBatch {
        batch_id: u64,
    },
    Status,
    Leave,
    End,
}

#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct StatusInfo {
    pub room: String,
    pub role: Role,
    pub peer: PeerState,
    pub peer_since: SystemTime,
    pub peer_last_waited: Option<SystemTime>,
    pub path: Option<String>,
    pub last_link_drop: Option<SystemTime>,
    pub invite_expires_at: Option<u64>,
    pub unread: usize,
    pub outbound: usize,
}

#[derive(Serialize, Deserialize, Debug, Clone)]
#[serde(tag = "r", rename_all = "snake_case")]
pub enum Resp {
    Info {
        room: String,
        invite: Option<String>,
        peer: PeerState,
        rejected: Option<String>,
        unreachable: bool,
        env_error: Option<String>,
    },
    Sent {
        id: String,
    },
    SentFile {
        id: String,
        name: String,
        size: u64,
        sha256: String,
    },
    Batch {
        batch_id: u64,
        items: Vec<Item>,
    },
    Timeout,
    Preempted,
    Status(StatusInfo),
    LeaveReport {
        delivered_unread: usize,
        undelivered: usize,
    },
    Ok,
    Err {
        exit: u8,
        message: String,
    },
}

pub fn check_peer_uid(s: &UnixStream) -> Result<()> {
    let cred = s.peer_cred()?;
    if cred.uid() != crate::paths::uid() {
        return Err(fail(Exit::Error, "IPC caller has a different uid; refused"));
    }
    Ok(())
}

pub struct Client {
    r: BufReader<OwnedReadHalf>,
    w: OwnedWriteHalf,
}

impl Client {
    pub fn from_stream(s: UnixStream) -> Client {
        let (r, w) = s.into_split();
        Client {
            r: BufReader::new(r),
            w,
        }
    }

    pub async fn connect(room: &str) -> Result<Client> {
        let p = crate::paths::socket_path(room)?;
        match UnixStream::connect(&p).await {
            Ok(s) => Ok(Client::from_stream(s)),
            Err(_) if crate::ticket::is_ended(room) => {
                Err(fail(Exit::Ended, format!("room {room} has ended")))
            }
            Err(_) => Err(fail(
                Exit::NoRoom,
                format!("room {room} is not active (try `walkie rooms` / `walkie rejoin`)"),
            )),
        }
    }

    async fn write_line<T: Serialize>(&mut self, v: &T) -> Result<()> {
        let mut b = serde_json::to_vec(v)?;
        b.push(b'\n');
        self.w.write_all(&b).await?;
        self.w.flush().await?;
        Ok(())
    }

    async fn read_line<T: for<'de> Deserialize<'de>>(&mut self) -> Result<T> {
        let mut line = String::new();
        if self.r.read_line(&mut line).await? == 0 {
            anyhow::bail!("daemon closed the connection");
        }
        Ok(serde_json::from_str(&line)?)
    }

    pub async fn send(&mut self, req: &Req) -> Result<()> {
        self.write_line(req).await
    }
    pub async fn recv(&mut self) -> Result<Resp> {
        self.read_line().await
    }
    pub async fn call(&mut self, req: &Req) -> Result<Resp> {
        self.send(req).await?;
        self.recv().await
    }

    // server-side helpers (daemon uses the same framing)
    pub async fn recv_req(&mut self) -> Result<Req> {
        self.read_line().await
    }
    pub async fn send_resp(&mut self, resp: &Resp) -> Result<()> {
        self.write_line(resp).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn req_resp_roundtrip_over_unix_socket() {
        let t = tempfile::tempdir().unwrap();
        let sock = t.path().join("s.sock");
        let listener = tokio::net::UnixListener::bind(&sock).unwrap();
        let server = tokio::spawn(async move {
            let (s, _) = listener.accept().await.unwrap();
            check_peer_uid(&s).unwrap();
            let mut c = Client::from_stream(s);
            let req = c.recv_req().await.unwrap();
            assert!(matches!(req, Req::Send { .. }));
            c.send_resp(&Resp::Sent { id: "h1".into() }).await.unwrap();
        });
        let s = tokio::net::UnixStream::connect(&sock).await.unwrap();
        let mut c = Client::from_stream(s);
        let r = c
            .call(&Req::Send {
                body: "hi".into(),
                reply_to: None,
            })
            .await
            .unwrap();
        assert!(matches!(r, Resp::Sent { id } if id == "h1"));
        server.await.unwrap();
    }

    #[test]
    fn wire_names_are_stable() {
        let j = serde_json::to_string(&Req::AckBatch { batch_id: 3 }).unwrap();
        assert_eq!(j, r#"{"op":"ack_batch","batch_id":3}"#);
        let j = serde_json::to_string(&Resp::Err {
            exit: 3,
            message: "m".into(),
        })
        .unwrap();
        assert_eq!(j, r#"{"r":"err","exit":3,"message":"m"}"#);
    }
}
