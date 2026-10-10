use anyhow::{bail, Result};
use serde::{Deserialize, Serialize};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};

/// A 16 KiB body of control characters JSON-escapes to 6 bytes each (`\u0001`),
/// so the frame cap must stay well above 6 * BODY_CAP (preflight D4).
pub const MAX_FRAME: usize = 256 * 1024;

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
#[serde(tag = "t", rename_all = "snake_case")]
pub enum Wire {
    Hello {
        token: Option<String>,
        addr: String,
    },
    Welcome {
        addr: String,
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
            },
            Wire::Hello {
                token: None,
                addr: "endpointx".into(),
            },
            Wire::Welcome {
                addr: "endpointy".into(),
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
