#!/usr/bin/env bash
# tests/crash_surface.sh — FIX 4 (v0.1.15): surface WHY a miner keeps crashing.
#
# THE BUG: when the per-GPU miner process exits immediately (wrong build for the
# rig, missing driver lib, bad device), the launcher just logged
#   "miners not running - restarting"
# and silently relaunched the SAME crashing build forever. The actual crash reason
# (printed by the miner to its stdout.log) was (a) OVERWRITTEN on every restart
# because start_miners redirected with `>` (truncate), and (b) never shown to the
# operator. The rig looked "busy" but mined nothing, with no clue why.
#
# THE FIX (message-only, per the safety review — NO auto-swap of the build):
#   1. start_miners redirects each GPU's stdout with `>>` (append), so the crash
#      reason from the dying process is preserved across restarts, not clobbered.
#   2. On the "miners not running - restarting" path, tail the newest
#      gpu*-log/stdout.log AND print an actionable hint naming the variant and the
#      re-run options. Exposed as report_crash_logs() so it is unit-testable.
#
# HERMETIC: we extract report_crash_logs() from mine-auto.sh and drive it against a
# seeded crash log; and we static-check that start_miners appends (`>>`) the
# per-GPU stdout rather than truncating (`>`).
#
# Run:  bash tests/crash_surface.sh
# Exit: 0 = all pass

set -uo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]:-$0}")/.." && pwd)"
LAUNCHER="$ROOT/mine-auto.sh"
BAT="$ROOT/mine-auto.bat"
PASS=0; FAIL=0
ok()   { echo "  [PASS] $1"; PASS=$((PASS + 1)); }
fail() { echo "  [FAIL] $1" >&2; echo "         $2" >&2; FAIL=$((FAIL + 1)); }

echo
echo "=== crash-reason surfacing (FIX 4) ==="
echo

# ── (1) start_miners must APPEND the per-GPU stdout, not truncate it ──────────
# The redirect that captures each miner's stdout/stderr must be `>>` so a crash
# message survives the next restart. A bare `>` clobbers the evidence.
if grep -Eq 'stdout\.log" 2>&1 &' "$LAUNCHER" && grep -q '>> "\$LOGDIR/stdout.log" 2>&1 &' "$LAUNCHER"; then
  ok "mine-auto.sh start_miners appends per-GPU stdout (>> stdout.log) — crash reason survives restarts"
elif grep -q '> "\$LOGDIR/stdout.log" 2>&1 &' "$LAUNCHER"; then
  fail "start_miners redirect" "still truncates with single '>' — crash reason is overwritten each restart"
else
  fail "start_miners redirect" "could not find the per-GPU stdout redirect in mine-auto.sh"
fi

# ── (2) report_crash_logs(): tails the newest crash log + prints the hint ─────
# Extract the function body and source it in isolation, then point it at a fake
# DATA_DIR holding a gpu0-log/stdout.log with a known crash line.
FN="$(mktemp)"
awk '/^report_crash_logs\(\)[[:space:]]*\{/{f=1} f{print} f&&/^\}/{exit}' "$LAUNCHER" > "$FN"
if ! grep -q 'report_crash_logs' "$FN"; then
  fail "report_crash_logs present" "no report_crash_logs() function found in mine-auto.sh"
else
  SB="$(mktemp -d)"
  DATA="$SB/data"; mkdir -p "$DATA/gpu0-log"
  CRASH_MARK="FATAL: opencl backend: no OpenCL platforms found (this is the amd build on a non-AMD rig)"
  printf 'starting up\n%s\n' "$CRASH_MARK" > "$DATA/gpu0-log/stdout.log"
  OUT="$(DATA_DIR="$DATA" VARIANT="amd" bash -c 'set -uo pipefail; source "$1"; report_crash_logs' _ "$FN" 2>&1)"

  printf '%s' "$OUT" | grep -qF "$CRASH_MARK" \
    && ok "report_crash_logs tails the newest gpu*-log/stdout.log (shows the real crash reason)" \
    || fail "tail crash reason" "crash line not surfaced. got: $(printf '%s' "$OUT" | tr '\n' '|')"

  # Actionable hint: must name the running variant AND the re-run options.
  if printf '%s' "$OUT" | grep -qi 'amd' && printf '%s' "$OUT" | grep -Eqi 'nvidia.*amd.*cpu|re-run'; then
    ok "report_crash_logs prints an actionable hint (variant + 'nvidia | amd | cpu' re-run)"
  else
    fail "actionable hint" "no actionable variant/re-run hint. got: $(printf '%s' "$OUT" | tr '\n' '|')"
  fi
  rm -rf "$SB"
fi
rm -f "$FN"

# ── (3) mine-auto.bat mirrors the tail + actionable hint ─────────────────────
# Static check (the .bat is driven by the cmd.exe harness elsewhere): the restart
# path must tail the newest gpu*-log\stdout.log and print the re-run hint.
if grep -qi 'stdout.log' "$BAT" && grep -Eqi 'nvidia .* amd .* cpu|re-run' "$BAT" \
   && grep -qi 'miners not running' "$BAT"; then
  ok "mine-auto.bat mirrors the crash tail + actionable re-run hint"
else
  fail "mine-auto.bat mirror" "the .bat restart path does not tail stdout.log + print the re-run hint"
fi

echo
echo "========================================"
echo "  Passed: $PASS  Failed: $FAIL"
echo "========================================"
echo
[ "$FAIL" -gt 0 ] && exit 1
exit 0
