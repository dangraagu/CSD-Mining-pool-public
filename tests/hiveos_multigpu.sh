#!/usr/bin/env bash
# tests/hiveos_multigpu.sh — BRICK-SAFE multi-GPU launch in hiveos/h-run.sh.
#
# THE BUG: a multi-GPU HiveOS rig mines only ONE GPU. The binary is
# one-process-one-device (`--device N`); `--gpu-id` is merely an include-list the
# LAUNCHER reads to spawn one process per card (proven in the binary help +
# mine-all-gpus.sh). h-run.sh `exec`s exactly ONE process, so HiveOS rigs with 2+
# GPUs leave every card but device 0 idle.
#
# THE FIX (ported from mine-all-gpus.sh:250-300): IMMEDIATELY BEFORE the unchanged
# final `exec … --device 0`, h-run.sh launches devices 1..N-1 in the BACKGROUND
# (one process each, its own --device + a distinct stats port PORT+i + its own
# log). The whole pre-exec block is FAIL-SOFT: any failure (no nvidia-smi, count
# fails, a spawn errors, cpu variant) falls through to the unchanged single exec =
# the 1-GPU status quo. Worst case is 1 GPU; NEVER a brick.
#
# WHAT THIS PINS (the planning logic, sourced out of h-run.sh under a stubbed
# sandbox — mirroring tests/detect_variant.sh:resolve() and
# tests/hiveos_flow_e2e.sh:resolve_variant()):
#   hive_gpu_count()      — GPU count for the resolved variant (nvidia: nvidia-smi
#                           -L | grep -c '^GPU '; amd: clinfo GPU devices), timeout-
#                           guarded, fail-soft to empty/0.
#   hive_multi_gpu_plan() — the PURE plan: given the resolved variant, EXTRA_FLAGS
#                           (which may carry --gpu-id), PORT and the count, emit one
#                           line per launched device:
#                               EXEC device=0 port=<PORT>
#                               BG   device=<i> port=<PORT+i>
#                           Device 0 is ALWAYS the exec (and always present). The BG
#                           lines are the background launches. A cpu variant, count
#                           <2 / unknown, or a --gpu-id set without extra cards =>
#                           ONLY the EXEC line (no BG) — the brick-safe fallback.
#
# ASSERTIONS (each FAILS before the fix — h-run.sh has no such functions yet — and
# PASSES after):
#   (a) 4-GPU nvidia, no --gpu-id  => EXEC dev0(port=PORT) + BG dev1/2/3 (PORT+1/2/3)
#   (b) 1-GPU nvidia               => ONLY EXEC dev0 (no BG)
#   (c) GPU-count command absent   => ONLY EXEC dev0 (brick-safe fallback)
#   (d) --gpu-id 0,2 on 4-GPU box  => EXEC dev0 + BG dev2 only (1 and 3 skipped)
#   (e) cpu variant                => ONLY EXEC dev0 (single process)
#
# Run:  bash tests/hiveos_multigpu.sh
# Exit: 0 = all pass

set -uo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]:-$0}")/.." && pwd)"
HRUN="$ROOT/hiveos/h-run.sh"
PORT_BASE=3380
PASS=0; FAIL=0
ok()   { echo "  [PASS] $1"; PASS=$((PASS + 1)); }
fail() { echo "  [FAIL] $1" >&2; echo "         $2" >&2; FAIL=$((FAIL + 1)); }

# Make an executable stub on the sandbox PATH.
stub() { local dir="$1" name="$2" body="$3"; printf '#!/usr/bin/env bash\n%s\n' "$body" > "$dir/$name"; chmod +x "$dir/$name"; }

# Extract the planning functions from h-run.sh into one sourced file, then run
# hive_multi_gpu_plan under a stubbed PATH and echo the plan lines. Mirrors
# tests/hiveos_flow_e2e.sh:resolve_variant().
#   $1 resolved-variant override (nvidia|amd|cpu)
#   $2 EXTRA_FLAGS value
#   $3 sandbox-bin (PATH override)
#   $4 nvidia-dev-glob (for any detect_variant fall-through; unused when variant forced)
plan() {
  local variant="$1" flags="$2" binpath="$3" devglob="$4"
  : > "$SANDBOX/fn.sh"
  # All functions the plan transitively needs. Missing ones => __NO_*__ marker so a
  # pre-fix run produces an unmistakable diff (and the assertion fails cleanly).
  for f in update_variant detect_variant hive_gpu_count hive_gpu_id_list hive_multi_gpu_plan; do
    if grep -qE "^$f\(\)[[:space:]]*\{" "$HRUN"; then
      awk -v fn="$f" '$0 ~ "^"fn"\\(\\)[[:space:]]*\\{"{f=1} f{print} f&&/^\}/{exit}' "$HRUN" >> "$SANDBOX/fn.sh"
    else
      printf '%s() { echo "__NO_%s__"; }\n' "$f" "$(printf '%s' "$f" | tr '[:lower:]' '[:upper:]')" >> "$SANDBOX/fn.sh"
    fi
  done
  # Force the resolved variant (the plan keys off it; the test pins detection
  # separately via the real update_variant/detect_variant in hiveos_flow_e2e).
  printf 'update_variant() { echo "%s"; }\n' "$variant" >> "$SANDBOX/fn.sh"
  PATH="$binpath:/usr/bin:/bin" CSD_NVIDIA_DEV_GLOB="$devglob" \
    EXTRA_FLAGS="$flags" PORT="$PORT_BASE" \
    bash -c 'set -uo pipefail; source "$1"; hive_multi_gpu_plan' _ "$SANDBOX/fn.sh"
}

# A sandbox whose nvidia-smi -L reports N "GPU n:" lines.
sandbox_nvidia_n() {
  local n="$1" i
  SANDBOX="$(mktemp -d)"; BIN="$SANDBOX/bin"; mkdir -p "$BIN"; DEV="$SANDBOX/dev"; mkdir -p "$DEV"
  local body='if [ "$1" = "-L" ]; then'
  for ((i=0; i<n; i++)); do
    body+=$'\n'"  echo \"GPU $i: NVIDIA GeForce RTX 3080 (UUID: GPU-$i)\""
  done
  body+=$'\n''fi'$'\n''exit 0'
  stub "$BIN" nvidia-smi "$body"
}
# A sandbox with NO nvidia-smi (and no clinfo) at all.
sandbox_no_count() {
  SANDBOX="$(mktemp -d)"; BIN="$SANDBOX/bin"; mkdir -p "$BIN"; DEV="$SANDBOX/dev"; mkdir -p "$DEV"
}

# Assert the plan output equals the expected newline-separated set (order
# tolerant — sort both sides).
assert_plan() {
  local label="$1" got="$2" want="$3"
  local gs ws
  gs="$(printf '%s\n' "$got" | sed '/^$/d' | sort)"
  ws="$(printf '%s\n' "$want" | sed '/^$/d' | sort)"
  if [ "$gs" = "$ws" ]; then
    ok "$label"
  else
    fail "$label" "$(printf 'expected:\n%s\n--- got:\n%s' "$ws" "$gs")"
  fi
}

echo
echo "=== hiveos_multigpu: brick-safe spawn-one-process-per-GPU plan ==="
echo

# (a) 4-GPU nvidia, no --gpu-id => exec dev0 + bg dev1/2/3.
sandbox_nvidia_n 4
R="$(plan nvidia "--address dead" "$BIN" "$DEV/none/nvidia*")"
assert_plan "(a) 4-GPU nvidia, no --gpu-id => exec dev0 + bg dev1/2/3" "$R" \
"EXEC device=0 port=3380
BG device=1 port=3381
BG device=2 port=3382
BG device=3 port=3383"
rm -rf "$SANDBOX"

# (b) 1-GPU nvidia => only the exec, no background.
sandbox_nvidia_n 1
R="$(plan nvidia "--address dead" "$BIN" "$DEV/none/nvidia*")"
assert_plan "(b) 1-GPU nvidia => only exec dev0, no background" "$R" \
"EXEC device=0 port=3380"
rm -rf "$SANDBOX"

# (c) GPU-count command absent => only the exec (brick-safe fallback).
sandbox_no_count
R="$(plan nvidia "--address dead" "$BIN" "$DEV/none/nvidia*")"
assert_plan "(c) no nvidia-smi (count fails) => only exec dev0 (brick-safe)" "$R" \
"EXEC device=0 port=3380"
rm -rf "$SANDBOX"

# (d) operator --gpu-id 0,2 on a 4-GPU box => exec dev0 + bg dev2 only.
sandbox_nvidia_n 4
R="$(plan nvidia "--gpu-id 0,2 --address dead" "$BIN" "$DEV/none/nvidia*")"
assert_plan "(d) --gpu-id 0,2 on 4-GPU box => exec dev0 + bg dev2 (1,3 skipped)" "$R" \
"EXEC device=0 port=3380
BG device=2 port=3382"
rm -rf "$SANDBOX"

# (e) cpu variant => single exec, no multi-GPU.
sandbox_nvidia_n 4   # even with 4 nvidia GPUs visible, cpu variant => single proc
R="$(plan cpu "--backend cpu --address dead" "$BIN" "$DEV/none/nvidia*")"
assert_plan "(e) cpu variant => only exec dev0 (single process)" "$R" \
"EXEC device=0 port=3380"
rm -rf "$SANDBOX"

# ── Brick-safety structural guard on h-run.sh itself ────────────────────────────
# The EXISTING final exec line MUST remain the guaranteed-last action, with
# --device 0 added (HiveOS tracks this process = device 0 = ≥1 GPU always mines).
echo
echo "-- brick-safety: final exec line is the guaranteed last action with --device 0 --"
# The last non-blank, non-comment line of h-run.sh must be the miner exec.
LAST="$(grep -vE '^[[:space:]]*($|#)' "$HRUN" | tail -1)"
case "$LAST" in
  *'$EXTRA_FLAGS'*)
    ok "final non-comment line ends the exec (… \$EXTRA_FLAGS)" ;;
  *)
    fail "final exec line" "last non-comment line is not the miner exec tail: '$LAST'" ;;
esac
# The exec carries an explicit --device 0.
if grep -qE '^exec[[:space:]]+"\$CUSTOM_BIN"' "$HRUN" && grep -qE -- '--device 0' "$HRUN"; then
  ok "h-run.sh exec block carries --device 0"
else
  fail "exec --device 0" "exec block missing the explicit --device 0"
fi
# The background launches must be fail-soft: the multi-GPU block must NOT enable
# 'set -e' anywhere (a brick hazard). h-run.sh never uses set -e today.
if grep -qE '^[[:space:]]*set -e' "$HRUN"; then
  fail "no set -e" "h-run.sh introduced 'set -e' — a brick hazard in the multi-GPU block"
else
  ok "h-run.sh has no 'set -e' (background launches stay fail-soft)"
fi

echo
echo "========================================"
echo "  Passed: $PASS  Failed: $FAIL"
echo "========================================"
echo
[ "$FAIL" -gt 0 ] && exit 1
exit 0
