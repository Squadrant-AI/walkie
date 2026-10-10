use crate::exit::{fail, Exit};
use crate::model::Path;
use crate::proto::{read_frame, write_frame, Wire};
use anyhow::{Context, Result};
use iroh::address_lookup::AddrFilter;
use iroh::endpoint::presets;
use iroh::endpoint::{Connection, RecvStream, SendStream};
use iroh::{Endpoint, EndpointAddr, EndpointId, SecretKey};
use iroh_tickets::endpoint::EndpointTicket;
use std::str::FromStr;
use std::time::Duration;

pub const ALPN: &[u8] = b"walkie/1";

fn test_local() -> bool {
    std::env::var("WALKIE_TEST_LOCAL").is_ok()
}

pub fn new_secret() -> String {
    // Verified (docs/spikes/iroh-api.md): `generate()` takes no RNG; `SecretKey` has no `Display`,
    // so hex-encode the 32 bytes by hand. `SecretKey::from_str` parses this 64-char hex.
    SecretKey::generate()
        .to_bytes()
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

pub async fn bind(secret: &str, relay_only: bool) -> Result<Endpoint> {
    let sk = SecretKey::from_str(secret).context("bad secret key in ticket")?;
    let builder = if test_local() {
        // `presets::Empty` cannot bind (no rustls crypto provider). `Minimal` sets only the provider:
        // no relay, no address lookup. `bind_addr` is fallible, and the default 0.0.0.0/[::] IP
        // transports are removed first so only 127.0.0.1 is bound.
        Endpoint::builder(presets::Minimal)
            .clear_ip_transports()
            .bind_addr("127.0.0.1:0")?
    } else {
        Endpoint::builder(presets::N0)
    };
    let mut builder = builder.secret_key(sk).alpns(vec![ALPN.to_vec()]);
    if relay_only {
        // No IP socket is bound, so `ep.addr()` and the ticket carry only the relay URL; the filter
        // is belt-and-braces for the published record.
        builder = builder
            .clear_ip_transports()
            .addr_filter(AddrFilter::relay_only());
    }
    builder.bind().await.map_err(|e| {
        fail(
            Exit::Environment,
            format!("cannot bind network endpoint: {e}"),
        )
    })
}

pub async fn wait_online(ep: &Endpoint, timeout: Duration) -> Result<()> {
    if test_local() {
        return Ok(());
    }
    tokio::time::timeout(timeout, ep.online())
        .await
        .map_err(|_| {
            fail(
                Exit::Environment,
                "no relay reachable (no network, or a sandbox blocks it)",
            )
        })?;
    Ok(())
}

pub fn my_addr(ep: &Endpoint) -> String {
    EndpointTicket::new(ep.addr()).to_string()
}

pub struct PeerAddr {
    pub id: Option<String>,
    pub ticket: Option<String>,
}

pub async fn dial(ep: &Endpoint, peer: &PeerAddr) -> Result<Connection> {
    let addr: EndpointAddr = if let Some(t) = &peer.ticket {
        EndpointTicket::from_str(t)
            .context("bad peer ticket")?
            .endpoint_addr()
            .clone()
    } else {
        let id = EndpointId::from_str(peer.id.as_deref().context("no peer id")?)?;
        EndpointAddr::from(id)
    };
    Ok(ep.connect(addr, ALPN).await?)
}

pub async fn accept(ep: &Endpoint) -> Result<Connection> {
    let incoming = ep.accept().await.context("endpoint closed")?;
    Ok(incoming.await?)
}

#[derive(Debug)]
pub struct Link {
    pub conn: Connection,
    pub send: SendStream,
    pub recv: RecvStream,
    pub remote_id: String,
    pub dialed_by: String,
}

pub async fn client_handshake(
    conn: &Connection,
    token: Option<String>,
    my_addr: String,
) -> Result<(Link, String)> {
    let (mut send, mut recv) = conn.open_bi().await?;
    write_frame(
        &mut send,
        &Wire::Hello {
            token,
            addr: my_addr.clone(),
        },
    )
    .await?;
    match read_frame(&mut recv).await? {
        Wire::Welcome { addr } => {
            let remote_id = conn.remote_id().to_string(); // infallible on `Connection`
                                                          // `Connection` has no `local_id()`; our id is the one inside our own ticket.
            let me = EndpointTicket::from_str(&my_addr)?
                .endpoint_addr()
                .id
                .to_string();
            Ok((
                Link {
                    conn: conn.clone(),
                    send,
                    recv,
                    remote_id,
                    dialed_by: me,
                },
                addr,
            ))
        }
        Wire::Reject { reason } => Err(fail(Exit::BadInvite, format!("join refused: {reason}"))),
        other => Err(fail(
            Exit::Error,
            format!("unexpected handshake frame: {other:?}"),
        )),
    }
}

pub async fn server_handshake(
    conn: &Connection,
    auth: &mut (dyn FnMut(&str, Option<&str>) -> bool + Send),
    my_addr: String,
) -> Result<(Link, String)> {
    let (mut send, mut recv) = conn.accept_bi().await?;
    let remote_id = conn.remote_id().to_string();
    match read_frame(&mut recv).await? {
        Wire::Hello { token, addr } => {
            if !auth(&remote_id, token.as_deref()) {
                let _ = write_frame(
                    &mut send,
                    &Wire::Reject {
                        reason: "invite invalid, expired or already used".into(),
                    },
                )
                .await;
                tokio::time::sleep(Duration::from_millis(200)).await;
                conn.close(1u32.into(), b"rejected");
                return Err(fail(Exit::BadInvite, "rejected a join attempt"));
            }
            write_frame(&mut send, &Wire::Welcome { addr: my_addr }).await?;
            Ok((
                Link {
                    conn: conn.clone(),
                    send,
                    recv,
                    dialed_by: remote_id.clone(),
                    remote_id,
                },
                addr,
            ))
        }
        other => Err(fail(
            Exit::Error,
            format!("unexpected handshake frame: {other:?}"),
        )),
    }
}

pub fn path_of(conn: &Connection) -> Path {
    // Verified: `paths()` is a snapshot; the selected path says whether traffic goes via the relay.
    let relay = conn
        .paths()
        .iter()
        .find(|p| p.is_selected())
        .map(|p| p.is_relay());
    if relay == Some(false) {
        Path::Direct
    } else {
        Path::Relay
    }
}

/// Deterministic tie-break for two live connections: keep the one dialed by the lower id.
pub fn keep(a_dialer: &str, b_dialer: &str) -> bool {
    a_dialer < b_dialer
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::proto::{read_frame, write_frame, Wire};

    #[test]
    fn tie_break_is_symmetric() {
        assert!(keep("aaa", "bbb"));
        assert!(!keep("bbb", "aaa"));
    }

    #[tokio::test]
    async fn loopback_handshake_and_frames() {
        std::env::set_var("WALKIE_TEST_LOCAL", "1");
        let a = bind(&new_secret(), false).await.unwrap();
        let b = bind(&new_secret(), false).await.unwrap();
        let a_addr = my_addr(&a);
        let token = "abc";
        let server = tokio::spawn(async move {
            let conn = accept(&a).await.unwrap();
            let mut auth = |_id: &str, t: Option<&str>| t == Some(token);
            let (mut link, peer_addr) = server_handshake(&conn, &mut auth, my_addr(&a))
                .await
                .unwrap();
            assert!(peer_addr.starts_with("endpoint"));
            let w = read_frame(&mut link.recv).await.unwrap();
            assert_eq!(w, Wire::Waiting);
        });
        let conn = dial(
            &b,
            &PeerAddr {
                id: None,
                ticket: Some(a_addr),
            },
        )
        .await
        .unwrap();
        let (mut link, _) = client_handshake(&conn, Some(token.into()), my_addr(&b))
            .await
            .unwrap();
        write_frame(&mut link.send, &Wire::Waiting).await.unwrap();
        server.await.unwrap();
    }

    #[tokio::test]
    async fn wrong_token_is_rejected_with_bad_invite() {
        std::env::set_var("WALKIE_TEST_LOCAL", "1");
        let a = bind(&new_secret(), false).await.unwrap();
        let b = bind(&new_secret(), false).await.unwrap();
        let a_addr = my_addr(&a);
        tokio::spawn(async move {
            let conn = accept(&a).await.unwrap();
            let mut auth = |_: &str, _: Option<&str>| false;
            let _ = server_handshake(&conn, &mut auth, my_addr(&a)).await;
        });
        let conn = dial(
            &b,
            &PeerAddr {
                id: None,
                ticket: Some(a_addr),
            },
        )
        .await
        .unwrap();
        let e = client_handshake(&conn, Some("nope".into()), my_addr(&b))
            .await
            .unwrap_err();
        assert_eq!(crate::exit::exit_of(&e), crate::exit::Exit::BadInvite);
    }
}
