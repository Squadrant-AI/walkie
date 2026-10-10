#!/bin/sh
# Installs walkie. Works for public and private repos:
#   1. files already in the current directory (from `gh release download`) are used first;
#   2. else it downloads with curl (public repo);
#   3. else, if `gh` is logged in, with `gh release download` (private repo).
# The SHA-256 checksum must match. If `gh` is available the build attestation must verify,
# and a failed verification is fatal. Without `gh` only the checksum is checked (warned).
# It also installs the agent skill (SKILL.md, checksum-verified) for each agent found:
#   Claude Code (~/.claude), opencode (~/.config/opencode), Codex (~/.codex).
# Env: WALKIE_INSTALL_DIR (binary dir, default ~/.local/bin), WALKIE_NO_SKILL=1 (skip skills),
#      WALKIE_TARGET, WALKIE_REPO.
set -eu
REPO="${WALKIE_REPO:-Squadrant-AI/walkie}"

if [ -n "${WALKIE_TARGET:-}" ]; then
  t="$WALKIE_TARGET"
else
  os=$(uname -s); arch=$(uname -m)
  case "$os-$arch" in
    Darwin-arm64) t=aarch64-apple-darwin ;;
    Darwin-x86_64) t=x86_64-apple-darwin ;;
    Linux-x86_64) t=x86_64-unknown-linux-musl ;;
    Linux-aarch64|Linux-arm64) t=aarch64-unknown-linux-musl ;;
    *) echo "walkie: unsupported platform $os-$arch" >&2; exit 1 ;;
  esac
fi
name="walkie-$t.tar.gz"
dir="${WALKIE_INSTALL_DIR:-$HOME/.local/bin}"
tmp=$(mktemp -d)
trap 'rm -rf "$tmp"' EXIT

base="https://github.com/$REPO/releases/latest/download"

# fetch <asset> <dest>...: local file in cwd, else curl (public), else gh (private).
# Fetches every asset listed as "asset:dest"; returns non-zero if any is unavailable.
fetch() {
  ok=1
  for pair in "$@"; do
    a=${pair%%:*}; d=${pair#*:}
    if [ -f "./$a" ]; then cp "./$a" "$d" || ok=0
    elif curl -fsSL "$base/$a" -o "$d" 2>/dev/null; then :
    elif command -v gh >/dev/null 2>&1 \
         && gh release download -R "$REPO" -p "$a" -D "$tmp/gh" --clobber >/dev/null 2>&1 \
         && mv "$tmp/gh/$a" "$d"; then :
    else ok=0; fi
  done
  [ "$ok" = 1 ]
}

sha_of() {
  if command -v sha256sum >/dev/null 2>&1; then sha256sum "$1" | cut -d' ' -f1
  else shasum -a 256 "$1" | cut -d' ' -f1; fi
}

mkdir -p "$tmp/gh"
fetch "$name:$tmp/w.tgz" "$name.sha256:$tmp/w.sha256" \
  || { echo "walkie: cannot download $name (private repo? install gh and run \`gh auth login\`)" >&2; exit 1; }

expected=$(cut -d' ' -f1 "$tmp/w.sha256")
actual=$(sha_of "$tmp/w.tgz")
[ -n "$expected" ] && [ "$expected" = "$actual" ] || { echo "walkie: checksum mismatch" >&2; exit 1; }

# The skill is verified before anything is installed, so a bad SKILL.md aborts the whole install.
have_skill=0
if [ "${WALKIE_NO_SKILL:-}" != 1 ]; then
  if fetch "SKILL.md:$tmp/SKILL.md" "SKILL.md.sha256:$tmp/SKILL.md.sha256"; then
    sexp=$(cut -d' ' -f1 "$tmp/SKILL.md.sha256")
    [ -n "$sexp" ] && [ "$sexp" = "$(sha_of "$tmp/SKILL.md")" ] \
      || { echo "walkie: SKILL.md checksum mismatch" >&2; exit 1; }
    have_skill=1
  else
    echo "walkie: warning: SKILL.md not available in this release; skipping skill install" >&2
  fi
fi

if command -v gh >/dev/null 2>&1; then
  # gh needs the file to be named like the released asset
  cp "$tmp/w.tgz" "$tmp/$name"
  gh attestation verify "$tmp/$name" -R "$REPO" >/dev/null 2>&1 \
    || { echo "walkie: attestation verification FAILED; not installing" >&2; exit 1; }
  echo "attestation verified"
else
  echo "walkie: warning: gh not found, attestation not verified (checksum only)" >&2
fi

tar -xzf "$tmp/w.tgz" -C "$tmp"
mkdir -p "$dir"
install -m 0755 "$tmp/walkie" "$dir/walkie"
echo "installed walkie to $dir/walkie"
case ":$PATH:" in
  *":$dir:"*) ;;
  *) echo "walkie: $dir is not on your PATH; add: export PATH=\"$dir:\$PATH\"" ;;
esac

# install_skill <agent> <skills-dir>: never overwrites silently; a differing file is backed up.
install_skill() {
  sd="$2/walkie"; sf="$sd/SKILL.md"
  mkdir -p "$sd"
  if [ -f "$sf" ] && cmp -s "$sf" "$tmp/SKILL.md"; then
    echo "skill ($1): already up to date at $sf"; return
  fi
  if [ -f "$sf" ]; then
    cp "$sf" "$sf.bak"
    echo "skill ($1): updated $sf (previous version saved as SKILL.md.bak)"
  else
    echo "skill ($1): installed $sf"
  fi
  cp "$tmp/SKILL.md" "$sf"
}

if [ "$have_skill" = 1 ]; then
  found=0
  if [ -d "$HOME/.claude" ]; then install_skill "Claude Code" "$HOME/.claude/skills"; found=1; fi
  if [ -d "$HOME/.config/opencode" ]; then install_skill opencode "$HOME/.config/opencode/skills"; found=1; fi
  if [ -d "$HOME/.codex" ]; then install_skill Codex "$HOME/.codex/skills"; found=1; fi
  [ "$found" = 1 ] || echo "walkie: no agent config dir found (~/.claude, ~/.config/opencode, ~/.codex); skill not installed"
fi
