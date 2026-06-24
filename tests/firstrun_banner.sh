#!/usr/bin/env bash
# tests/firstrun_banner.sh — FIX 5 (v0.1.15): first-run banner.
#
# THE GAP: on start, the launcher told you the GPU count and the address, but NOT
# which BUILD (nvidia/amd/cpu) it actually selected, nor WHERE the per-GPU logs
# land. A rig auto-detected onto the wrong build, or quietly logging a crash, gave
# the operator no anchor. FIX 5 prints, near "Rig has N GPU(s)", the selected build
# and the per-GPU log path so the user knows what is running and where to look.
#
# HERMETIC: static structural check — the banner must reference the $VARIANT build
# and a gpu*-log path, in both mine-auto.sh and mine-auto.bat. (The full first-run
# path needs an address prompt + GPU probe; the banner content is what we lock.)
#
# Run:  bash tests/firstrun_banner.sh
# Exit: 0 = all pass

set -uo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]:-$0}")/.." && pwd)"
SH="$ROOT/mine-auto.sh"
BAT="$ROOT/mine-auto.bat"
PASS=0; FAIL=0
ok()   { echo "  [PASS] $1"; PASS=$((PASS + 1)); }
fail() { echo "  [FAIL] $1" >&2; echo "         $2" >&2; FAIL=$((FAIL + 1)); }

echo
echo "=== first-run banner: selected build + log path (FIX 5) ==="
echo

# A line in mine-auto.sh that echoes BOTH the build variant and a gpu*-log path.
if grep -Eq 'echo "[^"]*[Bb]uild[^"]*\$VARIANT' "$SH" && grep -Eq 'echo "[^"]*gpu[^"]*-log' "$SH"; then
  ok "mine-auto.sh banner names the selected build (\$VARIANT) and the per-GPU log path"
else
  fail "mine-auto.sh banner" "no banner echo referencing both \$VARIANT and a gpu*-log path"
fi

# mine-auto.bat: same idea with %VARIANT% and a gpu*-log path.
if grep -Eqi 'echo .*build.*%VARIANT%' "$BAT" && grep -Eqi 'echo .*gpu.*-log' "$BAT"; then
  ok "mine-auto.bat banner names the selected build (%VARIANT%) and the per-GPU log path"
else
  fail "mine-auto.bat banner" "no banner echo referencing both %VARIANT% and a gpu*-log path"
fi

echo
echo "========================================"
echo "  Passed: $PASS  Failed: $FAIL"
echo "========================================"
echo
[ "$FAIL" -gt 0 ] && exit 1
exit 0
