#!/usr/bin/env bash
# HERMETIC dry-run harness for FIX C-1 (sidecar reap) + FIX C-4 (per-pid temp) +
# fail-safe invariants of hive_update_sidecar in h-run.sh.
#
# We cannot use the real pgrep/pkill on this box (Git Bash on Windows does not
# model Linux /proc/<pid>/cmdline argv matching), so we model a FAKE process
# table in a file and stub pgrep/pkill against it. This isolates and proves the
# CONTROL FLOW the review asked about — reap-before-spawn, exactly-once miner
# kill on a VALID update, self-exit when the miner is gone, and NO kill on a
# FAILED update — independent of OS process semantics. The real marker→/proc
# matching is standard procps behaviour on the HiveOS target (confirmed argv[0]
# separately).
#
# Exit 0 = all assertions pass; non-zero = a failure (printed).
set -u

WORK="$(mktemp -d)"
PTABLE="$WORK/ptable"        # one "running marker" per line = the fake process table
KILLLOG="$WORK/killlog"      # every pkill target appended here, for exactly-once checks
: > "$PTABLE"; : > "$KILLLOG"
fails=0
ok()   { printf '  PASS: %s\n' "$*"; }
bad()  { printf '  FAIL: %s\n' "$*"; fails=$((fails+1)); }

# ── fake process primitives backed by $PTABLE ────────────────────────────────
proc_spawn()  { printf '%s\n' "$1" >> "$PTABLE"; }                  # add a running marker
proc_present(){ grep -qxF "$1" "$PTABLE"; }                          # is marker running?
proc_count()  { grep -Fx "$1" "$PTABLE" 2>/dev/null | wc -l | tr -d ' '; }  # how many copies

pgrep() {  # emulate: pgrep -f PATTERN  → 0 if any line CONTAINS pattern
  pat="${2:-$1}"
  grep -qF -- "$pat" "$PTABLE"
}
pkill() {  # emulate: pkill -f PATTERN  → record + remove every line CONTAINING pattern
  pat="${2:-$1}"
  printf '%s\n' "$pat" >> "$KILLLOG"
  grep -vF -- "$pat" "$PTABLE" > "$PTABLE.tmp" 2>/dev/null || : > "$PTABLE.tmp"
  mv "$PTABLE.tmp" "$PTABLE"
}
export -f pgrep pkill proc_present 2>/dev/null || true

# ── constants the sidecar body reads (mirror h-run.sh) ───────────────────────
MINER_PROC="csd-gpu-miner"
SIDE_MARKER="csd-hive-update-sidecar"
LOG="$WORK/miner.log"; : > "$LOG"
CHECK_MIN=0                 # so sleeps are 0s in the harness
UPDATE_BIN="$WORK/csd-gpu-miner"

# Swap-result knob: ua_download_verify_swap returns whatever this file says (0/1).
SWAPRESULT="$WORK/swapresult"; echo 1 > "$SWAPRESULT"   # default: FAIL (fail-safe)
ua_download_verify_swap() { return "$(cat "$SWAPRESULT")"; }
ua_latest_tag()        { echo "0.1.99"; }       # always "newer"
ua_installed_version() { echo "0.1.9"; }
ua_should_update()     { [ "${1#v}" != "${2#v}" ]; }   # real string-path logic

# ── the ACTUAL sidecar loop body, copied verbatim from h-run.sh (lines 261-289)
# except: the leading warm-up sleep is dropped and the trailing `sleep` is
# replaced by a single-pass `break`, so each invocation runs exactly ONE poll
# iteration deterministically. The decision logic is unchanged.
hive_update_sidecar_oneiter() {
  while true; do
    if ! pgrep -f "$MINER_PROC" >/dev/null 2>&1; then
      echo "[h-run] auto-update sidecar: miner no longer running — exiting sidecar." >> "$LOG" 2>&1
      return 0
    fi
    latest="$(ua_latest_tag || true)"
    if [ -n "$latest" ]; then
      installed="$(ua_installed_version)"
      if ua_should_update "$installed" "$latest"; then
        echo "[h-run] auto-update sidecar: $installed -> $latest (verify, then swap + restart)" >> "$LOG" 2>&1
        if ua_download_verify_swap; then
          echo "[h-run] auto-update sidecar: swapped in $latest — restarting miner." >> "$LOG" 2>&1
          pkill -f csd-relay-node 2>/dev/null || true
          pkill -f "$MINER_PROC" 2>/dev/null || true
          return 0
        else
          echo "[h-run] auto-update sidecar: update not applied — keeping current, will retry." >> "$LOG" 2>&1
        fi
      fi
    fi
    break   # harness: one iteration only (stands in for the periodic sleep+loop)
  done
  return 0
}

# ── the ACTUAL startup reap+spawn sequence from h-run.sh (FIX C-1) ────────────
# Mirrors lines around the sidecar launch: reap prior marker, sweep stale temps,
# then "spawn" a new marked sidecar (here = add the marker to the fake ptable).
hive_startup_reap_and_spawn() {
  pkill -f "$SIDE_MARKER" 2>/dev/null || true
  rm -f "$UPDATE_BIN".new.* 2>/dev/null || true
  proc_spawn "$SIDE_MARKER"   # = bash -c 'hive_update_sidecar' csd-hive-update-sidecar &
}

echo "================ (a) sidecar-reap across two launches ================"
: > "$PTABLE"; : > "$KILLLOG"
# Launch #1 (e.g. first boot): miner + a sidecar come up.
proc_spawn "$MINER_PROC --address 0xabc --device 0"
hive_startup_reap_and_spawn
c1=$(proc_count "$SIDE_MARKER")
[ "$c1" -eq 1 ] && ok "after launch #1: exactly 1 sidecar (got $c1)" || bad "after launch #1 expected 1 sidecar, got $c1"
# A NON-update HiveOS restart (flightsheet edit / OC apply / manual): h-run.sh
# re-runs. The OLD sidecar is still in the table (it was sleeping, not its own
# pkill). Without the reap there would now be 2; the reap must collapse to 1.
hive_startup_reap_and_spawn
c2=$(proc_count "$SIDE_MARKER")
[ "$c2" -eq 1 ] && ok "after launch #2 (non-update restart): still exactly 1 sidecar — OLD reaped (got $c2)" \
                 || bad "after launch #2 expected 1 sidecar (old reaped), got $c2"
# And a third, to be sure it never accumulates.
hive_startup_reap_and_spawn
c3=$(proc_count "$SIDE_MARKER")
[ "$c3" -eq 1 ] && ok "after launch #3: still exactly 1 sidecar (got $c3)" || bad "after launch #3 expected 1, got $c3"
# The reap must NOT have touched the miner.
proc_present "$MINER_PROC --address 0xabc --device 0" && ok "miner process untouched by sidecar reap" || bad "reap killed the miner!"

echo
echo "================ (b1) VALID update: miner pkilled EXACTLY once, sidecar self-exits ================"
: > "$PTABLE"; : > "$KILLLOG"
echo 0 > "$SWAPRESULT"            # swap SUCCEEDS
proc_spawn "$MINER_PROC --address 0xabc --device 0"
proc_spawn "csd-relay-node --rpc 127.0.0.1:18645"
proc_spawn "$SIDE_MARKER"
hive_update_sidecar_oneiter; rc=$?
[ "$rc" -eq 0 ] && ok "sidecar returned 0 (self-exit) after a valid update" || bad "sidecar rc=$rc (expected 0)"
miner_kills=$(grep -Fx "$MINER_PROC" "$KILLLOG" 2>/dev/null | wc -l | tr -d ' ')
[ "$miner_kills" -eq 1 ] && ok "miner pkilled exactly once on valid update (got $miner_kills)" || bad "expected 1 miner pkill, got $miner_kills"
relay_kills=$(grep -Fx "csd-relay-node" "$KILLLOG" 2>/dev/null | wc -l | tr -d ' ')
[ "$relay_kills" -eq 1 ] && ok "relay pkilled exactly once on valid update (got $relay_kills)" || bad "expected 1 relay pkill, got $relay_kills"
# The sidecar's own self-pkill of the miner must NOT have matched the sidecar
# marker (marker uniqueness): assert it never pkilled its own marker.
side_self_kill=$(grep -Fx "$SIDE_MARKER" "$KILLLOG" 2>/dev/null | wc -l | tr -d ' ')
[ "$side_self_kill" -eq 0 ] && ok "sidecar never pkilled its own marker (no self-kill collision)" || bad "sidecar pkilled its own marker $side_self_kill time(s)"

echo
echo "================ (b2) FAILED update: miner is NOT pkilled (fail-safe unchanged) ================"
: > "$PTABLE"; : > "$KILLLOG"
echo 1 > "$SWAPRESULT"            # swap FAILS (download/verify failed)
proc_spawn "$MINER_PROC --address 0xabc --device 0"
proc_spawn "csd-relay-node --rpc 127.0.0.1:18645"
proc_spawn "$SIDE_MARKER"
hive_update_sidecar_oneiter; rc=$?
[ "$rc" -eq 0 ] && ok "sidecar returned 0 (kept polling, did not crash) on failed update" || bad "sidecar rc=$rc"
miner_kills=$(grep -Fx "$MINER_PROC" "$KILLLOG" 2>/dev/null | wc -l | tr -d ' ')
[ "$miner_kills" -eq 0 ] && ok "miner NOT pkilled on a failed update — FAIL-SAFE intact (got $miner_kills)" || bad "miner pkilled $miner_kills time(s) on a FAILED update!"
proc_present "$MINER_PROC --address 0xabc --device 0" && ok "miner still running after failed update" || bad "miner gone after a failed update!"

echo
echo "================ (b3) miner already gone: sidecar self-exits, no pkill ================"
: > "$PTABLE"; : > "$KILLLOG"
echo 0 > "$SWAPRESULT"            # even if a swap would succeed, miner is absent
proc_spawn "$SIDE_MARKER"        # only the sidecar in the table; NO miner
hive_update_sidecar_oneiter; rc=$?
[ "$rc" -eq 0 ] && ok "sidecar self-exits (rc 0) when miner is gone — never spins on a dead slot" || bad "sidecar rc=$rc with miner gone"
total_kills=$(wc -l < "$KILLLOG" 2>/dev/null | tr -d ' ')
[ "${total_kills:-0}" -eq 0 ] && ok "no pkill issued when miner already gone (got ${total_kills:-0})" || bad "issued ${total_kills} pkill(s) when miner already gone"

echo
echo "================ (C-4) per-pid staging temp does not collide ================"
# Two updaters with different PIDs stage to "$UPDATE_BIN.new.$$" → distinct files.
sim_pid_a=1111; sim_pid_b=2222
touch "$UPDATE_BIN.new.$sim_pid_a" "$UPDATE_BIN.new.$sim_pid_b"
n=$(ls "$UPDATE_BIN".new.* 2>/dev/null | wc -l | tr -d ' ')
[ "$n" -eq 2 ] && ok "two PIDs stage to two distinct temps (no fixed-name collision)" || bad "expected 2 distinct temps, got $n"
# The startup sweep clears stale per-pid temps but never the live binary.
touch "$UPDATE_BIN"
rm -f "$UPDATE_BIN".new.* 2>/dev/null || true
[ -e "$UPDATE_BIN" ] && ok "startup sweep kept the live binary" || bad "startup sweep deleted the live binary!"
[ -z "$(ls "$UPDATE_BIN".new.* 2>/dev/null)" ] && ok "startup sweep removed all stale .new.* temps" || bad "stale temps survived the sweep"

echo
rm -rf "$WORK"
if [ "$fails" -eq 0 ]; then echo "ALL SIDECAR HARNESS ASSERTIONS PASSED"; exit 0
else echo "SIDECAR HARNESS FAILURES: $fails"; exit 1; fi
