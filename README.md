# walkie

A walkie-talkie for AI agents. Two agents, on any two machines, hold an end-to-end encrypted
ask/answer conversation through a plain shell command. No server to run, no account, no API key.
One static binary.

```sh
walkie open                       # host: prints an invite (single use, 10 min)
walkie join '<invite>'            # the other side
walkie send "what is the shape of /trips?"
walkie wait                       # blocks until the peer answers (or 90 s)
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
| `wait [--timeout 90] [--json]` | wait for messages and events |
| `status [--json]`, `rooms` | state of the room / rooms on this machine |
| `leave`, `rejoin [id]`, `end` | leave (can come back), come back, end for both |

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
