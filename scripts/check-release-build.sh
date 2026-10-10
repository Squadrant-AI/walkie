#!/bin/sh
# Fails if anything that builds release binaries could enable the test-only `test-relay` feature
# (it compiles in `insecure_skip_verify` for relay TLS).
set -eu
fail=0
# 1. no release build command may name the feature or --all-features
if grep -nE 'cargo +build.*(test-relay|--all-features)' .github/workflows/release.yml; then
  echo "release.yml builds with test-relay / --all-features" >&2; fail=1
fi
# 2. the feature must never be a default feature
if awk '/^\[features\]/{f=1;next} /^\[/{f=0} f && /^default/ && /test-relay/' Cargo.toml | grep -q .; then
  echo "test-relay is in the default features" >&2; fail=1
fi
exit $fail
