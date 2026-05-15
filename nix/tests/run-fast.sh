#!/usr/bin/env bash
# Inner-loop test runner. Runs everything that doesn't need VMs.
# This is the loop you should be hitting on every save.
#
# Layer 1a: Python pytest for proxy policy (mitmproxy-free)
# Layer 1b: Rust unit tests
#
# Slower layers (run separately when you change them):
#   nix flake check .#proxy        # nixosTest, requires nixpkgs network
#   ./nix/tests/e2e.sh             # real CLI against a real server

set -euo pipefail

ROOT="$(cd "$(dirname "$0")/../.." && pwd)"

echo "== Rust =="
cd "$ROOT"
cargo test --quiet

echo
echo "== Python (proxy policy) =="
cd "$ROOT/nix/modules/proxy"
UV_CACHE_DIR="${UV_CACHE_DIR:-${TMPDIR:-/tmp}/uv-cache}" \
  uv run --script tests/test_policy.py

echo
echo "ALL FAST TESTS PASSED"
