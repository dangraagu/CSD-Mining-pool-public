#!/usr/bin/env bash
# tests/bash_source_piped.sh — FIX 1 (v0.1.15): SCRIPT_DIR must survive curl|bash.
#
# THE BUG: install-csd-miner.sh and create-wallet.sh both run under
# `set -euo pipefail` and compute their own dir with
#     SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
# When the script is delivered over a pipe (curl ... | bash), bash reads the
# program from stdin: $0 is "bash" and the BASH_SOURCE array is EMPTY. Under
# `set -u`, the bare `${BASH_SOURCE[0]}` expansion is an UNBOUND VARIABLE and the
# script ABORTS with "BASH_SOURCE[0]: unbound variable" before doing anything.
#
# THE FIX: `${BASH_SOURCE[0]:-$0}` — fall back to $0 when the array is unset, so
# the assignment never trips set -u. (Piped, dirname "$0" -> dirname "bash" -> ".",
# which is the sane "current directory" behaviour for a curl|bash run.)
#
# HERMETIC: we don't run the whole installer (it would try to hit the network).
# We extract ONLY the SCRIPT_DIR= assignment line from each script and execute it
# under `set -euo pipefail` in a bash invoked FROM A PIPE (so BASH_SOURCE is empty,
# exactly reproducing the curl|bash environment), then assert it did not abort.
#
# Run:  bash tests/bash_source_piped.sh
# Exit: 0 = all pass, non-zero = a script still aborts when piped.

set -uo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]:-$0}")/.." && pwd)"
PASS=0; FAIL=0
ok()   { echo "  [PASS] $1"; PASS=$((PASS + 1)); }
fail() { echo "  [FAIL] $1" >&2; echo "         $2" >&2; FAIL=$((FAIL + 1)); }

echo
echo "=== BASH_SOURCE under curl|bash + set -u (FIX 1) ==="
echo

# Pull the SCRIPT_DIR= line verbatim from a script and run it the way curl|bash
# would: feed it to `bash` on STDIN, under the same strict flags, with NO
# arguments (so $0 is "bash" and BASH_SOURCE is empty). We append an `echo OK` so
# a successful (non-aborting) run is observable on stdout.
check_piped() {
  local label="$1" script="$2"
  local line
  line="$(grep -m1 'SCRIPT_DIR="\$(cd ' "$script")" || true
  if [ -z "$line" ]; then
    fail "$label" "no SCRIPT_DIR= line found in $script"
    return
  fi
  local out rc
  out="$(printf 'set -euo pipefail\n%s\necho PIPED_OK\n' "$line" | bash 2>&1)"; rc=$?
  if [ "$rc" -eq 0 ] && printf '%s' "$out" | grep -q 'PIPED_OK'; then
    ok "$label: SCRIPT_DIR= survives curl|bash (no unbound-variable abort)"
  else
    fail "$label" "piped run aborted (rc=$rc): $(printf '%s' "$out" | tr '\n' '|')"
  fi
}

check_piped "install-csd-miner.sh" "$ROOT/install-csd-miner.sh"
check_piped "create-wallet.sh"     "$ROOT/create-wallet.sh"

echo
echo "========================================"
echo "  Passed: $PASS  Failed: $FAIL"
echo "========================================"
echo
[ "$FAIL" -gt 0 ] && exit 1
exit 0
