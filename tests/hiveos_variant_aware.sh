#!/usr/bin/env bash
# tests/hiveos_variant_aware.sh — VARIANT-AWARE startup swap (the 15-day fleet bug).
#
# THE BUG: a fresh HiveOS install bundles ONE prebuilt binary (the opencl/amd
# build, due to the release-packaging overwrite bug). Its --version already equals
# "latest", so h-run.sh's VERSION-only update gate never fires, and an NVIDIA rig
# stays stuck on the opencl binary forever ("cuda=false opencl=true" under
# --backend cuda). The card can't use CUDA.
#
# THE FIX (under test): h-run.sh now determines the INSTALLED variant (from the
# binary's own `devices` self-report, marker file as fallback) and the REQUESTED
# variant (update_variant() from --backend); if they DIFFER it fetches+verifies+
# swaps the correct variant EVEN WHEN the version matches. Still fail-closed
# SHA-verified and brick-safe: a fetch/verify failure keeps the working binary.
#
# We source h-run.sh with CSD_SOURCE_ONLY=1 (its test hook: defines all functions,
# then returns BEFORE the startup check / sidecar / relay / exec) and drive the
# REAL functions, stubbing only the leaf I/O (the binary's `devices` output, the
# download, and the SHA verifier) so we test the actual control flow on this box.
#
# Run:  bash tests/hiveos_variant_aware.sh
# Exit: 0 = all pass
set -u

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]:-$0}")/.." && pwd)"
HRUN="$ROOT/hiveos/h-run.sh"
fails=0
ok()  { printf '  PASS: %s\n' "$*"; }
bad() { printf '  FAIL: %s\n' "$*"; fails=$((fails+1)); }

# Build a fresh sandbox each scenario: a fake $UPDATE_BIN whose `devices`
# subcommand prints the build-features line for a chosen variant, plus a directory
# for the marker. We point CUSTOM_BIN at it and source h-run.sh.
#
# $1 = installed variant the FAKE binary self-reports (nvidia|amd|cpu|none)
#       "none" => the binary does NOT print a build-features line (old binary).
make_fake_binary() {
  local variant="$1" dir="$2"
  local cuda opencl
  case "$variant" in
    nvidia) cuda=true;  opencl=false ;;
    amd)    cuda=false; opencl=true  ;;
    cpu)    cuda=false; opencl=false ;;
    none)   cuda="";    opencl=""    ;;
  esac
  cat > "$dir/csd-gpu-miner" <<EOF
#!/usr/bin/env bash
case "\$1" in
  devices)
    $( [ "$variant" != none ] && echo "echo \"build features: cuda=$cuda opencl=$opencl\"" )
    echo "=== csd-gpu-miner devices ==="
    exit 0 ;;
  --version) echo "csd-gpu-miner 0.1.15" ;;   # already == latest (the bug's premise)
  verify-file)
    # \$2=help → support probe; \$2=path \$3=sha → verify. Our stub: a file whose
    # FIRST LINE equals the wanted sha "verifies". Lets the test force OK/FAIL.
    if [ "\$2" = "--help" ]; then exit 0; fi
    want="\$3"; got="\$(head -n1 "\$2" 2>/dev/null)"
    [ "\$got" = "\$want" ] && exit 0 || exit 1 ;;
  check-update) exit 1 ;;   # never "newer" by version (forces variant path only)
  *) exit 0 ;;
esac
EOF
  chmod +x "$dir/csd-gpu-miner"
}

# Source h-run.sh into THIS shell with the test hook, pointing at our fake binary.
# Returns with every ua_* / hive_* function defined and ready to call.
load_hrun() {
  local dir="$1"
  CSD_SOURCE_ONLY=1 CUSTOM_BIN="$dir/csd-gpu-miner" \
  CUSTOM_CONFIG_FILENAME="$dir/config.toml" \
  CUSTOM_LOG_BASENAME="$dir/miner" \
    . "$HRUN"
}

# ──────────────────────────────────────────────────────────────────────────────
echo
echo "=== variant-aware startup swap (h-run.sh) ==="
echo

# ── (1) ua_binary_variant reads the binary's own `devices` self-report ────────
for V in nvidia amd cpu; do
  WORK="$(mktemp -d)"; make_fake_binary "$V" "$WORK"
  ( load_hrun "$WORK"
    r="$(ua_binary_variant)"
    [ "$r" = "$V" ] && echo "OKVAR" || echo "GOT:$r" ) | grep -q OKVAR \
    && ok "ua_binary_variant: cuda/opencl flags => $V" \
    || bad "ua_binary_variant for $V mis-detected"
  rm -rf "$WORK"
done

# ── (2) old binary (no build-features line): ua_binary_variant empty, marker wins
WORK="$(mktemp -d)"; make_fake_binary none "$WORK"
printf 'amd\n' > "$WORK/.installed-variant"
( load_hrun "$WORK"
  bv="$(ua_binary_variant)"; iv="$(ua_installed_variant)"
  [ -z "$bv" ] && [ "$iv" = "amd" ] && echo OKFALL || echo "bv=$bv iv=$iv" ) | grep -q OKFALL \
  && ok "no self-report => ua_binary_variant empty; ua_installed_variant falls back to marker" \
  || bad "marker fallback path broken"
rm -rf "$WORK"

# ── (3) THE BUG: installed=amd, requested=cuda(nvidia), version EQUAL ──────────
#        ua_download_verify_swap must be CALLED (variant trigger), fetch the
#        nvidia asset, verify, swap, and write the marker = nvidia.
WORK="$(mktemp -d)"; make_fake_binary amd "$WORK"
NVSHA="deadbeefnvidiasha"                       # pretend nvidia asset digest
(
  load_hrun "$WORK"
  EXTRA_FLAGS="--backend cuda"                  # flightsheet requests nvidia
  # Stub the network leaves: download writes a file whose 1st line = NVSHA (so the
  # fake binary's verify-file "passes"); expected-sha returns NVSHA for the nvidia
  # asset. The on-disk (amd) binary's 1st line is the shebang, so the same-binary
  # guard (rc=2) does NOT trigger — a real swap happens.
  ua_download() { printf '%s\n' "$NVSHA" > "$2"; return 0; }
  ua_expected_sha() { echo "$NVSHA"; }
  reason=""; want_var="$(update_variant)"; have_var="$(ua_installed_variant)"
  [ "$have_var" != "$want_var" ] && reason=variant
  echo "REASON:$reason WANT:$want_var HAVE:$have_var"
  ua_download_verify_swap; rc=$?
  echo "RC:$rc"
  echo "MARKER:$(cat "$WORK/.installed-variant" 2>/dev/null)"
  # The swapped-in binary is our staged file: its 1st line is NVSHA now.
  echo "ONDISK1:$(head -n1 "$WORK/csd-gpu-miner")"
) > "$WORK/out" 2>&1
grep -q 'REASON:variant WANT:nvidia HAVE:amd' "$WORK/out" \
  && ok "variant trigger fires: installed amd, requested nvidia, version equal" \
  || { bad "variant trigger did NOT fire"; cat "$WORK/out"; }
grep -q 'RC:0' "$WORK/out" \
  && ok "ua_download_verify_swap performed the swap (rc=0)" \
  || { bad "swap did not happen"; cat "$WORK/out"; }
grep -q 'MARKER:nvidia' "$WORK/out" \
  && ok "marker updated to nvidia after swap" || bad "marker not updated to nvidia"
grep -q "ONDISK1:$NVSHA" "$WORK/out" \
  && ok "on-disk binary replaced by the nvidia asset" || bad "binary not swapped"
rm -rf "$WORK"

# ── (4) BRICK-SAFETY: variant mismatch but the fetch FAILS => no swap, binary kept
WORK="$(mktemp -d)"; make_fake_binary amd "$WORK"
printf 'amd-seed-line\n' > "$WORK/seedmark"     # remember original 1st line target
ORIG1="$(head -n1 "$WORK/csd-gpu-miner")"
(
  load_hrun "$WORK"
  EXTRA_FLAGS="--backend cuda"
  ua_download() { return 1; }                   # network down / 404 for BOTH variant and cpu fallback
  ua_expected_sha() { echo "deadbeef"; }
  ua_download_verify_swap; echo "RC:$?"
) > "$WORK/out" 2>&1
grep -q 'RC:1' "$WORK/out" \
  && ok "fetch failure => ua_download_verify_swap returns 1 (no swap)" \
  || { bad "expected rc=1 on fetch failure"; cat "$WORK/out"; }
NOW1="$(head -n1 "$WORK/csd-gpu-miner")"
[ "$NOW1" = "$ORIG1" ] \
  && ok "BRICK-SAFE: on-disk binary UNCHANGED after a failed variant fetch — still mines" \
  || bad "binary was modified despite a failed fetch!"
rm -rf "$WORK"

# ── (5) BRICK-SAFETY: variant mismatch, fetch OK but SHA verify FAILS => kept ──
WORK="$(mktemp -d)"; make_fake_binary amd "$WORK"
ORIG1="$(head -n1 "$WORK/csd-gpu-miner")"
(
  load_hrun "$WORK"
  EXTRA_FLAGS="--backend cuda"
  ua_download() { printf 'WRONGBYTES\n' > "$2"; return 0; }  # 1st line != wanted sha
  ua_expected_sha() { echo "expected-nvidia-sha"; }          # mismatch → verify fails
  ua_download_verify_swap; echo "RC:$?"
) > "$WORK/out" 2>&1
grep -q 'RC:1' "$WORK/out" \
  && ok "SHA mismatch => returns 1 (fail-closed, no swap)" \
  || { bad "expected rc=1 on SHA mismatch"; cat "$WORK/out"; }
[ "$(head -n1 "$WORK/csd-gpu-miner")" = "$ORIG1" ] \
  && ok "BRICK-SAFE: binary UNCHANGED after a failed SHA verify" \
  || bad "binary swapped despite SHA mismatch!"
rm -rf "$WORK"

# ── (6) MATCH: installed variant already == requested => NO swap attempted ─────
#        (no churn: an NVIDIA rig already on the nvidia build must not re-download)
WORK="$(mktemp -d)"; make_fake_binary nvidia "$WORK"
(
  load_hrun "$WORK"
  EXTRA_FLAGS="--backend cuda"
  want_var="$(update_variant)"; have_var="$(ua_installed_variant)"
  if [ -n "$have_var" ] && [ "$have_var" != "$want_var" ]; then echo "WOULD_SWAP"; else echo "NO_SWAP"; fi
) > "$WORK/out" 2>&1
grep -q 'NO_SWAP' "$WORK/out" \
  && ok "installed nvidia + requested nvidia => no swap (no churn)" \
  || { bad "would needlessly swap a correct binary"; cat "$WORK/out"; }
rm -rf "$WORK"

# ── (7) UNKNOWN installed variant (old binary, no marker) => never swaps ───────
#        We cannot prove a mismatch, so leave it mining (fail-safe, no churn).
WORK="$(mktemp -d)"; make_fake_binary none "$WORK"   # no marker written
(
  load_hrun "$WORK"
  EXTRA_FLAGS="--backend cuda"
  have_var="$(ua_installed_variant)"; want_var="$(update_variant)"
  if [ -n "$have_var" ] && [ "$have_var" != "$want_var" ]; then echo "WOULD_SWAP"; else echo "NO_SWAP have=[$have_var]"; fi
) > "$WORK/out" 2>&1
grep -q 'NO_SWAP have=\[\]' "$WORK/out" \
  && ok "unknown installed variant => no swap (can't prove mismatch; fail-safe)" \
  || { bad "swapped on an UNKNOWN installed variant"; cat "$WORK/out"; }
rm -rf "$WORK"

echo
if [ "$fails" -eq 0 ]; then echo "ALL VARIANT-AWARE ASSERTIONS PASSED"; exit 0
else echo "VARIANT-AWARE FAILURES: $fails"; exit 1; fi
