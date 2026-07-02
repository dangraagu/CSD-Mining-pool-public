#!/usr/bin/env bash
# tests/launcher_selfupdate.sh — deterministic, hermetic tests for the launcher
# SELF-UPDATE added in v0.1.11 (mine-auto.sh `update_launcher_self`).
#
# The bug being fixed: download_verify_swap swaps only the miner BINARY, so a fix
# to the launcher script itself never reached a rig that only runs the on-disk
# launcher. v0.1.11 adds update_launcher_self, which must be FAIL-CLOSED (any
# download/verify failure keeps the OLD launcher untouched) and NO-BRICK (atomic
# on-disk replace, NEVER a mid-run re-exec).
#
# These tests run with NO network and NO real release: we source mine-auto.sh
# with CSD_SOURCE_ONLY=1 (so the prompt/GPU-probe/mining-loop don't run), then
# override the `download` primitive to serve a LOCAL fake "release" directory.
# Because expected_sha() fetches SHA256SUMS via download() too, that single
# override controls both the launcher fetch and the checksum lookup. We force the
# OS-sha256sum verifier path (BIN points at a non-verify-file stub), exercising
# the same trusted-verifier discipline the binary path uses.
#
# Asserts:
#   A happy path        — good SHA → on-disk launcher REPLACED atomically, .bak kept
#   B fail-closed bad   — wrong SHA in SHA256SUMS → OLD launcher byte-identical, no swap
#   C fail-closed miss  — SELF_NAME not listed in SHA256SUMS → OLD launcher kept
#   D fail-closed dlerr — launcher download fails → OLD launcher kept
#   E skip-if-same      — on-disk launcher already matches → rc=2 (verified, no
#                         change — mirrors h-run's ua_download_verify_swap), NO
#                         rewrite, NO .bak churn
#   F no-brick / no-exec— update_launcher_self body contains no `exec` (static
#                         guard). v0.2.0: the swap fn STAYS exec-free by design;
#                         the re-exec lives in the SEPARATE, gated
#                         reexec_new_launcher (tested below) — reviewers: this
#                         split is intentional, re-affirm it, don't fold them.
#   G atomic swap       — implementation uses mv (rename), not in-place truncate+write
#   R rc contract       — a REAL byte change returns rc=0 AND persists the
#                         verified digest (LAUNCHER_SWAPPED_SHA) for the re-exec
#
# v0.2.0 staged-handoff re-exec (reexec_new_launcher — applies a swapped
# launcher WITHOUT waiting for a manual restart; every guard fails SAFE to
# "keep running the old in-memory launcher + keep mining" and drops a crumb):
#   S1 refuses when SELF_PATH is not a real readable file (curl|bash piped run)
#   S2 refuses when the on-disk SHA no longer matches the swap-time digest
#   T  refuses (and restores .bak) when the new launcher fails `bash -n`
#   U  refuses at the CSD_REEXEC_GEN generation cap (never a re-exec loop)
#   V  execfail fall-through: a failed exec RESTARTS the miners and refuses
#   W  happy path: execs the new launcher with the ORIGINAL argv, generation 1
#   X  wiring: do_update_check calls reexec_new_launcher ONLY on rc=0 (a real
#      byte change), never on rc=2/1 — and mine-auto.bat keeps its trampoline
#      (parity: BOTH launchers apply a staged launcher without operator action)
#
# Run:   bash tests/launcher_selfupdate.sh
# Exit:  0 = all pass, non-zero = at least one failure printed to stderr

set -euo pipefail

REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
LAUNCHER="$REPO_ROOT/mine-auto.sh"

PASS=0
FAIL=0
ok()   { echo "  [PASS] $1"; PASS=$((PASS + 1)); }
fail() { echo "  [FAIL] $1" >&2; echo "         $2" >&2; FAIL=$((FAIL + 1)); }

# Byte-exact file-content comparison (newline-safe: command substitution strips
# trailing newlines, so we compare sha256 of the file vs sha256 of a literal).
sha_of_file()    { sha256sum "$1" 2>/dev/null | awk '{print $1}'; }
sha_of_string()  { printf '%s' "$1" | sha256sum | awk '{print $1}'; }
file_is_string() { [ "$(sha_of_file "$1")" = "$(sha_of_string "$2")" ]; }

echo
echo "=== launcher self-update tests (v0.1.11 — fail-closed + no-brick) ==="
echo

# A scratch sandbox per run; cleaned on exit.
SANDBOX="$(mktemp -d)"
trap 'rm -rf "$SANDBOX"' EXIT

# fake_release/ holds the bytes a real release would serve at
# releases/latest/download/<name>:  mine-auto.sh  and  SHA256SUMS.
FAKE_REL="$SANDBOX/fake_release"
mkdir -p "$FAKE_REL"

# A stub "$BIN": present + executable but does NOT understand `verify-file`
# (its --help exits non-zero), so update_launcher_self falls through to the OS
# sha256sum verifier — the path most rigs without a verify-file binary take.
BIN_STUB="$SANDBOX/bin-stub"
printf '#!/usr/bin/env bash\nexit 2\n' > "$BIN_STUB"
chmod +x "$BIN_STUB"

# Build a self-contained driver script for ONE scenario and run it in a fresh
# bash. Args:
#   $1 scenario tag (for messages)
#   $2 on-disk launcher initial content
#   $3 candidate (download) launcher content   (ignored if download must fail)
#   $4 sha256sums mode: GOOD | BADSHA | MISSING | DLFAIL
# Echoes a result block the caller greps. Keeping each scenario in its own bash
# avoids any global-state bleed between cases.
run_case() {
  local tag="$1" disk_content="$2" cand_content="$3" mode="$4"
  local work="$SANDBOX/case-$tag"
  mkdir -p "$work"
  local on_disk="$work/mine-auto.sh"
  printf '%s' "$disk_content" > "$on_disk"
  printf '%s' "$cand_content" > "$work/candidate"

  # Compute the candidate's real sha for GOOD; a deliberately wrong one for BADSHA.
  local cand_sha good_sha bad_sha
  good_sha="$(sha256sum "$work/candidate" | awk '{print $1}')"
  bad_sha="0000000000000000000000000000000000000000000000000000000000000000"

  # Stage the fake-release SHA256SUMS for this case.
  local sums="$work/SHA256SUMS"
  case "$mode" in
    GOOD)    printf '%s  mine-auto.sh\n' "$good_sha" > "$sums" ;;
    BADSHA)  printf '%s  mine-auto.sh\n' "$bad_sha"  > "$sums" ;;
    MISSING) printf '%s  some-other-asset\n' "$good_sha" > "$sums" ;;  # no mine-auto.sh line
    DLFAIL)  printf '%s  mine-auto.sh\n' "$good_sha" > "$sums" ;;       # sums fine; the .sh download fails
  esac

  CSD_SOURCE_ONLY=1 \
  CASE_DIR="$work" CASE_MODE="$mode" \
  bash -c '
    set -uo pipefail
    # Source the REAL launcher to get the REAL update_launcher_self + helpers.
    source "'"$LAUNCHER"'" >/dev/null 2>&1

    # Point self-update at our sandbox.
    SELF_PATH="'"$on_disk"'"
    BIN="'"$BIN_STUB"'"
    DATA_DIR="'"$work"'"          # expected_sha writes SHA256SUMS.tmp here

    # Override the download primitive to serve the LOCAL fake release.
    #   .../mine-auto.sh  -> copy candidate   (or FAIL if CASE_MODE=DLFAIL)
    #   .../SHA256SUMS    -> copy our staged sums
    download() {
      local url="$1" out="$2"
      case "$url" in
        *"/mine-auto.sh")
          if [ "$CASE_MODE" = "DLFAIL" ]; then return 1; fi
          cp "$CASE_DIR/candidate" "$out" ;;
        *"/SHA256SUMS")
          cp "$CASE_DIR/SHA256SUMS" "$out" ;;
        *) return 1 ;;
      esac
    }

    # NB: sourcing mine-auto.sh inherits its `set -e`. Guard the call so a
    # fail-closed return (1) does not abort before we print RC.
    rc=0
    update_launcher_self || rc=$?
    echo "RC=$rc"
    echo "SWAPPED_SHA=${LAUNCHER_SWAPPED_SHA:-}"
  '
}

# ── A. Happy path: good SHA → atomic on-disk replace + .bak kept ──────────────
A_DISK="OLD-LAUNCHER-v0.1.10
"
A_CAND="NEW-LAUNCHER-v0.1.11
"
A_OUT="$(run_case A "$A_DISK" "$A_CAND" GOOD || true)"
A_RC="$(printf '%s\n' "$A_OUT" | grep -oE 'RC=[0-9]+' | tail -1)"
A_FILE="$SANDBOX/case-A/mine-auto.sh"
if [ "$A_RC" = "RC=0" ] && file_is_string "$A_FILE" "$A_CAND"; then
  ok "A happy-path: on-disk launcher replaced with the verified candidate (RC=0)"
else
  fail "A happy-path" "expected RC=0 and on-disk == candidate; got $A_RC, content=[$(cat "$A_FILE" 2>/dev/null)]"
fi
if [ -f "$SANDBOX/case-A/mine-auto.sh.bak" ] && file_is_string "$SANDBOX/case-A/mine-auto.sh.bak" "$A_DISK"; then
  ok "A happy-path: prior launcher preserved as .bak (no-brick fallback)"
else
  fail "A happy-path .bak" "expected .bak == old on-disk launcher"
fi

# ── B. Fail-closed: WRONG sha in SHA256SUMS → old launcher untouched ──────────
B_DISK="OLD-LAUNCHER-KEEP-ME
"
B_CAND="MALICIOUS-OR-CORRUPT
"
B_OUT="$(run_case B "$B_DISK" "$B_CAND" BADSHA || true)"
B_RC="$(printf '%s\n' "$B_OUT" | grep -oE 'RC=[0-9]+' | tail -1)"
B_FILE="$SANDBOX/case-B/mine-auto.sh"
if [ "$B_RC" != "RC=0" ] && file_is_string "$B_FILE" "$B_DISK"; then
  ok "B fail-closed (bad SHA): on-disk launcher BYTE-IDENTICAL to original, swap refused (RC!=0)"
else
  fail "B fail-closed bad SHA" "expected RC!=0 and unchanged on-disk; got $B_RC, content=[$(cat "$B_FILE" 2>/dev/null)]"
fi
if [ ! -e "$SANDBOX/case-B/mine-auto.sh.bak" ]; then
  ok "B fail-closed (bad SHA): no .bak written (nothing was swapped)"
else
  fail "B fail-closed .bak" "a .bak was written despite the verify failing"
fi

# ── C. Fail-closed: SELF_NAME not listed in SHA256SUMS → old kept ─────────────
C_DISK="OLD-LAUNCHER-KEEP-ME-2
"
C_CAND="WHATEVER
"
C_OUT="$(run_case C "$C_DISK" "$C_CAND" MISSING || true)"
C_RC="$(printf '%s\n' "$C_OUT" | grep -oE 'RC=[0-9]+' | tail -1)"
C_FILE="$SANDBOX/case-C/mine-auto.sh"
if [ "$C_RC" != "RC=0" ] && file_is_string "$C_FILE" "$C_DISK"; then
  ok "C fail-closed (no SHA256SUMS entry): on-disk launcher unchanged, swap refused (RC!=0)"
else
  fail "C fail-closed missing entry" "expected RC!=0 and unchanged on-disk; got $C_RC"
fi

# ── D. Fail-closed: launcher download fails → old kept ────────────────────────
D_DISK="OLD-LAUNCHER-KEEP-ME-3
"
D_CAND="UNUSED
"
D_OUT="$(run_case D "$D_DISK" "$D_CAND" DLFAIL || true)"
D_RC="$(printf '%s\n' "$D_OUT" | grep -oE 'RC=[0-9]+' | tail -1)"
D_FILE="$SANDBOX/case-D/mine-auto.sh"
if [ "$D_RC" != "RC=0" ] && file_is_string "$D_FILE" "$D_DISK"; then
  ok "D fail-closed (download fails): on-disk launcher unchanged, swap refused (RC!=0)"
else
  fail "D fail-closed download" "expected RC!=0 and unchanged on-disk; got $D_RC"
fi

# ── E. Skip-if-same: on-disk already == candidate → rc=2, no rewrite, no .bak ─
# rc=2 = "verified, already current" (mirrors h-run's ua_download_verify_swap):
# the caller must NOT re-exec on it — that's the natural re-exec loop-break.
E_SAME="SAME-LAUNCHER-CONTENT
"
E_OUT="$(run_case E "$E_SAME" "$E_SAME" GOOD || true)"
E_RC="$(printf '%s\n' "$E_OUT" | grep -oE 'RC=[0-9]+' | tail -1)"
E_FILE="$SANDBOX/case-E/mine-auto.sh"
if [ "$E_RC" = "RC=2" ] && file_is_string "$E_FILE" "$E_SAME" && [ ! -e "$SANDBOX/case-E/mine-auto.sh.bak" ]; then
  ok "E skip-if-same: identical launcher → RC=2 (no change), content unchanged, NO .bak churn"
else
  fail "E skip-if-same" "expected RC=2, unchanged content, no .bak; got $E_RC, bak=$( [ -e "$SANDBOX/case-E/mine-auto.sh.bak" ] && echo present || echo absent)"
fi

# ── R. rc contract: a REAL byte change → rc=0 + persisted verified digest ─────
# The digest (LAUNCHER_SWAPPED_SHA) is what reexec_new_launcher re-verifies
# against before exec'ing (guards a racing writer between mv and exec).
R_SHA_EXPECT="$(sha_of_string "$A_CAND")"
R_SHA_GOT="$(printf '%s\n' "$A_OUT" | grep -oE 'SWAPPED_SHA=[0-9a-f]*' | tail -1)"
if [ "$R_SHA_GOT" = "SWAPPED_SHA=$R_SHA_EXPECT" ]; then
  ok "R rc contract: real swap (rc=0) persists the verified digest for the re-exec step"
else
  fail "R rc contract" "expected SWAPPED_SHA=$R_SHA_EXPECT after the happy-path swap; got '$R_SHA_GOT'"
fi

# ── F. No-brick: update_launcher_self body must contain NO `exec` COMMAND ─────
# Extract the function body and assert it never re-execs (mid-run re-exec of a
# new launcher is the brick risk we explicitly forbid). We must ignore the word
# "exec" inside COMMENTS (the body legitimately documents "we do NOT exec it"),
# so strip whole-line and inline `# ...` comments before scanning for an actual
# exec command token.
FN_BODY="$(awk '/^update_launcher_self\(\) \{/{f=1} f{print} /^}/{if(f)exit}' "$LAUNCHER")"
FN_CODE="$(printf '%s\n' "$FN_BODY" | sed -e 's/#.*$//')"   # drop comments
if printf '%s\n' "$FN_CODE" | grep -qE '(^|[^[:alnum:]_])exec([^[:alnum:]_]|$)'; then
  fail "F no-brick (no re-exec)" "update_launcher_self has an 'exec' command — it must replace on disk, never re-exec"
else
  ok "F no-brick: update_launcher_self code contains no 'exec' command (no mid-run re-exec)"
fi

# ── G. Atomic swap: implementation uses mv (rename), not truncate-in-place ────
if printf '%s\n' "$FN_BODY" | grep -qE 'mv[[:space:]].*"\$SELF_PATH"'; then
  ok "G atomic swap: uses mv onto \$SELF_PATH (atomic rename, no partial-write window)"
else
  fail "G atomic swap" "expected an 'mv ... \$SELF_PATH' atomic rename in update_launcher_self"
fi

# ── v0.2.0 staged-handoff re-exec (reexec_new_launcher) ───────────────────────
# The autorestart root-cause fix: update_launcher_self swaps the launcher on
# disk but never re-execs, so on an always-running rig the new launcher NEVER
# took effect. reexec_new_launcher applies it via a gated staged handoff. Every
# guard failing must fall back to "keep running the OLD in-memory launcher,
# miners keep mining" and drop a crumb file for the operator.
echo
echo "-- staged-handoff re-exec (reexec_new_launcher) --"

# Driver. Args:
#   $1 tag
#   $2 target-script content ("" => SELF_PATH points at a MISSING file)
#   $3 sha mode: GOOD (sha of the target) | BAD (deliberate mismatch)
#   $4 gen preset (e.g. "export CSD_REEXEC_GEN=3") or ""
#   $5 "BAK" to pre-place a .bak beside the target, else ""
# The stubbed stop_miners/start_miners append to $work/calls; a successfully
# exec'd target script writes $work/execed (proving the handoff + argv + gen).
reexec_case() {
  local tag="$1" content="$2" shamode="$3" gen_preset="$4" mkbak="$5"
  local work="$SANDBOX/reexec-$tag"
  mkdir -p "$work"
  local target="$work/mine-auto.sh"
  if [ -n "$content" ]; then
    printf '%s' "$content" > "$target"
    chmod +x "$target"
  fi
  [ "$mkbak" = "BAK" ] && printf '%s' "GOOD-OLD-BAK-LAUNCHER" > "$target.bak"
  local sha=""
  if [ "$shamode" = "GOOD" ] && [ -f "$target" ]; then
    sha="$(sha256sum "$target" | awk '{print $1}')"
  else
    sha="1111111111111111111111111111111111111111111111111111111111111111"
  fi

  # `set -euo pipefail` MATCHES production (mine-auto.sh line 2) so the harness
  # exercises reexec_new_launcher under the SAME shell options the rig runs it
  # with — a -e-less harness could green-light a `set -e`/exec-failure brick that
  # production would hit. Case V's failed exec must still fall through to the
  # miner-recovery path (execfail + the fn being a `||` left operand keep -e from
  # aborting); this harness proves it does under real -e.
  CSD_SOURCE_ONLY=1 CASE_DIR="$work" \
  bash -c '
    set -euo pipefail
    source "'"$LAUNCHER"'" >/dev/null 2>&1
    SELF_PATH="'"$target"'"
    DATA_DIR="$CASE_DIR"            # crumb file lands here
    ORIG_ARGS=(nvidia)              # the argv the handoff must preserve
    # Stub the miner lifecycle so the test never touches real processes.
    stop_miners()  { echo stop  >> "$CASE_DIR/calls"; }
    start_miners() { echo start >> "$CASE_DIR/calls"; }
    '"$gen_preset"'
    rc=0
    reexec_new_launcher "'"$sha"'" || rc=$?
    echo "RC=$rc"
  '
}

# A target script that, when exec'd, proves the handoff happened with the right
# argv + generation counter (CASE_DIR survives the exec via the environment).
EXEC_PROOF='#!/usr/bin/env bash
echo "EXECED gen=${CSD_REEXEC_GEN:-none} args=$*" > "$CASE_DIR/execed"
'
# Syntactically valid bash whose exec MUST fail: the shebang interpreter does
# not exist (execve => ENOENT), portable across Linux + git-bash — unlike a
# chmod -x probe, which is a no-op on Windows filesystems.
EXEC_FAIL='#!/nonexistent/interpreter-for-execfail-test
echo never-runs
'
BROKEN_SYNTAX='#!/usr/bin/env bash
if [ ; then fi (((
'

# ── S1. Refuses when SELF_PATH is not a real readable file (piped run) ────────
S1_OUT="$(reexec_case S1 "" GOOD "" "" || true)"
S1_RC="$(printf '%s\n' "$S1_OUT" | grep -oE 'RC=[0-9]+' | tail -1)"
if [ "$S1_RC" != "RC=0" ] && [ -n "$S1_RC" ] && [ ! -e "$SANDBOX/reexec-S1/execed" ] \
   && [ -f "$SANDBOX/reexec-S1/launcher-reexec-refused.txt" ]; then
  ok "S1 re-exec: missing SELF_PATH (piped run) → refused, no exec, crumb written"
else
  fail "S1 re-exec missing SELF_PATH" "expected refusal + crumb + no exec; got rc=$S1_RC execed=$( [ -e "$SANDBOX/reexec-S1/execed" ] && echo yes || echo no) crumb=$( [ -f "$SANDBOX/reexec-S1/launcher-reexec-refused.txt" ] && echo yes || echo no)"
fi

# ── S2. Refuses when the on-disk SHA != the swap-time digest (racing writer) ──
S2_OUT="$(reexec_case S2 "$EXEC_PROOF" BAD "" "" || true)"
S2_RC="$(printf '%s\n' "$S2_OUT" | grep -oE 'RC=[0-9]+' | tail -1)"
if [ "$S2_RC" != "RC=0" ] && [ -n "$S2_RC" ] && [ ! -e "$SANDBOX/reexec-S2/execed" ] \
   && [ -f "$SANDBOX/reexec-S2/launcher-reexec-refused.txt" ]; then
  ok "S2 re-exec: on-disk SHA mismatch vs swap-time digest → refused, no exec, crumb written"
else
  fail "S2 re-exec SHA re-verify" "expected refusal + crumb + no exec; got rc=$S2_RC"
fi

# ── T. Refuses on bash -n failure AND restores the .bak over the bad script ───
T_OUT="$(reexec_case T "$BROKEN_SYNTAX" GOOD "" BAK || true)"
T_RC="$(printf '%s\n' "$T_OUT" | grep -oE 'RC=[0-9]+' | tail -1)"
T_FILE="$SANDBOX/reexec-T/mine-auto.sh"
if [ "$T_RC" != "RC=0" ] && [ -n "$T_RC" ] && [ ! -e "$SANDBOX/reexec-T/execed" ] \
   && [ -f "$SANDBOX/reexec-T/launcher-reexec-refused.txt" ] \
   && file_is_string "$T_FILE" "GOOD-OLD-BAK-LAUNCHER"; then
  ok "T re-exec: bash -n syntax gate refuses a corrupt launcher and restores the .bak"
else
  fail "T re-exec syntax gate" "expected refusal + .bak restored; got rc=$T_RC content=[$(head -c 60 "$T_FILE" 2>/dev/null)]"
fi

# ── U. Bounded: refuses at the CSD_REEXEC_GEN generation cap (no exec loop) ───
U_OUT="$(reexec_case U "$EXEC_PROOF" GOOD "export CSD_REEXEC_GEN=3" "" || true)"
U_RC="$(printf '%s\n' "$U_OUT" | grep -oE 'RC=[0-9]+' | tail -1)"
if [ "$U_RC" != "RC=0" ] && [ -n "$U_RC" ] && [ ! -e "$SANDBOX/reexec-U/execed" ] \
   && [ -f "$SANDBOX/reexec-U/launcher-reexec-refused.txt" ]; then
  ok "U re-exec: generation cap (CSD_REEXEC_GEN>=3) → refused, no exec (bounded, never a loop)"
else
  fail "U re-exec gen cap" "expected refusal at gen cap; got rc=$U_RC execed=$( [ -e "$SANDBOX/reexec-U/execed" ] && echo yes || echo no)"
fi

# ── V. execfail fall-through: a failed exec restarts the miners + refuses ─────
V_OUT="$(reexec_case V "$EXEC_FAIL" GOOD "" "" || true)"
V_RC="$(printf '%s\n' "$V_OUT" | grep -oE 'RC=[0-9]+' | tail -1)"
V_CALLS="$( { cat "$SANDBOX/reexec-V/calls" 2>/dev/null || true; } | tr '\n' ',')"
if [ "$V_RC" != "RC=0" ] && [ -n "$V_RC" ] \
   && [ "$V_CALLS" = "stop,start," ] \
   && [ -f "$SANDBOX/reexec-V/launcher-reexec-refused.txt" ]; then
  ok "V re-exec: failed exec (execfail) falls through → miners RESTARTED on the old launcher, crumb written"
else
  fail "V re-exec execfail fall-through" "expected rc!=0 + calls=stop,start + crumb; got rc=$V_RC calls=[$V_CALLS]"
fi

# ── W. Happy path: execs the NEW launcher with the ORIGINAL argv, generation 1 ─
# THE autorestart fix: the staged launcher takes effect WITHOUT operator action.
# (On old code this fails: reexec_new_launcher doesn't exist / nothing execs.)
W_OUT="$(reexec_case W "$EXEC_PROOF" GOOD "" "" || true)"
W_EXECED="$(cat "$SANDBOX/reexec-W/execed" 2>/dev/null || true)"
W_CALLS="$( { cat "$SANDBOX/reexec-W/calls" 2>/dev/null || true; } | tr '\n' ',')"
if [ "$W_EXECED" = "EXECED gen=1 args=nvidia" ] && [ "$W_CALLS" = "stop," ] \
   && [ ! -f "$SANDBOX/reexec-W/launcher-reexec-refused.txt" ]; then
  ok "W re-exec happy path: miners stopped, NEW launcher exec'd with original argv at generation 1 (no manual restart)"
else
  fail "W re-exec happy path" "expected execed marker 'EXECED gen=1 args=nvidia' + calls=stop + no crumb; got execed=[$W_EXECED] calls=[$W_CALLS]"
fi

# ── X. Wiring + parity: re-exec fires ONLY on a real swap (rc=0) ──────────────
# do_update_check must gate reexec_new_launcher on update_launcher_self rc=0
# (never on rc=2 skip-if-same — the loop-break — nor rc=1 failure), and
# mine-auto.bat keeps its startup trampoline: parity = BOTH launchers apply a
# staged launcher without operator action.
DUC_BODY="$(awk '/^do_update_check\(\) \{/{f=1} f{print} /^}/{if(f)exit}' "$LAUNCHER")"
if printf '%s\n' "$DUC_BODY" | grep -q 'reexec_new_launcher' \
   && printf '%s\n' "$DUC_BODY" | grep -qE 'eq 0.*\]|\-eq 0'; then
  ok "X wiring: do_update_check invokes reexec_new_launcher gated on rc=0 (real byte change only)"
else
  fail "X wiring" "do_update_check must call reexec_new_launcher only when update_launcher_self returned 0"
fi
if grep -qE '^:update_launcher_self' "$REPO_ROOT/mine-auto.bat" && grep -qE 'csd-launcher-promote\.cmd' "$REPO_ROOT/mine-auto.bat"; then
  ok "X parity: mine-auto.bat keeps its trampoline — both launchers apply staged launchers unattended"
else
  fail "X parity" "mine-auto.bat trampoline missing"
fi

# ── mine-auto.bat (Windows) launcher self-update — STATIC safety checks ───────
# The .bat path can't be exercised hermetically here (it needs detached cmd.exe
# processes), so we lock in its critical SAFETY INVARIANTS statically. The
# brick-relevant rule the .bat must obey: NEVER move/replace its OWN running file
# (%~f0) inside :update_launcher_self — that derails a running cmd (verified
# empirically). The mid-run path may only STAGE a verified copy to %SELF_NEW%;
# the actual promote happens via the startup trampoline (a detached helper that
# runs AFTER this process exits).
BAT="$REPO_ROOT/mine-auto.bat"
echo
echo "-- mine-auto.bat launcher self-update (static safety) --"

# H. :update_launcher_self subroutine exists.
if grep -qE '^:update_launcher_self' "$BAT"; then
  ok "H bat: :update_launcher_self subroutine present"
else
  fail "H bat: subroutine" ":update_launcher_self not found in mine-auto.bat"
fi

# Extract the :update_launcher_self subroutine body (from its label to the next
# 'goto :eof' that closes it / the next label).
BAT_FN="$(awk '/^:update_launcher_self/{f=1} f{print} f&&/^goto :eof/{exit}' "$BAT")"

# I. FAIL-CLOSED: the subroutine refuses on a missing SHA256SUMS entry and on a
#    verify mismatch (mirrors the .sh). Check for the refusal + the discard.
if printf '%s\n' "$BAT_FN" | grep -qiE 'no SHA256SUMS entry'; then
  ok "I bat fail-closed: refuses when SELF_NAME not listed in SHA256SUMS"
else
  fail "I bat fail-closed" "missing 'no SHA256SUMS entry' refusal in :update_launcher_self"
fi
if printf '%s\n' "$BAT_FN" | grep -qiE 'verify FAILED'; then
  ok "I bat fail-closed: discards on SHA-256 verify failure"
else
  fail "I bat fail-closed verify" "missing 'verify FAILED' discard path"
fi

# J. NO-BRICK: :update_launcher_self must NOT move/copy onto %~f0 (its own running
#    file). It may only write the staging slot %SELF_NEW% (or the scratch .dl).
#    A move/copy targeting "%~f0" inside the subroutine is the forbidden brick.
if printf '%s\n' "$BAT_FN" | grep -ivE '^\s*REM' | grep -qiE '(move|copy)\b[^\r\n]*"%~f0"'; then
  fail "J bat no-brick" ":update_launcher_self moves/copies onto %~f0 (its own running file) — forbidden; stage to %SELF_NEW% instead"
else
  ok "J bat no-brick: :update_launcher_self never moves/copies onto its own running file (%~f0)"
fi

# K. The subroutine stages to %SELF_NEW% (the slot the trampoline promotes).
if printf '%s\n' "$BAT_FN" | grep -qE 'move /Y "!SELF_DL!" "%SELF_NEW%"'; then
  ok "K bat: stages the verified launcher to %SELF_NEW% (promoted later by the trampoline)"
else
  fail "K bat staging" "expected 'move /Y \"!SELF_DL!\" \"%SELF_NEW%\"' staging step"
fi

# L. NO-BRICK promote: the startup trampoline must hand off to a DETACHED helper
#    and `exit /b` BEFORE the running file is replaced (never an in-process move
#    over %~f0 while the loop runs). Assert the trampoline writes a helper .cmd
#    and exits.
if grep -qE 'csd-launcher-promote\.cmd' "$BAT" && grep -qE 'NO-BRICK launcher promote' "$BAT"; then
  ok "L bat no-brick promote: startup trampoline uses a detached helper .cmd (no in-process self-replace)"
else
  fail "L bat no-brick promote" "missing the detached helper trampoline (csd-launcher-promote.cmd)"
fi
# The trampoline block must `exit /b` after launching the helper (so our file is
# free before the helper moves it).
TRAMP="$(awk '/NO-BRICK launcher promote/{f=1} f{print} f&&/call :update_check/{exit}' "$BAT")"
if printf '%s\n' "$TRAMP" | grep -qE '^\s*exit /b 0'; then
  ok "L bat no-brick promote: trampoline exits (exit /b) before the helper replaces the file"
else
  fail "L bat no-brick promote exit" "trampoline must 'exit /b' after launching the helper"
fi

# M. The promote must NOT happen after miners/relay are spawned: the trampoline
#    block precedes 'call :update_check' (which is the first thing that spawns).
TRAMP_LINE=$(grep -n 'NO-BRICK launcher promote' "$BAT" | head -1 | cut -d: -f1)
UPDCHK_LINE=$(grep -n '^call :update_check' "$BAT" | head -1 | cut -d: -f1)
if [ -n "$TRAMP_LINE" ] && [ -n "$UPDCHK_LINE" ] && [ "$TRAMP_LINE" -lt "$UPDCHK_LINE" ]; then
  ok "M bat: launcher promote runs pre-spawn (trampoline@$TRAMP_LINE before first spawn@$UPDCHK_LINE)"
else
  fail "M bat promote ordering" "trampoline (${TRAMP_LINE:-?}) must precede call :update_check (${UPDCHK_LINE:-?})"
fi

# N. Trusted verifier: the .bat verifies the staged launcher with %BIN% verify-file
#    or PowerShell Get-FileHash — never letting the download verify itself.
if printf '%s\n' "$BAT_FN" | grep -qE '"%BIN%" verify-file "!SELF_DL!"' \
   && printf '%s\n' "$BAT_FN" | grep -qiE 'Get-FileHash'; then
  ok "N bat trusted verifier: uses %BIN% verify-file (trusted) with Get-FileHash OS fallback"
else
  fail "N bat trusted verifier" "expected %BIN% verify-file + Get-FileHash fallback in :update_launcher_self"
fi

# ── hiveos/h-run.sh SAME-BINARY GUARD (no HiveOS flap on launcher-only bumps) ──
# A launcher-only release republishes the SAME binary under a higher version, so
# the miner's --version stays stale forever. Without a guard, h-run.sh's sidecar
# re-downloads + kills/restarts the miner every poll (fleet-wide flapping). The
# guard: ua_download_verify_swap returns 2 ("verified, no change") when the
# on-disk binary already hashes to the target digest, so the caller skips the
# swap AND the miner-kill. These tests source h-run.sh hermetically and drive
# ua_download_verify_swap with a fake release.
HRUN="$REPO_ROOT/hiveos/h-run.sh"
echo
echo "-- hiveos/h-run.sh same-binary guard (no launcher-only-bump flap) --"

# Driver: stage an on-disk binary + a downloadable "release" binary + SHA256SUMS,
# source h-run.sh, override ua_download to serve the local files, run
# ua_download_verify_swap, print its RC. $1=tag $2=ondisk-content $3=release-content
hrun_case() {
  local tag="$1" ondisk="$2" release="$3"
  local work="$SANDBOX/hrun-$tag"
  mkdir -p "$work"
  printf '%s' "$ondisk"  > "$work/csd-gpu-miner"
  printf '%s' "$release" > "$work/release-bin"
  chmod +x "$work/csd-gpu-miner"
  local rsha
  rsha="$(sha256sum "$work/release-bin" | awk '{print $1}')"
  printf '%s  csd-pool-miner-linux-cpu\n' "$rsha" > "$work/SHA256SUMS"

  CSD_SOURCE_ONLY=1 CASE_DIR="$work" \
  bash -c '
    set -uo pipefail
    source "'"$HRUN"'" >/dev/null 2>&1
    UPDATE_BIN="'"$work/csd-gpu-miner"'"
    LOG="'"$work/log"'"
    EXTRA_FLAGS="--backend cpu"          # → variant cpu → asset csd-pool-miner-linux-cpu
    ua_download() {
      local url="$1" out="$2"
      case "$url" in
        *"/csd-pool-miner-linux-cpu") cp "$CASE_DIR/release-bin" "$out" ;;
        *"/SHA256SUMS")               cp "$CASE_DIR/SHA256SUMS" "$out" ;;
        *) return 1 ;;
      esac
    }
    rc=0
    ua_download_verify_swap || rc=$?
    echo "RC=$rc"
  '
}

# O. Same binary on disk as in the release → return 2 (no change), binary untouched.
O_BIN="IDENTICAL-MINER-BYTES-v0.1.10
"
O_OUT="$(hrun_case O "$O_BIN" "$O_BIN" || true)"
O_RC="$(printf '%s\n' "$O_OUT" | grep -oE 'RC=[0-9]+' | tail -1)"
if [ "$O_RC" = "RC=2" ] && file_is_string "$SANDBOX/hrun-O/csd-gpu-miner" "$O_BIN"; then
  ok "O h-run.sh: identical on-disk binary → ua_download_verify_swap returns 2 (no swap, no miner-kill) [stops HiveOS flap]"
else
  fail "O h-run.sh same-binary guard" "expected RC=2 and unchanged binary; got $O_RC"
fi

# P. DIFFERENT binary in the release → return 0 (real swap), on-disk now == release.
P_OLD="OLD-MINER-BYTES
"
P_NEW="NEW-MINER-BYTES-genuinely-different
"
P_OUT="$(hrun_case P "$P_OLD" "$P_NEW" || true)"
P_RC="$(printf '%s\n' "$P_OUT" | grep -oE 'RC=[0-9]+' | tail -1)"
if [ "$P_RC" = "RC=0" ] && file_is_string "$SANDBOX/hrun-P/csd-gpu-miner" "$P_NEW"; then
  ok "P h-run.sh: genuinely newer binary → returns 0 and atomically swaps it in (real updates still work)"
else
  fail "P h-run.sh real swap" "expected RC=0 and binary==release; got $P_RC"
fi

# Q. The sidecar must NOT kill the miner on rc=2 (the loop-breaking guard), but
#    MUST still kill+restart on rc=0 (a genuine update). We extract the sidecar
#    by line range: from the FIRST 'confirmed_current_for=""' (sidecar-local init)
#    to the end of file, then inspect the rc=0 vs rc=2 branches by their line
#    positions relative to the rc=2 marker line `confirmed_current_for="$latest"`.
# Find the sidecar's rc=2 marker (the no-flap assignment) and the two pkill lines.
RC2_MARK=$(grep -n 'confirmed_current_for="\$latest"' "$HRUN" | head -1 | cut -d: -f1)
RC2_ELIF=$(grep -n 'elif \[ "\$rc" -eq 2 \]' "$HRUN" | tail -1 | cut -d: -f1)   # sidecar one (last)
# The pkill of the miner inside the sidecar's rc=0 branch:
SIDE_PKILL=$(grep -n 'pkill -f "\$MINER_PROC"' "$HRUN" | head -1 | cut -d: -f1)
# Assert: the sidecar rc=2 branch (from RC2_ELIF to RC2_MARK) contains NO pkill.
if [ -n "$RC2_ELIF" ] && [ -n "$RC2_MARK" ] && [ "$RC2_ELIF" -lt "$RC2_MARK" ]; then
  RC2_BLOCK="$(sed -n "${RC2_ELIF},${RC2_MARK}p" "$HRUN")"
  if ! printf '%s\n' "$RC2_BLOCK" | grep -qE 'pkill'; then
    ok "Q h-run.sh sidecar: rc=2 path sets confirmed_current_for and contains NO pkill (no flap)"
  else
    fail "Q h-run.sh sidecar rc=2" "the rc=2 branch (lines $RC2_ELIF-$RC2_MARK) must not pkill the miner"
  fi
else
  fail "Q h-run.sh sidecar rc=2" "could not locate the sidecar rc=2 branch (elif@${RC2_ELIF:-?}, marker@${RC2_MARK:-?})"
fi
# Q2. The sidecar's rc=0 branch MUST kill the miner (real updates apply). The
#     miner pkill must sit BEFORE the rc=2 elif (i.e. inside the rc=0 branch).
if [ -n "$SIDE_PKILL" ] && [ -n "$RC2_ELIF" ] && [ "$SIDE_PKILL" -lt "$RC2_ELIF" ]; then
  ok "Q2 h-run.sh sidecar: rc=0 (real change) path STILL kills+restarts the miner (real updates apply)"
else
  fail "Q2 h-run.sh sidecar rc=0" "rc=0 branch must pkill the miner (pkill@${SIDE_PKILL:-?} should precede rc=2 elif@${RC2_ELIF:-?})"
fi

echo
echo "========================================"
echo "  Passed: $PASS  Failed: $FAIL"
echo "========================================"
echo
[ "$FAIL" -gt 0 ] && exit 1
exit 0
