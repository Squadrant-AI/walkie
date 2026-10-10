#!/bin/sh
# Installs walkie. Works for public and private repos:
#   1. files already in the current directory (from `gh release download`) are used first;
#   2. else it downloads with curl (public repo);
#   3. else, if `gh` is logged in, with `gh release download` (private repo).
# The SHA-256 checksum must match. If `gh` is available the build attestation must verify,
# and a failed verification is fatal. Without `gh` only the checksum is checked (warned).
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
mkdir -p "$dir"
tmp=$(mktemp -d)
trap 'rm -rf "$tmp"' EXIT

if [ -f "./$name" ] && [ -f "./$name.sha256" ]; then
  cp "./$name" "$tmp/w.tgz"; cp "./$name.sha256" "$tmp/w.sha256"
else
  base="https://github.com/$REPO/releases/latest/download"
  if curl -fsSL "$base/$name" -o "$tmp/w.tgz" 2>/dev/null \
     && curl -fsSL "$base/$name.sha256" -o "$tmp/w.sha256" 2>/dev/null; then
    :
  elif command -v gh >/dev/null 2>&1; then
    gh release download -R "$REPO" -p "$name" -p "$name.sha256" -D "$tmp" --clobber \
      || { echo "walkie: download failed (is \`gh auth login\` done and the release published?)" >&2; exit 1; }
    mv "$tmp/$name" "$tmp/w.tgz"; mv "$tmp/$name.sha256" "$tmp/w.sha256"
  else
    echo "walkie: cannot download $name (private repo? install gh and run \`gh auth login\`)" >&2
    exit 1
  fi
fi

expected=$(cut -d' ' -f1 "$tmp/w.sha256")
if command -v sha256sum >/dev/null 2>&1; then
  actual=$(sha256sum "$tmp/w.tgz" | cut -d' ' -f1)
else
  actual=$(shasum -a 256 "$tmp/w.tgz" | cut -d' ' -f1)
fi
[ -n "$expected" ] && [ "$expected" = "$actual" ] || { echo "walkie: checksum mismatch" >&2; exit 1; }

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
install -m 0755 "$tmp/walkie" "$dir/walkie"
echo "installed walkie to $dir/walkie"
