# walkie

A walkie-talkie for AI agents. Two agents, on any two machines, hold an end-to-end encrypted
ask/answer conversation through a plain shell command. No server to run, no account, no API key.
One static binary.

```sh
walkie open                       # host: prints an invite (single use, 10 min)
walkie join '<invite>'            # the other side
walkie send "what is the shape of /trips?"
walkie wait                       # blocks until the peer answers (or 90 s)
walkie wait --until-message       # keeps waiting past 90 s timeouts; for a background wait
walkie end                        # when the conversation is over
```

Set `WALKIE_ROOM=<id>` (printed by `open` / `join`) or pass `--room <id>`.
Agents use it through [`skill/SKILL.md`](skill/SKILL.md), a harness-neutral skill: any agent that
can run a shell command can use walkie.

## Guarantees

- **End-to-end encrypted.** QUIC with TLS 1.3 over [iroh](https://iroh.computer); each room has a
  fresh key pair and the peer is pinned by its public key. Relays only see ciphertext.
- **No infrastructure.** Direct connection when the network allows it, n0's public relays
  otherwise. No account, no key.
- **At-least-once delivery.** Messages survive dropped connections, `leave`/`rejoin` and a killed
  daemon. A message is acknowledged to the sender only after the receiving agent has printed it.
  After a killed `wait` a batch comes back with the same ids, so duplicates are recognisable.
- **Peer text is untrusted.** Printed messages are wrapped in nonce-delimited frames and stripped
  of control, ANSI, bidi and zero-width characters.

## Commands

| Command | |
|---|---|
| `open [--ttl 10m] [--idle-ttl 24h] [--relay-only]` | open a room, print the invite |
| `join <invite>` | join a room |
| `send <text>` / `send -` | send (stdin with `-`); `--reply-to <id>` |
| `send-file <path>...` | send files (regular files, max 8 MiB each); see [Sending files](#sending-files) |
| `wait [--timeout 90] [--json] [--until-message]` | wait for messages and events; `--until-message` loops past empty timeouts (exit 2) until a message or a terminal event (e.g. room ended, `3`) |
| `status [--json]`, `rooms` | state of the room / rooms on this machine |
| `leave`, `rejoin [id]`, `end` | leave (can come back), come back, end for both |

## Sending files

```sh
walkie send-file report.md data.bin     # sender: prints id, name, size and sha256 per file
walkie wait                             # receiver: a "WALKIE PEER FILE" frame per file
```

Files arrive as real files, not text the agent has to decode. The receiving daemon streams them
into a quarantine inbox, `~/.walkie/rooms/<room>/inbox/` (directory `0700`, files `0600`), and
never to a path the peer chooses: only a sanitized basename is kept (no `/`, `..`, control or
bidi characters), an existing file is never overwritten (a clash gets a `-1`, `-2`... suffix; a
redelivered identical file reuses the existing one), and the `wait` frame shows the local path.
The frame also shows the name, size and a SHA-256 that walkie computes over the bytes it wrote;
the sender's own SHA-256 travels with the file and a mismatch is reported (`NOT SAVED`), not
saved. Files in the inbox are peer data: the agent leaves them there until its user approves
reading or moving them.

- Limit: 8 MiB per file (the 16 KiB limit applies to text only). Directories and other non-regular
  files are refused. Several paths in one command are checked first, so a bad one sends nothing.
- Delivery is chunked over the same sequence/ack machinery as text: at-least-once, resent after a
  reconnect, never duplicated. An unread file counts against a 16 MiB outbox of files per peer.
- Needs walkie newer than 0.1.2 on both sides. Against an older peer, `send-file` fails with a
  message telling the user to run `walkie update`; text keeps working.

## Exit codes

| Code | Meaning |
|---|---|
| `0` | OK |
| `1` | error (details on stderr) |
| `2` | `wait` timed out with nothing to print |
| `3` | the room has ended |
| `4` | no such room, or several and none selected |
| `5` | the last event in the batch was `peer-left` |
| `6` | the other side is unreachable right now; retry |
| `7` | `join`: invite invalid, expired or already used |
| `8` | the environment can't run walkie (no network, no sockets, no background processes) |

## Install

```sh
curl -fsSL https://github.com/Squadrant-AI/walkie/releases/latest/download/install.sh | sh
```

This installs the `walkie` binary to `~/.local/bin` (override with `WALKIE_INSTALL_DIR`) and the
agent skill for each agent it finds: Claude Code (`~/.claude/skills/walkie`), opencode
(`~/.config/opencode/skills/walkie`) and Codex (`~/.codex/skills/walkie`). An existing, different
skill file is saved as `SKILL.md.bak`. Set `WALKIE_NO_SKILL=1` to install only the binary.

**Verify:** the installer always checks the SHA-256 checksum of the binary and skill, and, if `gh`
is installed, the build attestation of the binary; a failed check aborts the install.

Private repo fallback (needs `gh auth login`):

```sh
gh release download -R Squadrant-AI/walkie -p install.sh -p 'walkie-*' -p 'SKILL.md*' && sh install.sh
```

## Updates

walkie checks for a newer release at most once every 24 hours (result cached in
`~/.walkie/update-check.json`, lookup limited to ~2 s, silent on any failure). In an interactive
terminal it asks `Update now? [y/N]` after the command finishes. Otherwise (agents, pipes) it
never prompts: it prints one line to stderr (`walkie: X.Y.Z is available — run \`walkie update\``)
and leaves stdout untouched. `walkie update` downloads the latest `install.sh` and replaces the
running executable in place (it refuses if that directory is not writable). The check is skipped
for `wait`, in the background daemon, when `CI` is set, and when `WALKIE_NO_UPDATE_CHECK=1`.

### Other ways to install

```sh
cargo binstall walkie        # prebuilt release binary
cargo install walkie --locked # compiles from source
```

Neither installs the agent skill: run `install.sh` (above) or copy `skill/SKILL.md` into your agent's skills directory.

## Known limits

- Exactly two participants per room; pull-only (`wait`), no push into a running agent.
- n0's public relays are rate-limited and meant for development and hobby use.
- Without `--relay-only` your IP addresses are visible to the peer (they are in the invite and
  used for direct connections). `--relay-only` keeps them out of the invite and off the wire.
- Local state is plaintext under `~/.walkie` (mode 0700/0600), including the room's secret key.

More: [design spec](docs/specs/2026-10-10-walkie-v1-design.md),
[explainer](docs/walkie-v1-explainer.html).

## License

MIT
