#!/usr/bin/env bash
# Contract test for hiveos/h-stop.sh — the STOP hook HiveOS runs to reap the
# background children h-run.sh launches before its exec (the SP2 relay + the
# auto-update sidecar), which `screen -X quit` does NOT kill.
#
# STATIC ONLY — this test never RUNS h-stop.sh (it pkills csd-relay-node /
# csd-gpu-miner / the sidecar; executing it on a live mining box would kill real
# processes). It asserts the reaper patterns, the marker consistency with
# h-run.sh, the absence of the unsafe `pkill -f h-run.sh`, and that release.yml
# ships + chmods it. Real reaping is verified on a canary rig.
set -u
ROOT="$(cd "$(dirname "$0")/.." && pwd)"
STOP="$ROOT/hiveos/h-stop.sh"
RUN="$ROOT/hiveos/h-run.sh"
RY="$ROOT/.github/workflows/release.yml"
pass=0; fail=0
ok(){ echo "  PASS: $1"; pass=$((pass + 1)); }
no(){ echo "  FAIL: $1"; fail=$((fail + 1)); }

echo "== h-stop.sh exists + reaps the right targets =="
[ -f "$STOP" ] && ok "hiveos/h-stop.sh exists" || no "hiveos/h-stop.sh MISSING"
head -1 "$STOP" | grep -q '^#!.*bash' && ok "has a bash shebang" || no "no bash shebang"
grep -q 'pkill -f csd-hive-update-sidecar' "$STOP" && ok "reaps the auto-update sidecar (by unique marker)" || no "does not reap the sidecar"
grep -qE 'pkill -x csd-relay-node' "$STOP" && ok "reaps the SP2 relay (csd-relay-node, exact comm — no broad -f match)" || no "does not reap the relay by exact name"
grep -qE 'pkill -x csd-gpu-miner' "$STOP" && ok "reaps the miner (belt-and-suspenders, exact name)" || no "does not reap the miner"
grep -qE 'exit 0' "$STOP" && ok "exits 0 (never blocks the HiveOS stop path)" || no "no explicit exit 0"

echo "== safety: must NOT use the unsafe pkill -f h-run.sh (executable lines only) =="
grep -v '^[[:space:]]*#' "$STOP" | grep -q 'pkill.*h-run' \
  && no "uses pkill against h-run.sh in code (unsafe — would match unrelated)" \
  || ok "no pkill against h-run.sh in executable code"

echo "== sidecar marker is consistent with h-run.sh =="
RUN_MARKER="$(grep -oE 'SIDE_MARKER="[^"]+"' "$RUN" | head -1 | sed -E 's/.*"([^"]+)".*/\1/')"
echo "    h-run.sh SIDE_MARKER = '$RUN_MARKER'"
[ -n "$RUN_MARKER" ] && grep -q "pkill -f $RUN_MARKER" "$STOP" \
  && ok "h-stop.sh pkills the exact marker h-run.sh launches the sidecar with" \
  || no "h-stop.sh marker != h-run.sh SIDE_MARKER (sidecar would NOT be reaped)"

echo "== release.yml ships + chmods h-stop.sh =="
grep -q 'hiveos/h-stop.sh' "$RY" && ok "h-stop.sh is in the Package cp list" || no "h-stop.sh not copied into the tarball"
grep -qE 'chmod \+x .*h-stop\.sh' "$RY" && ok "h-stop.sh is chmod +x in the package" || no "h-stop.sh not made executable"

echo
echo "  Passed: $pass  Failed: $fail"
[ "$fail" -eq 0 ] && { echo "ALL HIVEOS STOP-HOOK ASSERTIONS PASSED"; exit 0; } || { echo "STOP-HOOK TEST FAILED"; exit 1; }
