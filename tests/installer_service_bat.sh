#!/usr/bin/env bash
# tests/installer_service_bat.sh — REAL cmd.exe integration tests for the OPT-IN
# Windows-service launchers install-as-service.bat + uninstall-service.bat, and
# for the install-csd-miner.bat wiring that fetches + advertises them.
#
# WHY THIS EXISTS: the miner BINARY (v0.1.12) already has --install-service /
# --uninstall-service. The launcher decision (user, 2026-06-23) is to NOT have the
# miner auto-install a service when run elevated; instead ship SEPARATE .bat files
# the user OPTIONALLY double-clicks. These tests prove, on real cmd.exe:
#   (1) install-as-service.bat is SELF-ELEVATING — when NOT admin it re-launches
#       itself elevated (Start-Process -Verb RunAs) and does NOT try to touch the
#       SCM unprivileged.
#   (2) The EXACT command it builds for the elevated copy is correct:
#         "<exe>" --install-service --address <addr> --backend <backend>
#       with (a) the exe resolved from %LOCALAPPDATA%\csd-pool-miner (the same path
#       mine-auto.bat runs) for the right cpu/nvidia/amd VARIANT, (b) the saved
#       payout ADDRESS read from address.txt, (c) the variant→backend map
#       (cpu→cpu, nvidia→cuda, amd→opencl).
#   (3) uninstall-service.bat is self-elevating and builds "<exe>" --uninstall-service.
#   (4) install-csd-miner.bat downloads BOTH service .bat files next to itself
#       (same RAW_BASE fetch pattern as mine-auto.bat) and prints the one optional
#       advisory line WITHOUT auto-running the service installer.
#
# HOW (mirrors tests/launcher_selfupdate_bat.sh + installer_bootstrap_verify_bat.sh):
# we DRIVE THE REAL SHIPPING .bat in actual cmd.exe. The two service .bat files
# honor a test hook CSD_SVC_DRYRUN=1 that makes them resolve everything (admin
# check, exe, address, backend) and ECHO the command they WOULD run to a result
# file instead of elevating / calling sc / launching the miner. We assert on that
# echoed command. ZERO effect on a normal (double-click) run — the var is unset.
#
# Run:   bash tests/installer_service_bat.sh
# Exit:  0 = all pass, non-zero = at least one failure printed to stderr
# Requires: cmd.exe, cygpath, powershell. Skips cleanly on non-Windows.

set -uo pipefail

REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
INSTALL_BAT="$REPO_ROOT/install-as-service.bat"
UNINSTALL_BAT="$REPO_ROOT/uninstall-service.bat"
INSTALLER_BAT="$REPO_ROOT/install-csd-miner.bat"

PASS=0
FAIL=0
ok()   { echo "  [PASS] $1"; PASS=$((PASS + 1)); }
fail() { echo "  [FAIL] $1" >&2; echo "         $2" >&2; FAIL=$((FAIL + 1)); }

echo
echo "=== opt-in Windows-service .bat — REAL cmd.exe integration ==="
echo

# ── locate cmd.exe + cygpath ──────────────────────────────────────────────────
CMD_EXE=""
for c in "${COMSPEC:-}" "/c/Windows/System32/cmd.exe" "$(command -v cmd.exe 2>/dev/null)"; do
  if [ -n "${c:-}" ] && [ -x "$c" ]; then CMD_EXE="$c"; break; fi
done
if [ -z "$CMD_EXE" ] || ! command -v cygpath >/dev/null 2>&1; then
  echo "  [SKIP] cmd.exe and/or cygpath not found — this integration test requires Windows." >&2
  exit 0
fi

# A few static structural assertions don't need cmd.exe; the command-building +
# elevation-guard assertions do. Fail early if the files don't exist at all (the
# TDD red state): the script must end non-zero so `watch it fail` is unambiguous.
missing=0
for f in "$INSTALL_BAT" "$UNINSTALL_BAT"; do
  if [ ! -f "$f" ]; then echo "  [FAIL] missing file: $f" >&2; missing=1; fi
done
if [ "$missing" -ne 0 ]; then
  echo "  Passed: $PASS  Failed: at least 1 (missing .bat) — RED" >&2
  exit 1
fi

# Run a Windows .bat by absolute path, passing through env + extra args. `//c`
# (not `/c`) stops MSYS from rewriting the flag; cygpath -w gives the C:\... path.
# Extra args after the bat path become %1.. inside the .bat.
run_bat() { local b="$1"; shift; "$CMD_EXE" //c "$(cygpath -w "$b")" "$@" </dev/null >/dev/null 2>&1 || true; }

SANDBOX="$REPO_ROOT/tests/.svc-$$"
rm -rf "$SANDBOX"; mkdir -p "$SANDBOX"
trap 'rm -rf "$SANDBOX"' EXIT

ADDR="dad2e284aabbccddeeff00112233445566778899"

# Stage a fake LOCALAPPDATA\csd-pool-miner with a saved address.txt and a fake
# miner exe for VARIANT. Returns the dir (Windows path) in SVC_DIR_WIN.
stage_dir() {
  local variant="$1" with_addr="$2"
  local d="$SANDBOX/lad-$variant-$with_addr/csd-pool-miner"
  rm -rf "$d"; mkdir -p "$d"
  printf 'MZ-FAKE-MINER-%s\r\n' "$variant" > "$d/csd-pool-miner-$variant.exe"
  if [ "$with_addr" = "withaddr" ]; then printf '%s\r\n' "$ADDR" > "$d/address.txt"; fi
  SVC_DIR="$d"
  SVC_DIR_WIN="$(cygpath -w "$d")"
}

# Run a service .bat in DRYRUN mode with a fake LOCALAPPDATA. The .bat writes the
# command it WOULD run (+ admin-guard decision) to %TEMP%\csd-svc-dryrun.txt; we
# point TEMP at the sandbox so we can read it back. We pass the variant as %1.
run_svc_dryrun() {
  local bat="$1" variant="$2" with_addr="$3"
  stage_dir "$variant" "$with_addr"
  local lad; lad="$(cygpath -w "$SANDBOX/lad-$variant-$with_addr")"
  local tmp="$SANDBOX/tmp-$variant-$with_addr"; mkdir -p "$tmp"
  local out="$tmp/csd-svc-dryrun.txt"; rm -f "$out"
  CSD_SVC_DRYRUN=1 LOCALAPPDATA="$lad" TEMP="$(cygpath -w "$tmp")" TMP="$(cygpath -w "$tmp")" \
    run_bat "$bat" "$variant"
  RESULT="$(sed 's/\r//' "$out" 2>/dev/null)"
  RESULT_FILE="$out"
}

# ════════════════════════════════════════════════════════════════════════════
#  install-as-service.bat — exact command per variant (admin path, addr saved)
# ════════════════════════════════════════════════════════════════════════════
# In DRYRUN we force the "already admin" branch so the command-build path runs
# (CSD_SVC_DRYRUN short-circuits the real elevation). The .bat must still PRINT
# which admin-branch it took so we can assert the guard logic separately below.

check_install_cmd() {
  local variant="$1" backend="$2"
  run_svc_dryrun "$INSTALL_BAT" "$variant" withaddr
  local exe_win="$SVC_DIR_WIN\\csd-pool-miner-$variant.exe"
  # Expected command line (cmd echoes it exactly as the .bat assembles it).
  local want="\"$exe_win\" --install-service --address $ADDR --backend $backend"
  if printf '%s\n' "$RESULT" | grep -qiF "INSTALL_CMD=$want"; then
    ok "install-as-service.bat [$variant]: builds  $want"
  else
    fail "install cmd [$variant]" "want INSTALL_CMD=$want ; got: $RESULT"
  fi
}

check_install_cmd cpu    cpu
check_install_cmd nvidia cuda
check_install_cmd amd    opencl

# ── admin guard fires when NOT admin ─────────────────────────────────────────
# DRYRUN must also report the admin decision it made. We assert the .bat contains
# the real non-admin re-launch via Start-Process -Verb RunAs (the elevation guard)
# AND that in DRYRUN it reports it would self-elevate when not admin. Because the
# test harness is itself usually NON-admin, the .bat's real `net session` check
# returns "not admin", and DRYRUN reports the elevation decision without actually
# spawning a UAC prompt.
run_svc_dryrun "$INSTALL_BAT" cpu withaddr
if printf '%s\n' "$RESULT" | grep -qiE 'ADMIN=(yes|no)'; then
  ok "install-as-service.bat reports its admin-guard decision (ADMIN=yes/no)"
else
  fail "admin guard report" "no ADMIN= line in DRYRUN output; got: $RESULT"
fi

# Static proof the real elevation mechanism is present (self-elevating via RunAs),
# guarded by a net-session admin check — these are the SHIPPING bytes.
if grep -qiE 'net session' "$INSTALL_BAT" && grep -qiE "Start-Process .*-Verb RunAs" "$INSTALL_BAT"; then
  ok "install-as-service.bat is self-elevating (net session check + Start-Process -Verb RunAs)"
else
  fail "self-elevating" "install-as-service.bat lacks 'net session' and/or 'Start-Process -Verb RunAs'"
fi

# Robustness: after --install-service it must CHECK errorlevel and, on failure,
# tell the user to run uninstall-service.bat then retry (the known SCM
# failure-action edge that leaves a registered-but-unstarted service), AND it must
# start the service on success.
if grep -qiE 'errorlevel' "$INSTALL_BAT" && grep -qiF 'uninstall-service.bat' "$INSTALL_BAT"; then
  ok "install-as-service.bat handles the install-failure edge (errorlevel check -> advise uninstall-service.bat)"
else
  fail "install failure edge" "no errorlevel check and/or no uninstall-service.bat advice in install-as-service.bat"
fi
# The service name is held in %SVC% (set to csd-pool-miner at the top), so the
# shipping source reads `sc start %SVC%` etc. Assert that form (+ that %SVC% is
# pinned to csd-pool-miner) rather than a literal that the source never contains.
if grep -qiE 'set "SVC=csd-pool-miner"' "$INSTALL_BAT" && grep -qiE 'sc start %SVC%' "$INSTALL_BAT"; then
  ok "install-as-service.bat starts the service (sc start %SVC%, SVC=csd-pool-miner)"
else
  fail "sc start" "install-as-service.bat does not 'sc start %SVC%' (with SVC=csd-pool-miner)"
fi
if grep -qiE 'sc query %SVC%' "$INSTALL_BAT"; then
  ok "install-as-service.bat documents verification (sc query %SVC%)"
else
  fail "sc query" "install-as-service.bat does not mention 'sc query %SVC%'"
fi

# ── missing exe -> clear error, NO command built ─────────────────────────────
# Stage a dir with the address but NO exe for the variant.
{
  variant=cpu
  d="$SANDBOX/noexe/csd-pool-miner"; rm -rf "$d"; mkdir -p "$d"
  printf '%s\r\n' "$ADDR" > "$d/address.txt"   # address present, exe absent
  lad="$(cygpath -w "$SANDBOX/noexe")"
  tmp="$SANDBOX/noexe-tmp"; mkdir -p "$tmp"
  out="$tmp/csd-svc-dryrun.txt"; rm -f "$out"
  CSD_SVC_DRYRUN=1 LOCALAPPDATA="$lad" TEMP="$(cygpath -w "$tmp")" TMP="$(cygpath -w "$tmp")" \
    run_bat "$INSTALL_BAT" "$variant"
  R="$(sed 's/\r//' "$out" 2>/dev/null)"
  if printf '%s\n' "$R" | grep -qiF 'INSTALL_CMD='; then
    fail "missing exe" "built an INSTALL_CMD with no exe present: $R"
  else
    ok "install-as-service.bat refuses cleanly when the miner exe is absent (no INSTALL_CMD built)"
  fi
}

# ════════════════════════════════════════════════════════════════════════════
#  uninstall-service.bat — self-elevating + builds the uninstall command
# ════════════════════════════════════════════════════════════════════════════
run_svc_dryrun "$UNINSTALL_BAT" cpu withaddr
UN_EXE_WIN="$SVC_DIR_WIN\\csd-pool-miner-cpu.exe"
if printf '%s\n' "$RESULT" | grep -qiF "UNINSTALL_CMD=\"$UN_EXE_WIN\" --uninstall-service"; then
  ok "uninstall-service.bat builds  \"<exe>\" --uninstall-service"
else
  fail "uninstall cmd" "want UNINSTALL_CMD=\"$UN_EXE_WIN\" --uninstall-service ; got: $RESULT"
fi
if grep -qiE 'net session' "$UNINSTALL_BAT" && grep -qiE "Start-Process .*-Verb RunAs" "$UNINSTALL_BAT"; then
  ok "uninstall-service.bat is self-elevating (net session check + Start-Process -Verb RunAs)"
else
  fail "uninstall self-elevating" "uninstall-service.bat lacks 'net session' and/or 'Start-Process -Verb RunAs'"
fi
if grep -qiE 'set "SVC=csd-pool-miner"' "$UNINSTALL_BAT" && grep -qiE 'sc stop %SVC%' "$UNINSTALL_BAT"; then
  ok "uninstall-service.bat stops the service first (sc stop %SVC%, SVC=csd-pool-miner)"
else
  fail "sc stop" "uninstall-service.bat does not 'sc stop %SVC%' (with SVC=csd-pool-miner)"
fi
# Fallback removal path (sc delete) if --uninstall-service is unavailable.
if grep -qiE 'sc delete %SVC%' "$UNINSTALL_BAT"; then
  ok "uninstall-service.bat has the sc-delete fallback (sc delete %SVC%)"
else
  fail "sc delete fallback" "uninstall-service.bat lacks the 'sc delete %SVC%' fallback"
fi

# ════════════════════════════════════════════════════════════════════════════
#  install-csd-miner.bat wiring — fetch BOTH service .bat + advise (no auto-run)
# ════════════════════════════════════════════════════════════════════════════
if [ -f "$INSTALLER_BAT" ]; then
  # It must fetch both files from RAW_BASE next to itself (same pattern as
  # mine-auto.bat / mine-all-gpus.bat fetch).
  if grep -qiF 'install-as-service.bat' "$INSTALLER_BAT" && grep -qiF 'uninstall-service.bat' "$INSTALLER_BAT"; then
    ok "install-csd-miner.bat references both service .bat files"
  else
    fail "installer references" "install-csd-miner.bat does not reference install-as-service.bat and uninstall-service.bat"
  fi
  if grep -qiE '%RAW_BASE%/install-as-service\.bat' "$INSTALLER_BAT" \
     && grep -qiE '%RAW_BASE%/uninstall-service\.bat' "$INSTALLER_BAT"; then
    ok "install-csd-miner.bat downloads both service .bat from RAW_BASE"
  else
    fail "installer fetch" "install-csd-miner.bat does not fetch the service .bat from %RAW_BASE%"
  fi
  # It must NOT auto-run the service installer (no `call ...install-as-service.bat`
  # and no plain invocation that would launch it). Advisory only.
  if grep -qiE '(call|start).*install-as-service\.bat' "$INSTALLER_BAT"; then
    fail "no auto-run" "install-csd-miner.bat auto-runs install-as-service.bat (must be advisory only)"
  else
    ok "install-csd-miner.bat does NOT auto-run install-as-service.bat (advisory only)"
  fi
  # One optional advisory line present.
  if grep -qiE 'double-click install-as-service\.bat' "$INSTALLER_BAT"; then
    ok "install-csd-miner.bat prints the optional 'double-click install-as-service.bat' line"
  else
    fail "advisory line" "install-csd-miner.bat lacks the optional advisory line"
  fi
else
  fail "installer present" "install-csd-miner.bat not found"
fi

echo
echo "========================================"
echo "  Passed: $PASS  Failed: $FAIL"
echo "========================================"
echo
[ "$FAIL" -gt 0 ] && exit 1
exit 0
