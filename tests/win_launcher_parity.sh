#!/usr/bin/env bash
# tests/win_launcher_parity.sh — Windows .bat launchers must match the Linux/HiveOS
# fix: (1) the no-arg build variant is AUTO-DETECTED (the same Win32_VideoController
# WMI probe install-csd-miner.bat uses), NOT a hard `set "VARIANT=amd"` default that
# ran the amd build on NVIDIA rigs launched with no arg; and (2) the payout address
# is normalised + 40-hex-validated BEFORE it is written to the on-disk config (%CFG%),
# so a bad/quoted address can never reach the miner.
#
# Static analysis only (these are Windows CMD scripts; we don't execute them here).
# Mirrors the spirit of tests/detect_variant.sh's FIX-3 grep guard for the .sh side.
#
# Run:  bash tests/win_launcher_parity.sh
# Exit: 0 = all pass

set -uo pipefail
ROOT="$(cd "$(dirname "${BASH_SOURCE[0]:-$0}")/.." && pwd)"
PASS=0; FAIL=0
ok()   { echo "  [PASS] $1"; PASS=$((PASS + 1)); }
fail() { echo "  [FAIL] $1" >&2; echo "         $2" >&2; FAIL=$((FAIL + 1)); }

echo
echo "=== win_launcher_parity: .bat launchers auto-detect variant + validate address ==="
echo

for BAT in mine-auto.bat mine-all-gpus.bat; do
  F="$ROOT/$BAT"
  echo "-- $BAT --"
  [ -f "$F" ] || { fail "$BAT exists" "file not found: $F"; continue; }

  # (1a) The bare hard-coded `set "VARIANT=amd"` default must be GONE.
  if grep -Eq 'set[[:space:]]+"VARIANT=amd"' "$F"; then
    fail "$BAT no hard amd default" "still has set \"VARIANT=amd\": $(grep -nE 'set[[:space:]]+"VARIANT=amd"' "$F" | head -1)"
  else
    ok "$BAT: no hard-coded \"VARIANT=amd\" default"
  fi

  # (1b) The no-arg default must SET VARIANT from a Win32_VideoController WMI probe
  # (auto-detect). Match the specific for/f line that pipes the probe into
  # `set "VARIANT=%%i"` — NOT a bare mention of Win32_VideoController (the GPU-COUNT
  # probe that fills NGPU already names it, so a substring match would false-pass).
  if grep -Eq 'Win32_VideoController.*do[[:space:]]+set[[:space:]]+"VARIANT=%%i"' "$F"; then
    ok "$BAT: no-arg variant default sets VARIANT from a Win32_VideoController probe"
  else
    fail "$BAT WMI probe" "no 'Win32_VideoController ... set \"VARIANT=%%i\"' auto-detect found for the variant default"
  fi

  # (2) Address must be validated against 40-hex (^[0-9a-f]{40}$) BEFORE the %CFG%
  # write. Assert the regex is present AND that its line precedes the line that
  # redirects to "%CFG%" with `echo !ADDR!` (the on-disk write of the address).
  hexline="$(grep -n '\[0-9a-f\]{40}' "$F" | head -1 | cut -d: -f1)"
  # The write-of-the-address line: redirect to %CFG% echoing the (now-validated) ADDR.
  writeline="$(grep -nE '>[[:space:]]*"%CFG%"[[:space:]]+echo[[:space:]]+!ADDR!' "$F" | tail -1 | cut -d: -f1)"
  if [ -z "$hexline" ]; then
    fail "$BAT validates 40-hex" "no ^[0-9a-f]{40}\$ address validation found"
  elif [ -z "$writeline" ]; then
    fail "$BAT writes validated ADDR to %CFG%" "no '> \"%CFG%\" echo !ADDR!' write found"
  elif [ "$hexline" -lt "$writeline" ]; then
    ok "$BAT: 40-hex validation (line $hexline) precedes the %CFG% address write (line $writeline)"
  else
    fail "$BAT validate-before-write" "40-hex check (line $hexline) does NOT precede %CFG% write (line $writeline)"
  fi
done

echo
echo "========================================"
echo "  Passed: $PASS  Failed: $FAIL"
echo "========================================"
echo
[ "$FAIL" -gt 0 ] && exit 1
exit 0
