#!/usr/bin/env bash
set -euo pipefail

# --- What this is -------------------------------------------------------
# Opt-in CSD miner launcher: mines on THIS machine, to YOUR own payout
# address, only while you choose to run it. Not silent or hidden, and does
# not install or run itself on anyone else's computer. Standard pool miner
# for the public Compute Substrate (CSD) chain. See README "What this is".
# ------------------------------------------------------------------------

# ============================================================
#  Runs ONE miner instance per GPU for the biggest combined
#  hashrate. Each instance mines the SAME payout address on a
#  different --device; the pool sums their shares.
#  Default = the OpenCL ("amd") build, which drives NVIDIA and
#  AMD GPUs with just the vendor driver (no CUDA toolkit needed).
#  Use the CUDA build instead with:  ./mine-all-gpus.sh nvidia
#
#  AUTO-UPDATE (fleet, no clawback — FAIL-SAFE IS RULE #1):
#  This launcher now also keeps itself current. A background poll
#  checks GitHub every CHECK_MIN minutes; a newer release is gated
#  through the SAME three checks mine-auto.sh uses — numeric semver
#  compare (the binary's own `check-update`), download to a TEMP
#  path, and SHA-256 verify against the release SHA256SUMS — BEFORE
#  an atomic swap. Only after a verified swap are the per-GPU miners
#  restarted on the new binary. ANY update failure (no network,
#  rate-limit, SHA mismatch, partial download, disk full) falls
#  through and the EXISTING per-GPU miners keep running untouched.
#  Stop everything with Ctrl+C.
#
#  Env knobs (all optional):
#     CHECK_MIN   update-poll period in minutes   (default 15)
# ============================================================

REPO="dangraagu/CSD-Mining-pool-public"

# --- Shared GPU auto-detection (identical in install-csd-miner.sh /
# mine-auto.sh). Returns nvidia | amd | cpu. NVIDIA wins on ANY of three
# independent signals so a driver-only / container box (nvidia-smi may be absent,
# but the device nodes and/or libcuda.so are present) is correctly detected as
# nvidia, not amd/cpu:
#   1. nvidia-smi exists AND runs,
#   2. an NVIDIA device node exists (/dev/nvidiactl or /dev/nvidia* —
#      CSD_NVIDIA_DEV_GLOB overrides the glob for testing),
#   3. ldconfig lists libcuda.so on the loader path.
# Only if NONE hold do we consider AMD/OpenCL (lspci or clinfo), then cpu.
detect_variant() {
  local glob="${CSD_NVIDIA_DEV_GLOB:-/dev/nvidia*}"
  if command -v nvidia-smi >/dev/null 2>&1 && nvidia-smi >/dev/null 2>&1; then
    echo nvidia; return
  fi
  if [ -e /dev/nvidiactl ] || compgen -G "$glob" >/dev/null 2>&1; then
    echo nvidia; return
  fi
  if command -v ldconfig >/dev/null 2>&1 && ldconfig -p 2>/dev/null | grep -q 'libcuda\.so'; then
    echo nvidia; return
  fi
  if { command -v lspci >/dev/null 2>&1 && lspci 2>/dev/null | grep -Eiq '\[AMD/ATI\]|Advanced Micro Devices|Radeon|\bATI\b'; } \
     || { command -v clinfo >/dev/null 2>&1 && clinfo 2>/dev/null | grep -Eiq 'Advanced Micro Devices|Radeon|\bAMD\b'; }; then
    echo amd; return
  fi
  echo cpu
}

# Build variant: explicit arg wins; otherwise auto-detect (NOT a hard amd default,
# which ran the amd build on NVIDIA rigs launched with no arg).
VARIANT="${1:-}"
[ -z "$VARIANT" ] && VARIANT="$(detect_variant)"
case "$VARIANT" in
  nvidia|amd|cpu) ;;
  *) echo "[X] Unknown build '$VARIANT'. Use one of: nvidia | amd | cpu" >&2; exit 1 ;;
esac

DATA_DIR="${XDG_DATA_HOME:-$HOME/.local/share}/csd-pool-miner"
CFG_DIR="${XDG_CONFIG_HOME:-$HOME/.config}/csd-pool-miner"
CFG="$CFG_DIR/address.txt"
CHECK_MIN="${CHECK_MIN:-15}"
mkdir -p "$DATA_DIR" "$CFG_DIR"

echo
echo " === CSD Pool Miner - all GPUs + auto-update (build: $VARIANT) ==="
echo

# Download $1 -> $2 atomically: fetch to a temp file and only move it into
# place on success, so a failed/partial download never leaves a 0-byte binary
# that later gets chmod+x'd and exec'd. Returns non-zero on failure.
download() {
  local url="$1" out="$2" tmp
  tmp="$out.tmp"
  if command -v curl >/dev/null 2>&1; then
    curl -fL --retry 3 -o "$tmp" "$url" && mv "$tmp" "$out"
  elif command -v wget >/dev/null 2>&1; then
    wget -O "$tmp" "$url" && mv "$tmp" "$out"
  else
    echo "[X] Neither 'curl' nor 'wget' is installed." >&2
    return 1
  fi
}

# Latest published version, or empty string on any failure.
#
# FIX #8: resolve it from the releases/latest/download/ CDN asset
# latest-version.txt instead of api.github.com (unauthenticated cap = 60
# req/hr/IP, so a farm behind one public IP gets 403 + an empty tag + a silently
# frozen fleet). The CDN download path has no per-IP limit. Bare version, no
# leading 'v'. Empty on offline/404 → caller no-ops (keeps mining); we never fall
# back to the rate-limited API.
latest_tag() {
  local url="https://github.com/$REPO/releases/latest/download/latest-version.txt" out
  if command -v curl >/dev/null 2>&1; then
    out="$(curl -fsSL -H 'User-Agent: csd-miner' "$url" 2>/dev/null)" || return 0
  elif command -v wget >/dev/null 2>&1; then
    out="$(wget -qO- --header='User-Agent: csd-miner' "$url" 2>/dev/null)" || return 0
  else
    return 0
  fi
  out="$(printf '%s\n' "$out" | sed -e 's/[[:space:]]//g' -e '/^$/d' | head -n1)"
  printf '%s' "${out#v}"
}

# Decide whether $2 (latest) is newer than $1 (installed). Prefer the miner's
# OWN `check-update` (one tested numeric semver compare: 0.1.10 > 0.1.9). If no
# usable binary exists yet, fall back to a plain string inequality. Returns 0
# (update) / non-zero (skip).
should_update() {
  local installed="$1" latest="$2"
  if [ -x "$BIN" ] && "$BIN" check-update --current "$installed" --latest "$latest" >/dev/null 2>&1; then
    return 0
  fi
  if [ -x "$BIN" ] && "$BIN" check-update --help >/dev/null 2>&1; then
    return 1   # subcommand present, exited non-zero == up-to-date/older
  fi
  # String fallback: strip a leading 'v' from BOTH sides so a bare installed
  # version ("0.1.10") is not seen as different from a 'v'-prefixed release tag
  # ("v0.1.10"), which would re-download needlessly every poll.
  [ "${installed#v}" != "${latest#v}" ]
}

# Expected SHA-256 hex for asset $1 from the release SHA256SUMS (empty => no
# checksums published or asset not listed → caller treats empty as "cannot
# verify"). Line format: "<hex>  <filename>" (sha256sum style).
expected_sha() {
  local asset="$1" sums
  sums="$DATA_DIR/SHA256SUMS.allgpus.tmp"
  if download "https://github.com/$REPO/releases/latest/download/SHA256SUMS" "$sums" 2>/dev/null; then
    awk -v a="$asset" '$2==a || $2=="*"a {print $1; exit}' "$sums"
    rm -f "$sums"
  fi
}

# Download the latest $VARIANT build, VERIFY it, and only then atomically swap
# it onto $BIN. NEVER writes an unverified binary onto the live path. On a
# non-cpu 404, falls back to the cpu asset (and updates VARIANT/BIN_NAME/BIN).
# Returns non-zero (leaving the running binary untouched) if no verified binary
# could be staged — so the caller always keeps the current miners running.
download_verify_swap() {
  local staged="$BIN.new" want
  if ! download "https://github.com/$REPO/releases/latest/download/$BIN_NAME" "$staged"; then
    if [ "$VARIANT" != "cpu" ]; then
      echo "[!] '$VARIANT' build unavailable (download failed / 404). Falling back to the cpu build." >&2
      VARIANT="cpu"; BIN_NAME="csd-pool-miner-linux-cpu"; BIN="$DATA_DIR/$BIN_NAME"; staged="$BIN.new"
      download "https://github.com/$REPO/releases/latest/download/$BIN_NAME" "$staged" || { rm -f "$staged"; return 1; }
    else
      rm -f "$staged"; return 1
    fi
  fi

  want="$(expected_sha "$BIN_NAME")"
  if [ -z "$want" ]; then
    # FIX #9: FAIL CLOSED. Live releases (v0.1.7+) always publish SHA256SUMS, so a
    # missing SHA256SUMS (or our asset not listed) is anomalous — refuse the
    # update and keep the EXISTING binary rather than swapping in an unverified
    # download. (Previously this accepted the download: a fail-OPEN hole.)
    echo "[X] refusing unverified update: no SHA256SUMS published (or '$BIN_NAME' not listed in it). Keeping the running binary." >&2
    rm -f "$staged"; return 1
  else
    # Verify with a TRUSTED tool ONLY: the already-running $BIN's verify-file,
    # else the OS sha256sum. NEVER let the just-downloaded file verify itself.
    # With a digest but no trusted verifier → FAIL CLOSED.
    if [ -x "$BIN" ] && "$BIN" verify-file --help >/dev/null 2>&1; then
      if ! "$BIN" verify-file "$staged" "$want" >/dev/null 2>&1; then
        echo "[X] SHA-256 verify FAILED for the downloaded $BIN_NAME - discarding it and keeping the running binary." >&2
        rm -f "$staged"; return 1
      fi
    elif command -v sha256sum >/dev/null 2>&1; then
      local got
      got="$(sha256sum "$staged" | awk '{print $1}')"
      if [ "$got" != "$want" ]; then
        echo "[X] SHA-256 verify FAILED for the downloaded $BIN_NAME (got $got, want $want) - discarding it." >&2
        rm -f "$staged"; return 1
      fi
    else
      echo "[X] have a SHA256SUMS digest but no trusted verifier (no running verify-file, no sha256sum) - refusing the update." >&2
      rm -f "$staged"; return 1
    fi
  fi

  chmod +x "$staged"
  mv "$staged" "$BIN"   # atomic swap onto the live path, only after verify
}

# Installed version string (for the semver compare); "none" if unreadable.
installed_version() {
  local v
  if [ -x "$BIN" ]; then
    v="$("$BIN" --version 2>/dev/null | grep -Eo '[0-9]+\.[0-9]+\.[0-9]+' | head -n1)"
    [ -n "$v" ] && { echo "$v"; return; }
  fi
  echo "none"
}

# --- 1. download the latest binary (verified) ------------------------------
# The amd/OpenCL asset is built continue-on-error and may be missing from a
# release; if the requested variant 404s, download_verify_swap falls back to the
# cpu build, which is always published. We verify BEFORE running it.
BIN_NAME="csd-pool-miner-linux-$VARIANT"
BIN="$DATA_DIR/$BIN_NAME"
echo "Downloading + verifying $BIN_NAME ..."
if ! download_verify_swap; then
  # FAIL-SAFE: if we already have a usable binary from a previous run, mine on
  # it rather than aborting; only hard-fail when there is nothing to run.
  if [ -x "$BIN" ]; then
    echo "[!] Could not fetch/verify a fresh build; using the previously installed binary at $BIN." >&2
  else
    echo "[X] Download/verify failed and no usable binary is present. (No release yet, or no network.)" >&2
    echo "    Releases: https://github.com/$REPO/releases/latest" >&2
    exit 1
  fi
fi
[ -x "$BIN" ] || chmod +x "$BIN" 2>/dev/null || true

# --- 2. payout address (reuse the saved one, else prompt) ------------------
ADDR=""
if [ -f "$CFG" ]; then
  ADDR="$(tr -d '[:space:]' < "$CFG")"
fi
if [ -z "$ADDR" ]; then
  printf 'Enter your addr20 payout address (40 hex): '
  read -r ADDR
  ADDR="$(printf '%s' "$ADDR" | tr -d '[:space:]')"
fi
ADDR="$(printf '%s' "$ADDR" | tr '[:upper:]' '[:lower:]')"
ADDR="${ADDR#0x}"
if ! printf '%s' "$ADDR" | grep -Eq '^[0-9a-f]{40}$'; then
  echo "[X] '$ADDR' is not a valid addr20 (need 40 hex characters)." >&2
  exit 1
fi
printf '%s\n' "$ADDR" > "$CFG"

# --- 3. count GPUs ---------------------------------------------------------
# nvidia: count "GPU N:" lines from nvidia-smi -L.
# OpenCL: count only GPU devices ('Device Type ... GPU'); a plain 'Device Type'
# match overcounts because clinfo also lists CPU OpenCL devices and repeats the
# field per platform.
NGPU=0
if [ "$VARIANT" = "nvidia" ] && command -v nvidia-smi >/dev/null 2>&1; then
  NGPU="$(nvidia-smi -L 2>/dev/null | grep -c '^GPU ' || true)"
elif command -v nvidia-smi >/dev/null 2>&1 && nvidia-smi -L >/dev/null 2>&1; then
  NGPU="$(nvidia-smi -L 2>/dev/null | grep -c '^GPU ' || true)"
elif command -v clinfo >/dev/null 2>&1; then
  NGPU="$(clinfo 2>/dev/null | grep -c 'Device Type.*GPU' || true)"
fi
# Normalise: must be a positive integer; default to 1.
case "$NGPU" in
  ''|*[!0-9]*) NGPU=1 ;;
esac
[ "$NGPU" -lt 1 ] && NGPU=1
echo "Detected $NGPU GPU(s). Launching one miner per GPU to $ADDR ..."
echo "Auto-checking GitHub for updates every $CHECK_MIN min (verified swap, then restart). Keep this running."
echo

# --- 4. spawn one instance per GPU device (0 .. NGPU-1) --------------------
PIDS=()

start_all() {
  PIDS=()
  local i LOGDIR LAST
  LAST=$((NGPU - 1))
  for i in $(seq 0 "$LAST"); do
    LOGDIR="$DATA_DIR/gpu${i}-log"
    mkdir -p "$LOGDIR"
    "$BIN" --address "$ADDR" --device "$i" --log-dir "$LOGDIR" \
      > "$LOGDIR/stdout.log" 2>&1 &
    PIDS+=("$!")
  done
}

stop_all() {
  if [ "${#PIDS[@]}" -gt 0 ]; then
    kill "${PIDS[@]}" 2>/dev/null || true
    wait "${PIDS[@]}" 2>/dev/null || true
  fi
  pkill -f "$BIN_NAME" 2>/dev/null || true
  PIDS=()
}

# Clean shutdown on Ctrl+C / TERM (also stops the per-GPU miners).
cleanup() {
  echo
  echo "Stopping miners ..."
  stop_all
  exit 0
}
trap cleanup INT TERM

start_all
echo "Launched $NGPU miner process(es), one per GPU, all mining to $ADDR."
echo "PIDs: ${PIDS[*]}"
echo "Per-GPU logs are under: $DATA_DIR/gpu<i>-log/"
echo

# --- 5. auto-update poll loop ----------------------------------------------
# Foreground loop: every CHECK_MIN minutes, check for a newer release and, when
# one verifies, atomically swap it in and restart ALL per-GPU miners on it. The
# verify+swap is the same fail-safe path used at startup, so a failed update
# never disturbs the running miners. (Liveness/crash-loop handling is the job of
# mine-auto.sh; this launcher's job is multi-GPU + staying current.)
INSTALLED="$(installed_version)"
while true; do
  sleep "$((CHECK_MIN * 60))"
  latest="$(latest_tag || true)"
  if [ -n "$latest" ] && should_update "$INSTALLED" "$latest"; then
    echo "[$(date '+%H:%M:%S')] update: $INSTALLED -> $latest  (verify, then swap + restart all GPUs)"
    if download_verify_swap; then
      stop_all
      start_all
      INSTALLED="$(installed_version)"
      echo "[$(date '+%H:%M:%S')] now mining $INSTALLED on $NGPU GPU(s) (build: $VARIANT). PIDs: ${PIDS[*]}"
    else
      echo "[$(date '+%H:%M:%S')] update not applied (download/verify failed); keeping the running miners."
    fi
  fi
done
