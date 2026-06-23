#!/usr/bin/env bash
# tests/launcher_selfupdate_bat.sh — REAL cmd.exe integration tests for the
# mine-auto.bat launcher self-update startup TRAMPOLINE (v0.1.11 brick fix).
#
# WHY THIS EXISTS: the prior .bat suite only STATIC-grep'd mine-auto.bat. That is
# how the brick slipped through — a cross-volume staged %SELF_NEW% could be left
# TRUNCATED-but-non-zero, and the trampoline promoted it onto the live launcher
# with ONLY a "size != 0" guard (no SHA re-verify) → a severed .bat that dies with
# `. was unexpected at this time` and can neither mine nor self-heal. These tests
# DRIVE THE REAL TRAMPOLINE BLOCK in actual cmd.exe and prove the fix empirically.
#
# HOW: we EXTRACT the real trampoline block out of mine-auto.bat (from the
# "NO-BRICK launcher promote" marker up to — but not including — the
# "call :update_check" line that follows it), wrap it in a tiny harness that sets
# only the vars the block reads (DIR / SELF_NEW / SELF_SHA / SELF_MAX_PROMOTE,
# with %~f0 = the harness path so staging is sandboxed and same-volume), and
# appends `echo REACHED_NORMAL_STARTUP` after it (the fall-through = "kept the live
# launcher"). The block is byte-for-byte from the source — we test shipping code.
#
# PROVES (the task's required evidence):
#   (a) GOOD staged .new (correct SHA)      → promoted + live launcher relaunched
#   (b) TRUNCATED .new (size!=0, wrong SHA) → REJECTED; live launcher BYTE-IDENTICAL
#   (c) staging dir == launcher dir         → :update_launcher_self stages beside %~f0
#   (d) failing promote-move is BOUNDED     → exactly SELF_MAX_PROMOTE attempts, parked
#
# Run:   bash tests/launcher_selfupdate_bat.sh
# Exit:  0 = all pass, non-zero = at least one failure printed to stderr
#
# Requires: cmd.exe (invoked as `//c` so Git-Bash does not path-mangle the flag),
# cygpath, sha256sum, powershell (Get-FileHash — the trampoline itself uses it).
# Skips cleanly if cmd.exe/cygpath are unavailable (non-Windows CI).

set -uo pipefail

REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
BAT="$REPO_ROOT/mine-auto.bat"

PASS=0
FAIL=0
ok()   { echo "  [PASS] $1"; PASS=$((PASS + 1)); }
fail() { echo "  [FAIL] $1" >&2; echo "         $2" >&2; FAIL=$((FAIL + 1)); }

echo
echo "=== mine-auto.bat trampoline — REAL cmd.exe integration (v0.1.11 brick fix) ==="
echo

# ── locate cmd.exe + cygpath ──────────────────────────────────────────────────
CMD_EXE=""
for c in "${COMSPEC:-}" "/c/Windows/System32/cmd.exe" "$(command -v cmd.exe 2>/dev/null)"; do
  if [ -n "${c:-}" ] && [ -x "$c" ]; then CMD_EXE="$c"; break; fi
done
if [ -z "$CMD_EXE" ] || ! command -v cygpath >/dev/null 2>&1; then
  echo "  [SKIP] cmd.exe and/or cygpath not found — these integration tests require Windows." >&2
  echo "         (static safety for the .bat is covered by launcher_selfupdate.sh)"
  exit 0
fi

# Run a Windows .bat by absolute path. `//c` (not `/c`) stops MSYS from rewriting
# the flag into a path; cygpath -w gives the absolute C:\... path cmd.exe needs.
run_bat() { "$CMD_EXE" //c "$(cygpath -w "$1")" </dev/null >/dev/null 2>&1 || true; }
sha_of_file() { sha256sum "$1" 2>/dev/null | awk '{print $1}'; }

# Sandbox anchored under tests/ so paths are clean /c/... (NOT /tmp). Cleaned on exit.
SANDBOX="$REPO_ROOT/tests/.it-$$"
rm -rf "$SANDBOX"; mkdir -p "$SANDBOX"
trap 'rm -rf "$SANDBOX"' EXIT

# ── Extract the real trampoline block from mine-auto.bat ──────────────────────
TRAMP_START="$(grep -n 'NO-BRICK launcher promote' "$BAT" | head -1 | cut -d: -f1)"
UPDCHK="$(grep -n '^call :update_check' "$BAT" | head -1 | cut -d: -f1)"
if [ -z "$TRAMP_START" ] || [ -z "$UPDCHK" ] || [ "$TRAMP_START" -ge "$UPDCHK" ]; then
  fail "extract trampoline" "could not bracket the trampoline (marker@${TRAMP_START:-?}, update_check@${UPDCHK:-?})"
  echo "  Passed: $PASS  Failed: $FAIL"; exit 1
fi
TRAMP_END=$((UPDCHK - 1))
# The source is CRLF; sed keeps the \r, so the extracted block stays CRLF.
TRAMP_FILE="$SANDBOX/tramp.block"
sed -n "${TRAMP_START},${TRAMP_END}p" "$BAT" > "$TRAMP_FILE"

# Build a harness .bat for ONE scenario (real trampoline block embedded verbatim).
# Every line we add is written with explicit \r\n so the whole file is CRLF (the
# extracted block already is). %~f0 = the harness, so SELF_NEW/SELF_SHA land beside
# it; each run appends to %~f0.runlog so a relaunch shows as a 2nd RAN line.
make_harness() {
  local out="$1" maxp="$2"
  {
    printf '@echo off\r\n'
    printf 'setlocal EnableExtensions EnableDelayedExpansion\r\n'
    printf '>>"%%~f0.runlog" echo RAN\r\n'
    printf 'set "DIR=%%~dp0"\r\n'
    printf 'set "SELF_NEW=%%~f0.new"\r\n'
    printf 'set "SELF_SHA=%%~f0.new.sha"\r\n'
    printf 'set "SELF_MAX_PROMOTE=%s"\r\n' "$maxp"
    cat "$TRAMP_FILE"
    printf '\r\n'
    printf 'echo REACHED_NORMAL_STARTUP\r\n'
    printf '>>"%%~f0.runlog" echo NORMALSTARTUP\r\n'
    printf 'exit /b 0\r\n'
  } > "$out"
}

# The promote helper is detached (~4s ping before it acts), so poll for it to
# finish (it self-deletes) then settle.
wait_helper() {
  local helper="$1" i
  for i in $(seq 1 18); do
    sleep 1
    [ ! -e "$helper" ] && { sleep 1; return; }
  done
}

# ════════════════════════════════════════════════════════════════════════════
#  (a) GOOD staged .new (correct SHA) → promoted + relaunched
# ════════════════════════════════════════════════════════════════════════════
A="$SANDBOX/a"; mkdir -p "$A"
A_BAT="$A/launch.bat"
make_harness "$A_BAT" 5
A_BEFORE="$(sha_of_file "$A_BAT")"
# The staged NEW launcher must itself log a RAN line so that, once promoted and
# relaunched by the helper, the runlog shows a SECOND run (= relaunch observed).
printf '@echo off\r\n>>"%%~f0.runlog" echo RAN\r\necho PROMOTED-NEW-LAUNCHER-v0.1.11\r\nexit /b 0\r\n' > "$A_BAT.new"
sha_of_file "$A_BAT.new" > "$A_BAT.new.sha"
A_NEW_SHA="$(sha_of_file "$A_BAT.new")"
run_bat "$A_BAT"; wait_helper "$A/csd-launcher-promote.cmd"

A_AFTER="$(sha_of_file "$A_BAT")"
if [ "$A_AFTER" = "$A_NEW_SHA" ] && [ "$A_AFTER" != "$A_BEFORE" ]; then
  ok "(a) GOOD .new promoted: live launcher now == staged new bytes (SHA match, changed from before)"
else
  fail "(a) GOOD promote" "after=$A_AFTER staged=$A_NEW_SHA before=$A_BEFORE"
fi
if [ ! -e "$A_BAT.new" ] && [ ! -e "$A_BAT.new.sha" ]; then
  ok "(a) GOOD .new promoted: staged .new and .sha consumed/cleaned"
else
  fail "(a) cleanup" ".new ($( [ -e "$A_BAT.new" ] && echo present)) / .sha ($( [ -e "$A_BAT.new.sha" ] && echo present)) still present"
fi
A_RUNS="$(grep -c '^RAN' "$A_BAT.runlog" 2>/dev/null || echo 0)"
if [ "${A_RUNS:-0}" -ge 2 ]; then
  ok "(a) GOOD .new promoted: helper RELAUNCHED the updated launcher (runlog has $A_RUNS runs)"
else
  fail "(a) relaunch" "expected >=2 runs (relaunch); got ${A_RUNS:-0}; runlog=[$(tr '\n' ',' < "$A_BAT.runlog" 2>/dev/null)]"
fi

# ════════════════════════════════════════════════════════════════════════════
#  (b) TRUNCATED .new (size!=0, WRONG SHA) → REJECTED; live launcher BYTE-IDENTICAL
#      ── THE BRICK PROOF ──
# ════════════════════════════════════════════════════════════════════════════
B="$SANDBOX/b"; mkdir -p "$B"
B_BAT="$B/launch.bat"
make_harness "$B_BAT" 5
B_BEFORE="$(sha_of_file "$B_BAT")"
# Persist the digest of the FULL intended launcher; stage only a truncated prefix
# (non-zero, so it passes the size!=0 guard) — exactly an interrupted copy.
printf '@echo off\r\necho FULL-NEW-LAUNCHER-that-got-cut-off-mid-copy\r\necho line2\r\necho line3\r\nexit /b 0\r\n' > "$B/full-intended"
sha_of_file "$B/full-intended" > "$B_BAT.new.sha"     # digest of the FULL file
printf '@ec' > "$B_BAT.new"                            # truncated staged bytes (size != 0)
B_TRUNC_SZ="$(wc -c < "$B_BAT.new" | tr -d ' ')"
run_bat "$B_BAT"; wait_helper "$B/csd-launcher-promote.cmd"

B_AFTER="$(sha_of_file "$B_BAT")"
if [ "$B_AFTER" = "$B_BEFORE" ]; then
  ok "(b) TRUNCATED .new REJECTED: live launcher BYTE-IDENTICAL after (NO BRICK) [staged ${B_TRUNC_SZ}B, size!=0]"
else
  fail "(b) BRICK!" "live launcher CHANGED — before=$B_BEFORE after=$B_AFTER (THIS IS THE BRICK)"
fi
if [ ! -e "$B_BAT.new" ] && [ ! -e "$B_BAT.new.sha" ]; then
  ok "(b) TRUNCATED .new REJECTED: bad staged .new and its .sha discarded (fail-closed)"
else
  fail "(b) discard" ".new ($( [ -e "$B_BAT.new" ] && echo present)) / .sha ($( [ -e "$B_BAT.new.sha" ] && echo present)) not discarded"
fi
if [ ! -e "$B/csd-launcher-promote.cmd" ]; then
  ok "(b) TRUNCATED .new REJECTED: no promote helper generated (never handed off the bad file)"
else
  fail "(b) no-helper" "a promote helper was written despite the SHA mismatch"
fi
B_RUNS="$(grep -c '^RAN' "$B_BAT.runlog" 2>/dev/null || echo 0)"
if grep -q NORMALSTARTUP "$B_BAT.runlog" 2>/dev/null && [ "${B_RUNS:-0}" -eq 1 ]; then
  ok "(b) TRUNCATED .new REJECTED: fell through to normal startup on the GOOD live launcher (1 run, no relaunch)"
else
  fail "(b) fall-through" "expected NORMALSTARTUP + exactly 1 run; runs=${B_RUNS:-0}; runlog=[$(tr '\n' ',' < "$B_BAT.runlog" 2>/dev/null)]"
fi

# ════════════════════════════════════════════════════════════════════════════
#  (b2) MISSING digest sidecar (.new present, NO .sha) → REJECTED fail-closed.
#       Covers the trampoline's "staged launcher has NO persisted digest" branch:
#       a non-zero .new with no companion .sha must NOT be promoted (we cannot
#       re-verify it) → discard, keep the GOOD live launcher.
# ════════════════════════════════════════════════════════════════════════════
B2="$SANDBOX/b2"; mkdir -p "$B2"
B2_BAT="$B2/launch.bat"
make_harness "$B2_BAT" 5
B2_BEFORE="$(sha_of_file "$B2_BAT")"
printf '@echo off\r\necho SOME-NEW-LAUNCHER-but-no-digest\r\nexit /b 0\r\n' > "$B2_BAT.new"  # NO .sha written
run_bat "$B2_BAT"; wait_helper "$B2/csd-launcher-promote.cmd"
B2_AFTER="$(sha_of_file "$B2_BAT")"
if [ "$B2_AFTER" = "$B2_BEFORE" ] && [ ! -e "$B2_BAT.new" ] && [ ! -e "$B2/csd-launcher-promote.cmd" ]; then
  ok "(b2) MISSING digest REJECTED: live launcher byte-identical, .new discarded, no helper (fail-closed)"
else
  fail "(b2) missing-digest" "after=$B2_AFTER before=$B2_BEFORE; .new=$( [ -e "$B2_BAT.new" ] && echo present || echo gone); helper=$( [ -e "$B2/csd-launcher-promote.cmd" ] && echo present || echo none)"
fi

# ════════════════════════════════════════════════════════════════════════════
#  (c) staging dir == launcher dir (:update_launcher_self stages beside %~f0)
#      Executes the REAL SELF_DL assignment from the source in cmd.exe and confirms
#      it expands to a SIBLING of %~f0, NOT under %DIR% (%LOCALAPPDATA%).
# ════════════════════════════════════════════════════════════════════════════
C="$SANDBOX/c"; mkdir -p "$C"
C_BAT="$C/probe.bat"
SELF_DL_LINE="$(grep -E '^[[:space:]]*set "SELF_DL=' "$BAT" | head -1 | sed -E 's/^[[:space:]]+//')"
{
  printf '@echo off\r\n'
  printf 'setlocal EnableExtensions EnableDelayedExpansion\r\n'
  printf 'set "DIR=%%LOCALAPPDATA%%\\csd-pool-miner"\r\n'
  printf '%s\r\n' "$SELF_DL_LINE"
  printf 'for %%%%A in ("%%~f0") do set "LAUNCH_DIR=%%%%~dpA"\r\n'
  printf 'for %%%%A in ("!SELF_DL!") do set "STAGE_DIR=%%%%~dpA"\r\n'
  printf '>"%%~f0.dirs" echo LAUNCH_DIR=!LAUNCH_DIR!\r\n'
  printf '>>"%%~f0.dirs" echo STAGE_DIR=!STAGE_DIR!\r\n'
  printf 'exit /b 0\r\n'
} > "$C_BAT"
run_bat "$C_BAT"
C_LAUNCH_DIR="$(grep '^LAUNCH_DIR=' "$C_BAT.dirs" 2>/dev/null | sed 's/^LAUNCH_DIR=//' | tr -d '\r')"
C_STAGE_DIR="$(grep '^STAGE_DIR=' "$C_BAT.dirs" 2>/dev/null | sed 's/^STAGE_DIR=//' | tr -d '\r')"
if [ -n "$C_LAUNCH_DIR" ] && [ "$C_LAUNCH_DIR" = "$C_STAGE_DIR" ]; then
  ok "(c) staging dir == launcher dir: SELF_DL resolves beside %~f0 (same volume → atomic rename)"
  echo "         LAUNCH_DIR=$C_LAUNCH_DIR"
  echo "         STAGE_DIR =$C_STAGE_DIR"
else
  fail "(c) staging dir" "SELF_DL dir [$C_STAGE_DIR] != launcher dir [$C_LAUNCH_DIR] (cross-volume copy = the brick)"
fi

# ════════════════════════════════════════════════════════════════════════════
#  (d) failing promote-move is BOUNDED (exactly SELF_MAX_PROMOTE attempts, parks)
#      Reproduces the SHIPPING bound control flow verbatim (set /a TRIES+=1 ;
#      if not errorlevel 1 goto promote_ok ; if %TRIES% GEQ MAX goto promote_fail ;
#      goto promote_retry) with a move that fails every time (target = nonexistent
#      dir), counts attempts, and asserts it stops at MAX + parks .fail.
# ════════════════════════════════════════════════════════════════════════════
D="$SANDBOX/d"; mkdir -p "$D"
D_MAX=3
D_BAT="$D/bound.bat"
{
  printf '@echo off\r\n'
  printf 'set "MAXP=%s"\r\n' "$D_MAX"
  printf 'set "BADDST=%%~dp0NO_SUCH_DIR\\x"\r\n'
  printf 'set "SRC=%%~dp0src"\r\n'
  printf 'echo data > "%%SRC%%"\r\n'
  printf 'set "TRIES=0"\r\n'
  printf ':promote_retry\r\n'
  printf 'set /a TRIES+=1\r\n'
  printf '>>"%%~dp0attempts.log" echo ATTEMPT %%TRIES%%\r\n'
  printf 'move /Y "%%SRC%%" "%%BADDST%%" >nul 2>&1\r\n'
  printf 'if not errorlevel 1 goto promote_ok\r\n'
  printf 'if %%TRIES%% GEQ %%MAXP%% goto promote_fail\r\n'
  printf 'goto promote_retry\r\n'
  printf ':promote_ok\r\n'
  printf '>"%%~dp0bound.result" echo OK\r\n'
  printf 'goto :eof\r\n'
  printf ':promote_fail\r\n'
  printf 'move /Y "%%SRC%%" "%%SRC%%.fail" >nul 2>&1\r\n'
  printf '>"%%~dp0bound.result" echo FAIL_PARKED\r\n'
  printf 'goto :eof\r\n'
} > "$D_BAT"
run_bat "$D_BAT"
D_ATTEMPTS="$(grep -c '^ATTEMPT' "$D/attempts.log" 2>/dev/null || echo 0)"
D_RESULT="$(cat "$D/bound.result" 2>/dev/null | tr -d '\r')"
if [ "${D_ATTEMPTS:-0}" = "$D_MAX" ] && [ "$D_RESULT" = "FAIL_PARKED" ] && [ -e "$D/src.fail" ]; then
  ok "(d) bounded promote: exactly $D_MAX move attempts then park-as-.fail (no infinite re-fire)"
else
  fail "(d) bound" "expected $D_MAX attempts + FAIL_PARKED + src.fail; got attempts=${D_ATTEMPTS:-0} result=$D_RESULT fail=$( [ -e "$D/src.fail" ] && echo present || echo absent)"
fi
# Structural cross-check: the SHIPPING helper-gen really emits the bound guard.
if grep -qE 'if %%TRIES%% GEQ %SELF_MAX_PROMOTE% goto promote_fail' "$BAT"; then
  ok "(d) bound guard present in shipping helper-gen (TRIES GEQ SELF_MAX_PROMOTE -> :promote_fail)"
else
  fail "(d) guard" "shipping helper-gen is missing the 'TRIES GEQ SELF_MAX_PROMOTE goto promote_fail' bound"
fi

echo
echo "========================================"
echo "  Passed: $PASS  Failed: $FAIL"
echo "========================================"
echo
[ "$FAIL" -gt 0 ] && exit 1
exit 0
