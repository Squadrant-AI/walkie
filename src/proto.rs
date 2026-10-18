use anyhow::{bail, Result};
use serde::{Deserialize, Serialize};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};

/// A 16 KiB body of control characters JSON-escapes to 6 bytes each (`\u0001`),
/// so the frame cap must stay well above 6 * BODY_CAP (preflight D4).
pub const MAX_FRAME: usize = 256 * 1024;

/// Capability advertised in `Hello` / `Welcome` by peers that understand `Wire::File`.
pub const CAP_FILE: &str = "file/1";

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
#[serde(tag = "t", rename_all = "snake_case")]
pub enum Wire {
    /// `caps`: optional features this side supports (v0.1.2 peers send none and ignore ours).
    Hello {
        token: Option<String>,
        addr: String,
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        caps: Vec<String>,
    },
    Welcome {
        addr: String,
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        caps: Vec<String>,
    },
    Reject {
        reason: String,
    },
    /// `recv_highest`: highest peer seq I hold. `send_base`: highest of MY seqs I know you have
    /// read. `read_upto`: highest peer seq I have read (lets the sender prune a lost AckRead,
    /// spec §6.2, preflight D3).
    Resume {
        recv_highest: u64,
        send_base: u64,
        read_upto: u64,
    },
    Msg {
        seq: u64,
        reply_to: Option<String>,
        body: String,
    },
    /// One chunk of a file transfer; shares the `seq` space, dedupe and acks with `Msg`.
    /// Every chunk repeats the header so each frame is self-describing; `data` is base64.
    File {
        seq: u64,
        xfer: u64,
        idx: u32,
        last: bool,
        name: String,
        size: u64,
        sha256: String,
        data: String,
    },
    AckStored {
        seq: u64,
    },
    AckRead {
        seq: u64,
    },
    Waiting,
    Leave,
    End,
}

pub async fn write_frame<W: AsyncWrite + Unpin>(w: &mut W, f: &Wire) -> Result<()> {
    let b = serde_json::to_vec(f)?;
    if b.len() > MAX_FRAME {
        bail!("frame too large: {} bytes", b.len());
    }
    w.write_u32(b.len() as u32).await?;
    w.write_all(&b).await?;
    w.flush().await?;
    Ok(())
}

pub async fn read_frame<R: AsyncRead + Unpin>(r: &mut R) -> Result<Wire> {
    let n = r.read_u32().await? as usize;
    if n > MAX_FRAME {
        bail!("peer frame too large: {n} bytes");
    }
    let mut b = vec![0u8; n];
    r.read_exact(&mut b).await?;
    Ok(serde_json::from_slice(&b)?)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn roundtrip_every_variant() {
        let frames = vec![
            Wire::Hello {
                token: Some("abc".into()),
                addr: "endpointx".into(),
                caps: vec![CAP_FILE.into()],
            },
            Wire::Hello {
                token: None,
                addr: "endpointx".into(),
                caps: vec![],
            },
            Wire::Welcome {
                addr: "endpointy".into(),
                caps: vec![CAP_FILE.into()],
            },
            Wire::Reject {
                reason: "bad token".into(),
            },
            Wire::Resume {
                recv_highest: 3,
                send_base: 2,
                read_upto: 1,
            },
            Wire::Msg {
                seq: 4,
                reply_to: Some("h3".into()),
                body: "hi\n\"q\"".into(),
            },
            Wire::File {
                seq: 5,
                xfer: 2,
                idx: 0,
                last: true,
                name: "a.txt".into(),
                size: 3,
                sha256: "00".repeat(32),
                data: "YWJj".into(),
            },
            Wire::AckStored { seq: 4 },
            Wire::AckRead { seq: 4 },
            Wire::Waiting,
            Wire::Leave,
            Wire::End,
        ];
        let (mut a, mut b) = tokio::io::duplex(1 << 20);
        for f in &frames {
            write_frame(&mut a, f).await.unwrap();
        }
        for f in &frames {
            assert_eq!(&read_frame(&mut b).await.unwrap(), f);
        }
    }

    #[test]
    fn v012_handshake_frames_still_parse_and_ours_parse_there() {
        // a v0.1.2 peer sends no `caps`; its parser ignores ours (no deny_unknown_fields)
        let old: Wire = serde_json::from_str(r#"{"t":"hello","token":null,"addr":"a"}"#).unwrap();
        assert_eq!(
            old,
            Wire::Hello {
                token: None,
                addr: "a".into(),
                caps: vec![]
            }
        );
        let old: Wire = serde_json::from_str(r#"{"t":"welcome","addr":"a"}"#).unwrap();
        assert_eq!(
            old,
            Wire::Welcome {
                addr: "a".into(),
                caps: vec![]
            }
        );
        let ours = serde_json::to_string(&Wire::Welcome {
            addr: "a".into(),
            caps: vec![],
        })
        .unwrap();
        assert_eq!(ours, r#"{"t":"welcome","addr":"a"}"#);
    }

    #[tokio::test]
    async fn max_file_chunk_fits_in_a_frame() {
        let w = Wire::File {
            seq: u64::MAX,
            xfer: u64::MAX,
            idx: u32::MAX,
            last: false,
            name: "n".repeat(200),
            size: u64::MAX,
            sha256: "f".repeat(64),
            data: data_encoding::BASE64.encode(&vec![0xffu8; crate::file::CHUNK_BYTES]),
        };
        assert!(serde_json::to_vec(&w).unwrap().len() < MAX_FRAME / 3);
        let (mut a, mut b) = tokio::io::duplex(1 << 20);
        write_frame(&mut a, &w).await.unwrap();
        assert_eq!(read_frame(&mut b).await.unwrap(), w);
    }

    #[tokio::test]
    async fn rejects_oversized_frames_both_ways() {
        let (mut a, _b) = tokio::io::duplex(1 << 20);
        let huge = Wire::Msg {
            seq: 1,
            reply_to: None,
            body: "x".repeat(MAX_FRAME + 1),
        };
        assert!(write_frame(&mut a, &huge).await.is_err());

        let (mut c, mut d) = tokio::io::duplex(64);
        use tokio::io::AsyncWriteExt;
        c.write_u32((MAX_FRAME + 1) as u32).await.unwrap();
        assert!(read_frame(&mut d).await.is_err());
    }

    #[tokio::test]
    async fn max_size_control_char_body_fits_in_a_frame() {
        // preflight D4: 16 KiB of control chars escapes to ~98 KiB of JSON.
        let body = "\u{1}".repeat(crate::frame::BODY_CAP);
        let w = Wire::Msg {
            seq: 1,
            reply_to: Some("h1".into()),
            body,
        };
        let encoded = serde_json::to_vec(&w).unwrap().len();
        assert!(encoded > 64 * 1024, "test must exceed the old 64 KiB cap");
        let (mut a, mut b) = tokio::io::duplex(1 << 20);
        write_frame(&mut a, &w).await.unwrap();
        assert_eq!(read_frame(&mut b).await.unwrap(), w);
    }
}
