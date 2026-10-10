//! Forced-relay run: both daemons use `--relay-only` against a local iroh relay.
//! Needs `--features test-relay` (the test relay and its self-signed-cert escape hatch).
#![cfg(feature = "test-relay")]
mod common;
use common::*;
use iroh_tickets::endpoint::EndpointTicket;
use std::str::FromStr;

fn assert_no_ips(ticket: &str, what: &str) {
    let t = EndpointTicket::from_str(ticket).unwrap_or_else(|e| panic!("{what}: {e}"));
    let addr = t.endpoint_addr();
    assert_eq!(addr.ip_addrs().count(), 0, "{what} leaks an IP: {addr:?}");
    assert!(
        addr.relay_urls().count() > 0,
        "{what} has no relay: {addr:?}"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn relay_only_has_no_ips_and_uses_the_relay_path() {
    let (_map, relay_url, _guard) = iroh::test_utils::run_relay_server().await.unwrap();
    let url = relay_url.to_string();
    tokio::task::spawn_blocking(move || {
        let (a, b) = (Machine::with_relay(&url), Machine::with_relay(&url));
        let (c, out, err) = a.run(&["open", "--relay-only"]);
        assert_eq!(c, 0, "open failed: {err}");
        let room = field(&out, "room:").to_string();
        let invite = field(&out, "invite:").to_string();
        a.track(&room);
        let rest = invite.strip_prefix("walkie1r:").expect("relay-only prefix");
        let (ticket, _) = rest.rsplit_once('.').unwrap();
        assert_no_ips(ticket, "invite");

        let (c, _, err) = b.run(&["join", &invite]);
        assert_eq!(c, 0, "join failed: {err}");
        b.track(&room);

        // the address each side stored for the other (from Hello / Welcome) has no IPs either
        for (m, what) in [
            (&a, "host's stored peer addr"),
            (&b, "joiner's stored peer addr"),
        ] {
            let f = m
                .dir
                .path()
                .join("home/rooms")
                .join(format!("{room}.ticket"));
            let v: serde_json::Value = serde_json::from_slice(&std::fs::read(f).unwrap()).unwrap();
            assert_no_ips(v["peer_addr"].as_str().unwrap(), what);
        }

        let (_, st, _) = a.run(&["--room", &room, "status"]);
        assert!(st.contains("path: relay"), "{st}");
        let (_, st, _) = b.run(&["--room", &room, "status"]);
        assert!(st.contains("path: relay"), "{st}");

        assert_eq!(b.run(&["--room", &room, "send", "via-relay"]).0, 0);
        let out = a.wait_for(&room, "via-relay", 3);
        assert!(out.contains("via-relay"));
        assert_eq!(a.run(&["--room", &room, "send", "pong"]).0, 0);
        b.wait_for(&room, "pong", 3);
    })
    .await
    .unwrap();
}
