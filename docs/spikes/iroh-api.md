# Task 0 spike: iroh 1.x API and environment facts

Verified 2026-10-10 on macOS (Darwin 25.6, arm64), cargo 1.91.0. Every snippet below was compiled and run
in a throwaway crate (`spikes/iroh-probe`, git-ignored).

Versions: `iroh 1.3.0`, `iroh-tickets 1.0.0`, `iroh-base 1.3.0`, `iroh-relay 1.3.0`, `iroh-dns 1.3.0`.

```toml
iroh = "1"                 # default features include tls-ring
iroh-tickets = "1"
# dev-only, for the local test relay:
# iroh = { version = "1", features = ["test-utils"] }
```

Imports used throughout:

```rust
use iroh::address_lookup::AddrFilter;
use iroh::endpoint::{presets, Connection, RecvStream, SendStream};
use iroh::{Endpoint, EndpointAddr, EndpointId, RelayMode, SecretKey};
use iroh_tickets::endpoint::EndpointTicket;
```

## Differences from what the plan first guessed (all fixed in Tasks 9 and 14)

| Plan guess | Verified |
|---|---|
| `SecretKey::generate(&mut OsRng)` | `SecretKey::generate()` (no RNG argument) |
| `secret.to_string()` | `SecretKey` has **no `Display`**. Hex-encode `to_bytes()`; `SecretKey::from_str` parses that 64-char hex |
| `presets::Empty` for tests | `Empty` fails to bind (`Missing or incompatible rustls crypto provider configured`). Use `presets::Minimal` |
| `.bind_addr_v4("127.0.0.1:0".parse()?)` | No such method. Use `.clear_ip_transports().bind_addr("127.0.0.1:0")?` (fallible) |
| `conn.remote_id()?` | `Connection::remote_id()` is infallible and returns `EndpointId` (the `?` form is on `Incoming`/`Accepting`) |
| `conn.local_id()` | Does not exist. Use `endpoint.id()`, or the id inside our own ticket |
| `is_relay_path` unknown | `conn.paths().iter().find(\|p\| p.is_selected()).map(\|p\| p.is_relay())` |
| relay-only needs IP stripping from the ticket | Not needed: `clear_ip_transports()` leaves no IP in `ep.addr()` |

## 1. Building endpoints

```rust
// SecretKey <-> string. No Display impl, so encode by hand.
fn new_secret() -> String {
    SecretKey::generate().to_bytes().iter().map(|b| format!("{b:02x}")).collect() // 64 hex chars
}
let sk = SecretKey::from_str(&secret)?;            // parses the same hex
let id = sk.public();                              // PublicKey == EndpointId

// Production: N0 preset (n0 relays + Pkarr publisher + Pkarr/DNS resolvers)
let ep = Endpoint::builder(presets::N0)
    .secret_key(sk)
    .alpns(vec![b"walkie/1".to_vec()])
    .bind().await?;

// Tests: no relay, no discovery, bound to 127.0.0.1 only
let ep = Endpoint::builder(presets::Minimal)       // Minimal = crypto provider only
    .clear_ip_transports()                         // drop the default 0.0.0.0 / [::] sockets
    .bind_addr("127.0.0.1:0")?
    .secret_key(sk)
    .alpns(vec![b"walkie/1".to_vec()])
    .bind().await?;
// ep.bound_sockets() == [127.0.0.1:<port>]
```

Run output: `secret len 64`, `a.bound_sockets [127.0.0.1:51891]`.
`Builder` defaults are `RelayMode::Disabled` and no address lookup, so `Minimal` needs no further clearing.

## 2. Waiting until online

`Endpoint::online()` returns `()` once a home relay connection is up. It never completes with relay disabled,
so always wrap it in a timeout.

```rust
tokio::time::timeout(Duration::from_secs(10), ep.online()).await   // Err(Elapsed) on timeout
```

Run: N0 endpoint was online within 15 s on this machine; with `RelayMode::Disabled`, `online()` timed out
(as expected, so skip it in test mode).

## 3. Own address and ticket

```rust
let addr: EndpointAddr = ep.addr();                          // sync snapshot
let ticket: String = EndpointTicket::new(addr.clone()).to_string();
let back: EndpointAddr = EndpointTicket::from_str(&ticket)?.endpoint_addr().clone();
assert_eq!(back, addr);
let id: EndpointId = back.id;                                // pub field
```

Run: `ticket has '.': false`; the string starts with `endpoint` followed by lowercase base32
(e.g. `endpointabq5auxdevpd...`). The round trip equals the original `EndpointAddr`.

Call `ep.addr()` after `online()` so the relay URL is included (all runs above did).
With default (non-relay-only) settings the ticket includes the machine's local and public-interface IPs
(observed: `Relay(..), Ip(127.0.0.1:..), Ip(192.168.x.x:..)`).

## 4. connect / accept / remote id

```rust
// dialer
let conn: Connection = ep.connect(addr, b"walkie/1").await?;      // addr: EndpointAddr (or EndpointId, see below)
// acceptor
let incoming = ep.accept().await;                                  // Option<Incoming>; None when endpoint closed
let conn: Connection = incoming.expect("closed").await?;           // Incoming is a Future<Output=Result<Connection,_>>
let remote: EndpointId = conn.remote_id();                         // infallible on Connection
let me: EndpointId = ep.id();                                      // no Connection::local_id
```

`remote == a.id()` was `true` on both sides. Dial by id alone: `EndpointAddr::from(id)` (an `EndpointId` also
works as the first argument of `connect`). It needs an address lookup service. Verified:

- N0 preset on the real network: `b.connect(a.id(), ALPN)` succeeded via n0 DNS/Pkarr (with a relay-only `a`).
- Local test pkarr+DNS server (`iroh::test_utils::DnsPkarrServer`): succeeded.
- `Minimal` with no lookup: `timed out` (nothing to resolve).

`EndpointId` `Display` is 64 hex chars and `str::parse::<EndpointId>()` round-trips (it also accepts base32).

## 5. Streams

```rust
let (mut tx, mut rx): (SendStream, RecvStream) = conn.open_bi().await?;     // dialer
let (mut tx, mut rx) = conn.accept_bi().await?;                             // acceptor
```

Compiled with `fn assert_read<T: tokio::io::AsyncRead + Unpin>(); assert_read::<RecvStream>();` and the
`AsyncWrite` equivalent for `SendStream`: both hold. `tokio::io::AsyncReadExt::read_exact` and
`AsyncWriteExt::write_all` work.

## 6. Direct or relay path

```rust
let sel = conn.paths().iter().find(|p| p.is_selected());      // PathList snapshot (closed paths excluded)
let is_relay: Option<bool> = sel.map(|p| p.is_relay());       // also p.is_ip(), p.remote_addr()
```

Run: loopback test mode `path ip=true relay=false selected=true remote=Ip(127.0.0.1:..)`.
With a test relay and IPs allowed, both paths listed; the IP path was selected (relay `selected=false`).
Relay-only: `path ip=false relay=true selected=true remote=Relay(https://...)`.
Live updates: `conn.paths_stream()` / `conn.path_events()`. Before the first path is selected, `selected` is `None`.

## 7. Close and detect close

```rust
conn.close(7u32.into(), b"bye");                 // (VarInt, &[u8]); non-async
conn.close_reason();                             // Option<ConnectionError>: None while open
let reason = conn.closed().await;                // resolves when the connection closes
```

Run: before close `None`; after local close `Some(LocallyClosed)`; the peer's `closed().await` gave
`ApplicationClosed(ApplicationClose { error_code: 7, reason: b"bye" })` and its `close_reason()` returned the same.

## 8. Relay-only: FEASIBLE (API-verified; packet capture PENDING)

```rust
let ep = Endpoint::builder(presets::N0)
    .clear_ip_transports()                          // no UDP socket bound at all: bound_sockets() == []
    .addr_filter(AddrFilter::relay_only())          // publish only relay URLs (belt and braces)
    .alpns(vec![b"walkie/1".to_vec()])
    .bind().await?;
```

- `Builder::clear_ip_transports()` removes every IP transport. The endpoint then has only the relay transport:
  `ep.bound_sockets()` is `[]` and `ep.addr()` has only `Relay(..)` (verified with the real n0 relay and a local test relay).
- `AddrFilter` is `iroh::address_lookup::AddrFilter` (re-exported from `iroh-dns`); `Builder::addr_filter(AddrFilter::relay_only())`
  filters all published address data. `PkarrPublisher` already defaults to `relay_only()` (see `address_lookup/pkarr.rs`),
  so the N0 preset never publishes IPs. Verified against a local pkarr/DNS server: with a normal IP transport,
  `ep.addr()` had `Relay + 2 Ip`, but the **published record resolved by another endpoint had only `Relay`**.
- Ticket with relay-only: `endpointabq5auxdevpdiyyziyv4peoiovuc6zkl5bmrwhb5xjxdzm6vyfqvkaiaenuhi5dqom5c6l3bobztcljrfzzgk3dbpexg4mbonfzg62bonruw42zof4`
  decodes to `EndpointAddr { addrs: {Relay(https://aps1-1.relay.n0.iroh.link./)} }`: no IPs.
- Message exchange on the real n0 relays, both sides relay-only, dialing by `EndpointId` only: `ping`/`pong` round trip OK;
  the connection's only path was `ip=false relay=true sel=true`.
- Without the IP transport, a direct UDP flow between the peers cannot exist (no socket is bound).
- **tcpdump capture: PENDING — operator to run in Task 16** (needs sudo). Command: `sudo tcpdump -n udp and host <peer-ip>`; expect no peer-to-peer packets.

Consequence for Task 14: the feature is feasible, so Task 14 proceeds as written. No IP-stripping of the ticket is needed.
Side effect to note: in the default (non-relay-only) mode the ticket embeds the host's LAN and interface IPs.

## 9. Local relay server for tests

Needs iroh feature `test-utils` (dev-dependency `iroh = { version = "1", features = ["test-utils"] }`).

```rust
let (relay_map, relay_url, _guard) = iroh::test_utils::run_relay_server().await?;   // keep _guard alive
// relay_url: RelayUrl, e.g. https://127.0.0.1:51455/  (self-signed cert)

let ep = Endpoint::builder(presets::Minimal)
    .relay_mode(RelayMode::Custom(relay_map))                      // or RelayMode::custom([relay_url])
    .ca_tls_config(iroh::tls::CaTlsConfig::insecure_skip_verify()) // required: the test relay is self-signed
    .secret_key(sk)
    .alpns(vec![b"walkie/1".to_vec()])
    .bind().await?;
ep.online().await;
```

Verified: two such endpoints connected through the test relay by `EndpointAddr` and exchanged `ping`/`pong`.
With `.clear_ip_transports().addr_filter(AddrFilter::relay_only())` added, the address is `{Relay(https://127.0.0.1:<port>/)}`
and the selected path is relay. A daemon running as a separate process can be pointed at the relay with
`RelayMode::custom([url.parse()?])` (compiled and run: `online via https://127.0.0.1:51949/`).
For offline discovery tests, `iroh::test_utils::DnsPkarrServer::run().await?` plus `.preset()` provides a local DNS/pkarr pair.

## Environment facts

**macOS `$TMPDIR`.** `echo -n $TMPDIR | wc -c` → 49 (`/var/folders/rv/y35yc8ns24g7js2zg0mtzs3r0000gn/T/`, trailing slash).
`$TMPDIR/walkie/abcdef.sock` → 68 bytes, under the 104-byte `sun_path` limit on macOS. Confirmed. (A sandbox or CI may use a
different `TMPDIR`; the plan's Task 6 runtime-dir check still has to guard the length.)

**Claude Code Bash tool.** Default timeout 120 000 ms, maximum 600 000 ms via the `timeout` parameter (tool contract).
Detached children survive across Bash calls:
`perl -e 'use POSIX; POSIX::setsid(); exec "sleep","300"' &` and `nohup sleep 301 &` were both still running in the next
call, reparented to PID 1 (`PPID 1`). **macOS has no `setsid` binary**, so the daemon must call `libc::setsid()` itself
(or double-fork) rather than shelling out. Running as uid 501.

**Codex CLI** (`codex-cli 0.147.0` installed; no real session started, to avoid billing). Unverified beyond help text:
- `codex --help` / `codex exec --help` expose `-s/--sandbox <SANDBOX_MODE>` (read-only, workspace-write, danger-full-access) and a
  separate `codex sandbox` command; there is **no timeout flag** in the help. `~/.codex/config.toml` sets no timeouts.
- Shell tool default timeout, max timeout, early-yield behaviour: **unverified**. Assume the shell call can be cut off at
  an unknown limit, so `walkie wait` must return well before it (keep the plan's bounded-wait behaviour).
- Whether a background process survives the tool call: **unverified**.
- Whether `workspace-write` allows network: **unverified**. Recollection of the Codex docs (not checked here) is that it is network-off by default, which would
  block the relay (`wait_online` → `Exit::Environment`). Verify in Task 16 with a real session.

**Gemini CLI.** Not installed here (`gemini: command not found`). Shell tool default timeout: **unverified**.

## Probe sources

Kept only in the git-ignored `spikes/iroh-probe/src/bin/` (`canon.rs`, `relay.rs`, `publish.rs`, `byid.rs`, `n0relay.rs`,
`custom.rs`, `netcheck.rs`). `netcheck.rs` is the Task 9 `net.rs` iroh code copied from the plan with crate types stubbed; it compiled and ran.
