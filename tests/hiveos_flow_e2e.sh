#!/usr/bin/env bash
# tests/hiveos_flow_e2e.sh — FLOW-LEVEL e2e for the HiveOS two-call sequence.
#
# THE BUG (v0.1.17 "0 GPU hashrate"): hiveos/h-run.sh update_variant() maps ONLY
# the space-form `--backend cuda|opencl|cpu`. Everything else — a flightsheet with
# NO --backend (address only), `--backend auto`, the equals-form `--backend=cuda`,
# or a typo — falls through to `*) echo "cpu"`, so the rig fetches+runs the bundled
# CPU seed, reports "no GPU backend usable", and produces ZERO GPU hashrate even on
# a perfectly good NVIDIA rig. mine-auto.sh already solved this with detect_variant()
# (the proven GPU auto-detect, also used by the no-arg launcher); this test pins the
# port of that auto-detect into the HiveOS update_variant() fall-through.
#
# WHAT THIS REPLAYS — the REAL HiveOS sequence, two separate process invocations:
#   call 1: HiveOS runs h-config.sh WITH the flight-sheet env ($CUSTOM_TEMPLATE /
#           $CUSTOM_USER_CONFIG set) to bake config.toml + extra-flags.
#   call 2: HiveOS runs h-run.sh with those env vars UNSET; h-run.sh reloads
#           extra-flags from disk and update_variant() must resolve the right asset.
# We source update_variant() + detect_variant() out of h-run.sh under a stubbed PATH
# (mirroring tests/detect_variant.sh's resolve()) and assert the RESOLVED variant.
#
# ASSERTS THAT MUST FAIL ON CURRENT CODE (pre-fix):
#   (a) EXTRA_FLAGS='--address <40hex>' (no --backend) + stubbed nvidia => nvidia
#   (b) '--backend auto'                                 + stubbed nvidia => nvidia
#   (c) '--backend=cuda' (equals-form)                                   => nvidia
#   (d) two-call, env-stripped, keeps a non-blank address AND --backend (regression
#       guard on the existing idempotency behaviour; passes on current code).
#
# Run:  bash tests/hiveos_flow_e2e.sh
# Exit: 0 = all pass

set -uo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]:-$0}")/.." && pwd)"
HRUN="$ROOT/hiveos/h-run.sh"
HCONF="$ROOT/hiveos/h-config.sh"
ADDR40="da408d177dba334ad18c479d84eba8a0a723b7a8"   # 40-hex sample
PASS=0; FAIL=0
ok()   { echo "  [PASS] $1"; PASS=$((PASS + 1)); }
fail() { echo "  [FAIL] $1" >&2; echo "         $2" >&2; FAIL=$((FAIL + 1)); }

# Extract update_variant() AND detect_variant() from h-run.sh into one sourced file
# (update_variant calls detect_variant in its fall-through, so both must be present),
# then run update_variant under a stubbed PATH + CSD_NVIDIA_DEV_GLOB with the given
# EXTRA_FLAGS, echoing the resolved variant. Mirrors tests/detect_variant.sh:resolve().
#   $1 EXTRA_FLAGS value   $2 sandbox-bin (PATH override)   $3 nvidia-dev-glob
resolve_variant() {
  local flags="$1" binpath="$2" devglob="$3"
  : > "$SANDBOX/fn.sh"
  awk '/^update_variant\(\)[[:space:]]*\{/{f=1} f{print} f&&/^\}/{exit}' "$HRUN" >> "$SANDBOX/fn.sh"
  if ! grep -q 'update_variant' "$SANDBOX/fn.sh"; then echo "__NO_UPDATE_VARIANT__"; return; fi
  # detect_variant() is the GPU auto-detect update_variant falls through to. PRE-FIX
  # it does not yet exist in h-run.sh; provide a tiny stub then so update_variant's
  # CURRENT fall-through (`*) echo "cpu"`) runs and the test SEES the cpu trap (the
  # assertion fails with got='cpu', the faithful demonstration). POST-FIX (STEP 2)
  # h-run.sh has the real detect_variant lifted from mine-auto.sh, so we source that
  # instead and the stub is never used.
  if grep -q '^detect_variant()' "$HRUN"; then
    awk '/^detect_variant\(\)[[:space:]]*\{/{f=1} f{print} f&&/^\}/{exit}' "$HRUN" >> "$SANDBOX/fn.sh"
  else
    printf 'detect_variant() { echo "__NO_DETECT_VARIANT__"; }\n' >> "$SANDBOX/fn.sh"
  fi
  PATH="$binpath:/usr/bin:/bin" CSD_NVIDIA_DEV_GLOB="$devglob" EXTRA_FLAGS="$flags" \
    bash -c 'set -uo pipefail; source "$1"; update_variant' _ "$SANDBOX/fn.sh"
}

# Make an executable stub on the sandbox PATH.
stub() { local dir="$1" name="$2" body="$3"; printf '#!/usr/bin/env bash\n%s\n' "$body" > "$dir/$name"; chmod +x "$dir/$name"; }

# Build a sandbox that stubs an NVIDIA rig via a working nvidia-smi (the first
# detect_variant signal). Sets SANDBOX/BIN/DEV in the caller.
sandbox_nvidia() {
  SANDBOX="$(mktemp -d)"; BIN="$SANDBOX/bin"; mkdir -p "$BIN"; DEV="$SANDBOX/dev"; mkdir -p "$DEV"
  stub "$BIN" nvidia-smi 'exit 0'
}
# Build a sandbox with NO gpu signal at all (detect_variant must yield cpu).
sandbox_nogpu() {
  SANDBOX="$(mktemp -d)"; BIN="$SANDBOX/bin"; mkdir -p "$BIN"; DEV="$SANDBOX/dev"; mkdir -p "$DEV"
  stub "$BIN" lspci 'echo "00:02.0 VGA compatible controller: Intel Corporation UHD Graphics"'
}

echo
echo "=== hiveos_flow_e2e: update_variant() resolves the right asset (port of detect_variant) ==="
echo

# (a) address-only flightsheet (NO --backend) on a stubbed-NVIDIA rig => nvidia.
sandbox_nvidia
R="$(resolve_variant "--address $ADDR40" "$BIN" "$DEV/none/nvidia*")"
[ "$R" = "nvidia" ] \
  && ok "(a) address-only (no --backend) + nvidia rig => nvidia" \
  || fail "(a) address-only no-backend" "expected nvidia, got '$R'"
rm -rf "$SANDBOX"

# (b) --backend auto on a stubbed-NVIDIA rig => nvidia.
sandbox_nvidia
R="$(resolve_variant "--backend auto --address $ADDR40" "$BIN" "$DEV/none/nvidia*")"
[ "$R" = "nvidia" ] \
  && ok "(b) --backend auto + nvidia rig => nvidia" \
  || fail "(b) --backend auto" "expected nvidia, got '$R'"
rm -rf "$SANDBOX"

# (c) equals-form --backend=cuda => nvidia (must NOT fall through to cpu).
sandbox_nogpu   # no GPU signal: prove the equals-form arm wins WITHOUT auto-detect
R="$(resolve_variant "--backend=cuda --address $ADDR40" "$BIN" "$DEV/none/nvidia*")"
[ "$R" = "nvidia" ] \
  && ok "(c) --backend=cuda (equals-form) => nvidia" \
  || fail "(c) --backend=cuda" "expected nvidia, got '$R'"
rm -rf "$SANDBOX"

# Sanity: the EXPLICIT space-form must still resolve, both pre- and post-fix.
sandbox_nogpu
R="$(resolve_variant "--backend cuda --address $ADDR40" "$BIN" "$DEV/none/nvidia*")"
[ "$R" = "nvidia" ] \
  && ok "    explicit --backend cuda => nvidia (unchanged)" \
  || fail "explicit --backend cuda" "expected nvidia, got '$R'"
rm -rf "$SANDBOX"
sandbox_nogpu
R="$(resolve_variant "--backend opencl --address $ADDR40" "$BIN" "$DEV/none/nvidia*")"
[ "$R" = "amd" ] \
  && ok "    explicit --backend opencl => amd (unchanged)" \
  || fail "explicit --backend opencl" "expected amd, got '$R'"
rm -rf "$SANDBOX"

# (d) Two-call env-stripped flow: h-config WITH env bakes config+extra-flags, then
# h-run's idempotent re-render (env UNSET) must KEEP a non-blank address AND the
# --backend token in the extra-flags it reloads. This guards the existing
# idempotency behaviour end-to-end (passes on current code).
echo
echo "-- (d) two-call env-stripped flow keeps address + --backend --"
D="$(mktemp -d)"
cp "$HCONF" "$D/h-config.sh"
cp "$HRUN" "$D/h-run.sh"
# Stub manifest: CONF in the temp dir; CUSTOM_BIN points at a NON-existent binary so
# h-run.sh's re-render guard + extra-flags reload run without trying to exec/update.
printf 'CUSTOM_CONFIG_FILENAME=%s/config.toml\nCUSTOM_BIN=%s/csd-gpu-miner\nCUSTOM_API_PORT=3380\n' "$D" "$D" > "$D/h-manifest.conf"
# call 1: h-config.sh WITH the flight-sheet env (HiveOS bakes the config).
( cd "$D" && CUSTOM_TEMPLATE="" CUSTOM_USER_CONFIG="--backend cuda --address $ADDR40" bash ./h-config.sh >/dev/null 2>&1 )
# call 2: h-run.sh with the flight-sheet env UNSET. CSD_SOURCE_ONLY=1 stops h-run
# right after it reloads EXTRA_FLAGS from disk (before relay/update/exec), and we
# echo what it loaded so we can assert on it.
OUT="$( cd "$D" && unset CUSTOM_TEMPLATE CUSTOM_USER_CONFIG 2>/dev/null
        CSD_SOURCE_ONLY=1 bash -c 'source ./h-run.sh; printf "ADDR=%s\nFLAGS=%s\n" \
          "$(sed -n '"'"'s/^address = "\(.*\)"$/\1/p'"'"' "'"$D"'/config.toml")" "$EXTRA_FLAGS"' )"
GOT_ADDR="$(printf '%s\n' "$OUT" | sed -n 's/^ADDR=//p')"
GOT_FLAGS="$(printf '%s\n' "$OUT" | sed -n 's/^FLAGS=//p')"
[ "$GOT_ADDR" = "$ADDR40" ] \
  && ok "(d) env-stripped re-run keeps a non-blank 40-hex address" \
  || fail "(d) address blanked" "expected $ADDR40, got '$GOT_ADDR'"
case "$GOT_FLAGS" in
  *"--backend cuda"*) ok "(d) env-stripped re-run keeps --backend in extra-flags" ;;
  *) fail "(d) --backend dropped" "expected to contain '--backend cuda', got '$GOT_FLAGS'" ;;
esac
rm -rf "$D"

# ── STEP 3: driver-check WARNING is derived from the RESOLVED variant ───────────
# The driver check must NOT false-warn. With the warning keyed off update_variant
# (not raw EXTRA_FLAGS): a no-GPU box with an address-only / auto flightsheet
# resolves to cpu => neither arm fires => NO "GPU missing" warning. A stubbed-nvidia
# box resolves to nvidia AND nvidia-smi works => no warning either. We rebuild the
# exact resolved-variant + driver-check snippet from h-run.sh and grep its output.
#
# Extract the driver-check block (the `RESOLVED_VARIANT=...; case ... esac`) verbatim
# from h-run.sh, strip the `| tee -a "$LOG"` so it prints to stdout in the sandbox.
driver_warn() {  # $1 EXTRA_FLAGS  $2 sandbox-bin  $3 dev-glob  -> echoes WARN|OK
  local flags="$1" binpath="$2" devglob="$3"
  : > "$SANDBOX/dc.sh"
  awk '/^update_variant\(\)[[:space:]]*\{/{f=1} f{print} f&&/^\}/{exit}' "$HRUN" >> "$SANDBOX/dc.sh"
  awk '/^detect_variant\(\)[[:space:]]*\{/{f=1} f{print} f&&/^\}/{exit}' "$HRUN" >> "$SANDBOX/dc.sh"
  # The driver-check block: from `RESOLVED_VARIANT="$(update_variant)"` to its `esac`.
  awk '/^RESOLVED_VARIANT="\$\(update_variant\)"/{f=1} f{print} f&&/^esac/{exit}' "$HRUN" \
    | sed 's/ | tee -a "\$LOG"//' >> "$SANDBOX/dc.sh"
  if ! grep -q 'RESOLVED_VARIANT' "$SANDBOX/dc.sh"; then echo "__NO_DRIVER_CHECK__"; return; fi
  local out
  out="$(PATH="$binpath:/usr/bin:/bin" CSD_NVIDIA_DEV_GLOB="$devglob" EXTRA_FLAGS="$flags" LOG=/dev/null \
    bash -c 'set -uo pipefail; source "$1" 2>&1' _ "$SANDBOX/dc.sh")"
  case "$out" in *WARNING*) echo "WARN" ;; *) echo "OK" ;; esac
}

echo
echo "-- STEP 3: driver-check warning derives from resolved variant (no false warns) --"
# no GPU + no backend => resolves cpu => NO warning.
sandbox_nogpu
R="$(driver_warn "--address $ADDR40" "$BIN" "$DEV/none/nvidia*")"
[ "$R" = "OK" ] \
  && ok "no-GPU + no-backend resolves cpu => no false GPU warning" \
  || fail "no-GPU no-backend driver check" "expected OK (no warn), got '$R'"
rm -rf "$SANDBOX"

# stubbed-nvidia (working nvidia-smi) + no backend => resolves nvidia, smi OK => no warn.
sandbox_nvidia
stub "$BIN" nvidia-smi 'exit 0'   # also answers `nvidia-smi -L` (any arg) => 0
R="$(driver_warn "--address $ADDR40" "$BIN" "$DEV/none/nvidia*")"
[ "$R" = "OK" ] \
  && ok "stubbed-nvidia + no-backend resolves nvidia, smi works => no false warning" \
  || fail "nvidia no-backend driver check" "expected OK (no warn), got '$R'"
rm -rf "$SANDBOX"

# Negative control: explicit --backend cuda but NO nvidia-smi => genuine mismatch => WARN.
sandbox_nogpu
R="$(driver_warn "--backend cuda --address $ADDR40" "$BIN" "$DEV/none/nvidia*")"
[ "$R" = "WARN" ] \
  && ok "explicit --backend cuda with no nvidia-smi => WARNING (genuine mismatch still flagged)" \
  || fail "cuda-no-driver mismatch" "expected WARN, got '$R'"
rm -rf "$SANDBOX"

echo
echo "========================================"
echo "  Passed: $PASS  Failed: $FAIL"
echo "========================================"
echo
[ "$FAIL" -gt 0 ] && exit 1
exit 0
