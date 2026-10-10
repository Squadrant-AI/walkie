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

/// Our address as an `EndpointTicket` string. In relay-only mode every IP address is stripped,
/// so neither the invite nor the Hello/Welcome `addr` can leak one (spec §7, preflight D12).
pub fn my_addr(ep: &Endpoint, relay_only: bool) -> String {
    let mut addr = ep.addr();
    if relay_only {
        addr.addrs.retain(|a| a.is_relay());
    }
    EndpointTicket::new(addr).to_string()
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

    // The daemon accepts in its own task (with a handshake timeout); the tests accept inline.
    async fn accept(ep: &Endpoint) -> Result<Connection> {
        let incoming = ep.accept().await.context("endpoint closed")?;
        Ok(incoming.await?)
    }

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
        let a_addr = my_addr(&a, false);
        let token = "abc";
        let server = tokio::spawn(async move {
            let conn = accept(&a).await.unwrap();
            let mut auth = |_id: &str, t: Option<&str>| t == Some(token);
            let (mut link, peer_addr) = server_handshake(&conn, &mut auth, my_addr(&a, false))
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
        let (mut link, _) = client_handshake(&conn, Some(token.into()), my_addr(&b, false))
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
        let a_addr = my_addr(&a, false);
        tokio::spawn(async move {
            let conn = accept(&a).await.unwrap();
            let mut auth = |_: &str, _: Option<&str>| false;
            let _ = server_handshake(&conn, &mut auth, my_addr(&a, false)).await;
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
        let e = client_handshake(&conn, Some("nope".into()), my_addr(&b, false))
            .await
            .unwrap_err();
        assert_eq!(crate::exit::exit_of(&e), crate::exit::Exit::BadInvite);
    }

    async fn local(name_secret: &str) -> Endpoint {
        std::env::set_var("WALKIE_TEST_LOCAL", "1");
        bind(name_secret, false).await.unwrap()
    }

    fn peer_of(ep: &Endpoint) -> PeerAddr {
        PeerAddr {
            id: None,
            ticket: Some(my_addr(ep, false)),
        }
    }

    #[tokio::test]
    async fn simultaneous_dial_leaves_exactly_one_link_both_sides_agree() {
        // spec §6.2 / §10: both sides dial at once; each applies the same rule to the two
        // connections it holds and must keep the same one (dialed by the lower id).
        let a = local(&new_secret()).await;
        let b = local(&new_secret()).await;
        let (pa, pb) = (peer_of(&a), peer_of(&b));
        let acc = |ep: Endpoint| async move { accept(&ep).await.unwrap() };
        let ((a_dialed, b_acc), (b_dialed, a_acc)) = tokio::join!(
            async { tokio::join!(async { dial(&a, &pb).await.unwrap() }, acc(b.clone())) },
            async { tokio::join!(async { dial(&b, &pa).await.unwrap() }, acc(a.clone())) },
        );
        let (a_id, b_id) = (a.id().to_string(), b.id().to_string());
        // side A: current = what it dialed (dialer a), new = what it accepted (dialer b)
        let a_keeps_own = keep(&a_id, &b_id);
        // side B: current = what it dialed (dialer b), new = what it accepted (dialer a)
        let b_keeps_own = keep(&b_id, &a_id);
        assert_ne!(
            a_keeps_own, b_keeps_own,
            "exactly one side's own dial survives"
        );
        // the survivor is the connection dialed by the lower id, seen from both ends
        let (dialer_conn, acceptor_conn) = if a_id < b_id {
            (a_dialed, b_acc)
        } else {
            (b_dialed, a_acc)
        };
        let (mut tx, _rx) = dialer_conn.open_bi().await.unwrap();
        write_frame(&mut tx, &Wire::Waiting).await.unwrap();
        let (_tx2, mut rx2) = acceptor_conn.accept_bi().await.unwrap();
        assert_eq!(read_frame(&mut rx2).await.unwrap(), Wire::Waiting);
    }

    #[tokio::test]
    async fn unpinned_key_is_refused_pinned_key_is_welcome() {
        use crate::daemon::authorize;
        use crate::model::Role;
        let host = local(&new_secret()).await;
        let friend = local(&new_secret()).await;
        let stranger = local(&new_secret()).await;
        let pinned = friend.id().to_string();
        let host_addr = peer_of(&host);
        let h = host.clone();
        tokio::spawn(async move {
            let mut keep_alive = vec![];
            for _ in 0..2 {
                let conn = accept(&h).await.unwrap();
                let pinned = pinned.clone();
                let mut auth = move |remote: &str, tok: Option<&str>| {
                    authorize(Role::Host, Some(&pinned), None, None, 0, remote, tok)
                };
                keep_alive.push(
                    server_handshake(&conn, &mut auth, my_addr(&h, false))
                        .await
                        .ok(),
                );
            }
            // dropping a Link closes its connection; let the client finish first
            tokio::time::sleep(Duration::from_secs(2)).await;
        });
        let c = dial(&stranger, &host_addr).await.unwrap();
        let e = client_handshake(&c, Some("x".into()), my_addr(&stranger, false))
            .await
            .unwrap_err();
        assert_eq!(crate::exit::exit_of(&e), crate::exit::Exit::BadInvite);
        let c = dial(&friend, &host_addr).await.unwrap();
        client_handshake(&c, None, my_addr(&friend, false))
            .await
            .expect("the pinned key reconnects without a token");
    }
}
