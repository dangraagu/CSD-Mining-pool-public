#!/usr/bin/env bash
# tests/installer_bootstrap_verify_bat.sh — REAL cmd.exe integration test for the
# FIRST (bootstrap) binary download+verify block in install-csd-miner.bat.
#
# THE GAP THIS LOCKS DOWN: install-csd-miner.bat fetched the initial miner exe
# from releases/latest/download/<exe> straight into %BIN% (no temp) and ran the
# hand-off WITHOUT SHA-256 verifying it (fail-OPEN); an interrupted first download
# could also leave a truncated %BIN%. The fix must mirror mine-auto.bat: download
# to a TEMP, look the exe's digest up in the release SHA256SUMS (LF-safe PowerShell
# extraction — NOT findstr /e, which mis-handles the LF-only file), verify with
# PowerShell Get-FileHash, and only move-into-place on a match — else discard +
# abort, never run a truncated/tampered %BIN%.
#
# HOW (mirrors tests/launcher_selfupdate_bat.sh): EXTRACT the real bootstrap
# verify block out of install-csd-miner.bat (between the BOOTSTRAP-VERIFY BEGIN/END
# markers), wrap it byte-for-byte in a tiny CRLF harness that sets only the vars
# the block reads (DIR / EXE / BIN / BASE_URL), points BASE_URL at a LOCAL
# file:// fake-release dir, runs it in real cmd.exe, and asserts on %BIN% +
# the block's BOOTSTRAP_OK result. We test SHIPPING bytes.
#
# PROVES:
#   GOOD      (exe matches SHA256SUMS)             -> %BIN% placed, OK=1
#   TAMPER    (served exe != SHA256SUMS digest)    -> %BIN% ABSENT, OK=0 (rejected)
#   TRUNC     (short/interrupted download)         -> %BIN% ABSENT, OK=0
#   NOSUMS    (no SHA256SUMS)                       -> %BIN% ABSENT, OK=0 (fail-closed)
#   NOTLISTED (exe not listed in SHA256SUMS)        -> %BIN% ABSENT, OK=0
#
# Run:   bash tests/installer_bootstrap_verify_bat.sh
# Exit:  0 = all pass, non-zero = at least one failure printed to stderr
# Requires: cmd.exe, cygpath, sha256sum, powershell (Get-FileHash). Skips on non-Windows.

set -uo pipefail

REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
BAT="$REPO_ROOT/install-csd-miner.bat"

PASS=0
FAIL=0
ok()   { echo "  [PASS] $1"; PASS=$((PASS + 1)); }
fail() { echo "  [FAIL] $1" >&2; echo "         $2" >&2; FAIL=$((FAIL + 1)); }

echo
echo "=== install-csd-miner.bat bootstrap verify — REAL cmd.exe integration ==="
echo

CMD_EXE=""
for c in "${COMSPEC:-}" "/c/Windows/System32/cmd.exe" "$(command -v cmd.exe 2>/dev/null)"; do
  if [ -n "${c:-}" ] && [ -x "$c" ]; then CMD_EXE="$c"; break; fi
done
if [ -z "$CMD_EXE" ] || ! command -v cygpath >/dev/null 2>&1; then
  echo "  [SKIP] cmd.exe and/or cygpath not found — this integration test requires Windows." >&2
  exit 0
fi

run_bat() { "$CMD_EXE" //c "$(cygpath -w "$1")" </dev/null >/dev/null 2>&1 || true; }
sha_of_file() { sha256sum "$1" 2>/dev/null | awk '{print $1}'; }
# file:// URL with forward-slash Windows path (curl opens these on this box).
file_url() { local p; p="$(cygpath -w "$1")"; printf 'file:///%s' "$(printf '%s' "$p" | sed 's#\\#/#g')"; }

SANDBOX="$REPO_ROOT/tests/.itb-$$"
rm -rf "$SANDBOX"; mkdir -p "$SANDBOX"
trap 'rm -rf "$SANDBOX"' EXIT

# ── Extract the real bootstrap-verify block from install-csd-miner.bat ────────
B_START="$(grep -n 'BOOTSTRAP-VERIFY BEGIN' "$BAT" | head -1 | cut -d: -f1)"
B_END="$(grep -n 'BOOTSTRAP-VERIFY END' "$BAT" | head -1 | cut -d: -f1)"
if [ -z "$B_START" ] || [ -z "$B_END" ] || [ "$B_START" -ge "$B_END" ]; then
  fail "extract bootstrap block" "could not bracket BOOTSTRAP-VERIFY BEGIN@${B_START:-?}..END@${B_END:-?} in install-csd-miner.bat (markers missing = block not implemented)"
  echo "  Passed: $PASS  Failed: $FAIL"; exit 1
fi
BLOCK_FILE="$SANDBOX/bootstrap.block"
# Keep the source CRLF (sed preserves \r); the extracted block stays CRLF.
sed -n "${B_START},${B_END}p" "$BAT" > "$BLOCK_FILE"

# Build a harness .bat embedding the real block for ONE scenario.
make_harness() {
  local out="$1" dir="$2" exe="$3" base="$4"
  {
    printf '@echo off\r\n'
    printf 'setlocal EnableExtensions EnableDelayedExpansion\r\n'
    printf 'set "DIR=%s"\r\n' "$dir"
    printf 'set "EXE=%s"\r\n' "$exe"
    printf 'set "BIN=%s\\%s"\r\n' "$dir" "$exe"
    printf 'set "BASE_URL=%s"\r\n' "$base"
    printf 'set "BOOTSTRAP_OK=1"\r\n'
    cat "$BLOCK_FILE"
    printf '\r\n'
    printf '>"%%~f0.result" echo OK=!BOOTSTRAP_OK!\r\n'
    printf 'exit /b 0\r\n'
  } > "$out"
}

EXE="csd-pool-miner-cpu.exe"

# Stage a fake-release dir for a scenario; returns the file:// base in REL_BASE.
stage_release() {
  local mode="$1" rel="$2"
  rm -rf "$rel"; mkdir -p "$rel"
  printf 'MZ-FAKE-MINER-EXE-%s\r\n' "$mode" > "$rel/$EXE"
  local good; good="$(sha_of_file "$rel/$EXE")"
  local sums="$rel/SHA256SUMS"
  case "$mode" in
    GOOD)      printf '%s  %s\n' "$good" "$EXE" > "$sums" ;;
    TAMPER)    printf '%s  %s\n' "$good" "$EXE" > "$sums"; printf 'MZ-TAMPERED-PAYLOAD\r\n' > "$rel/$EXE" ;;
    TRUNC)     printf '%s  %s\n' "$good" "$EXE" > "$sums"; printf 'MZ' > "$rel/$EXE" ;;
    NOSUMS)    : ;;
    NOTLISTED) printf '%s  other-asset.exe\n' "$good" > "$sums" ;;
  esac
  GOOD_SHA="$good"
  REL_BASE="$(file_url "$rel")"
}

run_scenario() {
  local tag="$1" mode="$2"
  local work="$SANDBOX/$tag"; mkdir -p "$work"
  local dir="$work/dir"; mkdir -p "$dir"
  local rel="$work/fake_release"
  stage_release "$mode" "$rel"
  local hbat="$work/h.bat"
  make_harness "$hbat" "$(cygpath -w "$dir")" "$EXE" "$REL_BASE"
  run_bat "$hbat"
  BIN_PATH="$dir/$EXE"
  RESULT="$(sed 's/\r//' "$hbat.result" 2>/dev/null)"
  WORK="$work"
}

# ── GOOD ─────────────────────────────────────────────────────────────────────
run_scenario good GOOD
if [ -f "$BIN_PATH" ] && [ "$(sha_of_file "$BIN_PATH")" = "$GOOD_SHA" ] && [ "$RESULT" = "OK=1" ]; then
  ok "GOOD: verified bootstrap exe placed at %BIN% (SHA matches SHA256SUMS, OK=1)"
else
  fail "GOOD place" "BIN present=$( [ -f "$BIN_PATH" ] && echo y||echo n) sha=$(sha_of_file "$BIN_PATH") want=$GOOD_SHA result=$RESULT"
fi

# ── TAMPER ───────────────────────────────────────────────────────────────────
run_scenario tamper TAMPER
if [ ! -e "$BIN_PATH" ] && [ "$RESULT" = "OK=0" ]; then
  ok "TAMPER: wrong-SHA exe REJECTED — %BIN% never placed, OK=0 (fail-closed)"
else
  fail "TAMPER reject" "BIN=$( [ -e "$BIN_PATH" ] && echo PRESENT-sha=$(sha_of_file "$BIN_PATH")||echo absent) result=$RESULT — fail-OPEN if present/OK=1"
fi

# ── TRUNC ────────────────────────────────────────────────────────────────────
run_scenario trunc TRUNC
if [ ! -e "$BIN_PATH" ] && [ "$RESULT" = "OK=0" ]; then
  ok "TRUNC: truncated download REJECTED — %BIN% never placed, OK=0 (no partial exe left)"
else
  fail "TRUNC reject" "BIN=$( [ -e "$BIN_PATH" ] && echo PRESENT||echo absent) result=$RESULT"
fi

# ── NOSUMS ───────────────────────────────────────────────────────────────────
run_scenario nosums NOSUMS
if [ ! -e "$BIN_PATH" ] && [ "$RESULT" = "OK=0" ]; then
  ok "NO SHA256SUMS: REFUSED — %BIN% absent, OK=0 (fail-closed)"
else
  fail "NOSUMS reject" "BIN=$( [ -e "$BIN_PATH" ] && echo PRESENT||echo absent) result=$RESULT"
fi

# ── NOTLISTED ────────────────────────────────────────────────────────────────
run_scenario notlisted NOTLISTED
if [ ! -e "$BIN_PATH" ] && [ "$RESULT" = "OK=0" ]; then
  ok "ASSET NOT LISTED in SHA256SUMS: REFUSED — %BIN% absent, OK=0"
else
  fail "NOTLISTED reject" "BIN=$( [ -e "$BIN_PATH" ] && echo PRESENT||echo absent) result=$RESULT"
fi

echo
echo "========================================"
echo "  Passed: $PASS  Failed: $FAIL"
echo "========================================"
echo
[ "$FAIL" -gt 0 ] && exit 1
exit 0
