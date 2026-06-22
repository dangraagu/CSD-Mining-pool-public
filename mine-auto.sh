#!/usr/bin/env bash
set -euo pipefail

# --- What this is -------------------------------------------------------
# Opt-in CSD miner launcher: mines on THIS machine, to YOUR own payout
# address, only while you choose to run it. Not silent or hidden, and does
# not install or run itself on anyone else's computer. Standard pool miner
# for the public Compute Substrate (CSD) chain. See README "What this is".
# ------------------------------------------------------------------------

# ============================================================
#  Self-updating, multi-GPU launcher. Leave this running.
#   * Runs one miner instance per GPU (each --device i, all to
#     your address) for the biggest combined hashrate.
#   * Checks GitHub for the latest release every CHECK_MIN
#     minutes. A new version is gated through THREE checks before
#     it ever runs (P4 hardening):
#       1. semver compare (the miner's own `check-update`, so
#          0.1.10 is correctly newer than 0.1.9 — a string "!="
#          got this wrong),
#       2. download to a TEMP path (never onto the live binary),
#       3. SHA-256 verify against the release SHA256SUMS (the
#          miner's own `verify-file`) BEFORE the atomic swap.
#     A failed verify discards the temp and keeps the running
#     binary; the rig never executes an unverified download.
#   * Liveness is checked on a SHORT cadence (LIVE_SEC), decoupled
#     from the slow update poll, with ESCALATING BACKOFF so a
#     crash-looping rig doesn't hammer (5s,15s,60s capped). After
#     MAX_RESTARTS rapid restarts it backs off and (optionally)
#     runs your CSD_ON_CRASH hook.
#  Build (default OpenCL/amd = NVIDIA+AMD on just the driver):
#     ./mine-auto.sh nvidia
#  Stop everything: Ctrl+C (this also stops the miners).
#
#  Env knobs (all optional):
#     CHECK_MIN     update-poll period in minutes        (default 15)
#     LIVE_SEC      liveness-check period in seconds      (default 30)
#     MAX_RESTARTS  rapid restarts before backing off     (default 5)
#     CSD_GPU_IDS   comma list of GPU ids to mine, e.g.
#                   "0,2" to skip card 1 (default: all cards)
#     CSD_ON_CRASH  path to a script run once when the
#                   restart cap is hit (driver reset, etc.)
# ============================================================

REPO="dangraagu/CSD-Mining-pool-public"

VARIANT="${1:-amd}"
case "$VARIANT" in
  nvidia|amd|cpu) ;;
  *) echo "[X] Unknown build '$VARIANT'. Use one of: nvidia | amd | cpu" >&2; exit 1 ;;
esac

DATA_DIR="${XDG_DATA_HOME:-$HOME/.local/share}/csd-pool-miner"
CFG_DIR="${XDG_CONFIG_HOME:-$HOME/.config}/csd-pool-miner"
CFG="$CFG_DIR/address.txt"
BIN_NAME="csd-pool-miner-linux-$VARIANT"
BIN="$DATA_DIR/$BIN_NAME"
CHECK_MIN="${CHECK_MIN:-15}"
LIVE_SEC="${LIVE_SEC:-30}"
MAX_RESTARTS="${MAX_RESTARTS:-5}"
mkdir -p "$DATA_DIR" "$CFG_DIR"

# ── SP2: csd-relay-node paths ─────────────────────────────────────────────────
# The relay binary lives alongside the miner binary in DATA_DIR; it is downloaded
# as a standalone release asset (or extracted from the HiveOS tarball). The relay
# runs resource-capped (nice/ionice) so it never starves the GPU miner.
#
# SP2 relay-node launch args (REAL flags + seeds confirmed against binary):
#   --rpc             127.0.0.1:18645         local RPC (SP1.2 anchor config; REAL flag)
#   --datadir         $HOME/.local/share/csd-relay  relay chain data dir
#   --peer-seeds      <comma-sep multiaddrs>  well-known peers (confirmed real flag)
#   CSD_RELAY_BLACKLIST_ADDR20 env            path to addr20 blacklist file (node writes it)
#   CSD_BLACKLIST_URL env                     signed-blacklist source; ENABLES the node's built-in
#                                             15-min Ed25519-signed fetcher (pull→verify→write fail-closed)
#   --p2p-listen      /ip4/0.0.0.0/tcp/18644 p2p listen (multiaddr; confirmed real flag)
#
# WALLET: the relay node requires a non-zero --wallet (the binary hard-rejects
# zero/absent wallets). The relay never mines (no bridge polls it), so the wallet
# is just a placeholder. If the wallet file is absent, we generate one below
# before launch using:
#   <relay-bin> wallet new --out <path>
# wallet-new subcommand confirmed against the release relay binary.
#
RELAY_BIN_NAME="csd-relay-node"
RELAY_BIN="$DATA_DIR/$RELAY_BIN_NAME"
RELAY_DATADIR="${XDG_DATA_HOME:-$HOME/.local/share}/csd-relay"
RELAY_WALLET="$DATA_DIR/relay-wallet.json"
RELAY_BLACKLIST="$CFG_DIR/relay-blacklist.txt"
RELAY_LOG="$DATA_DIR/relay.log"
RELAY_PID=0   # tracked PID of the background relay process; 0 = not running
# Required by the `node` subcommand (binary rejects its absence). The relay is a
# follower and never mines, so an empty fanout list is correct.
RELAY_PUSH_PEERS="$DATA_DIR/relay-push-peers.txt"
RELAY_WALLET_CMD=("$RELAY_BIN" wallet new --out "$RELAY_WALLET")
# ── end SP2 constants ─────────────────────────────────────────────────────────

echo
echo " === CSD Pool Miner - auto-update (build: $VARIANT) ==="
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

# Resolve the latest published version (empty string on any failure).
#
# FIX #8: fetch it from the releases/latest/download/ CDN asset latest-version.txt
# — NOT api.github.com/.../releases/latest. The unauthenticated API is capped at
# 60 req/hr/IP, so ~20 rigs behind ONE public IP (a farm) get HTTP 403, an empty
# tag, and the whole farm SILENTLY stops updating. The CDN download path has no
# such per-IP limit. Output is the bare version (e.g. "0.1.10", no leading 'v').
# On offline/404 we return empty and the caller cleanly no-ops (keeps mining) —
# we deliberately do NOT fall back to the rate-limited API.
latest_tag() {
  local url="https://github.com/$REPO/releases/latest/download/latest-version.txt" out
  if command -v curl >/dev/null 2>&1; then
    out="$(curl -fsSL -H 'User-Agent: csd-miner' "$url" 2>/dev/null)" || return 0
  elif command -v wget >/dev/null 2>&1; then
    out="$(wget -qO- --header='User-Agent: csd-miner' "$url" 2>/dev/null)" || return 0
  else
    return 0
  fi
  # First non-empty line, whitespace-stripped, with any leading 'v' removed.
  out="$(printf '%s\n' "$out" | sed -e 's/[[:space:]]//g' -e '/^$/d' | head -n1)"
  printf '%s' "${out#v}"
}

# Decide whether $LATEST is newer than $INSTALLED. Prefer the miner's OWN
# `check-update` subcommand (one tested semver compare: 0.1.10 > 0.1.9), so the
# shell does not re-implement a fragile string compare. If the currently-
# installed binary is missing or too old to support the subcommand
# (chicken-and-egg on the very first hardened update), fall back to a plain
# string inequality. Returns 0 (update) / non-zero (skip).
should_update() {
  local installed="$1" latest="$2"
  if [ -x "$BIN" ] && "$BIN" check-update --current "$installed" --latest "$latest" >/dev/null 2>&1; then
    return 0   # subcommand present and says: newer
  fi
  # Subcommand present but exited non-zero == up-to-date/older: do NOT update.
  if [ -x "$BIN" ] && "$BIN" check-update --help >/dev/null 2>&1; then
    return 1
  fi
  # No usable binary yet (first run) or it predates check-update: string fallback.
  [ "$installed" != "$latest" ]
}

# Fetch SHA256SUMS for the latest release and echo the expected hex digest for
# $1 (the asset basename). Empty output => no checksums published (older
# release) OR the asset isn't listed; the caller treats empty as "cannot
# verify". The SHA256SUMS line format is `<hex>  <filename>` (sha256sum style).
expected_sha() {
  local asset="$1" sums
  sums="$DATA_DIR/SHA256SUMS.tmp"
  if download "https://github.com/$REPO/releases/latest/download/SHA256SUMS" "$sums" 2>/dev/null; then
    # Match the exact basename in the second field; print the first field (hex).
    awk -v a="$asset" '$2==a || $2=="*"a {print $1; exit}' "$sums"
    rm -f "$sums"
  fi
}

# Download the latest $VARIANT build, VERIFY it, and only then atomically swap
# it into $BIN. Never writes an unverified binary onto the live path:
#   1. download to a staging path "$BIN.new",
#   2. look up the expected SHA-256 from the release SHA256SUMS,
#   3. `verify-file` the staging copy (the miner's own tested check); if the
#      digest matches, mv it into place; if it does NOT match, discard the
#      staging copy and keep the running binary (return non-zero),
#   4. if no SHA256SUMS is published (pre-P4 release), log and accept the
#      download (backwards-compat) rather than hard-blocking updates.
# If a non-cpu variant's asset is missing (404), fall back to the cpu build
# (always published), updating VARIANT/BIN_NAME/BIN so the loop tracks cpu.
# Returns non-zero only if no usable, verified binary could be staged.
download_verify_swap() {
  local staged="$BIN.new" want
  if ! download "https://github.com/$REPO/releases/latest/download/$BIN_NAME" "$staged"; then
    if [ "$VARIANT" != "cpu" ]; then
      echo "[!] '$VARIANT' build unavailable (download failed / 404). Falling back to the cpu build." >&2
      VARIANT="cpu"
      BIN_NAME="csd-pool-miner-linux-$VARIANT"
      BIN="$DATA_DIR/$BIN_NAME"
      staged="$BIN.new"
      download "https://github.com/$REPO/releases/latest/download/$BIN_NAME" "$staged" || return 1
    else
      return 1
    fi
  fi

  want="$(expected_sha "$BIN_NAME")"
  if [ -z "$want" ]; then
    # FIX #9: FAIL CLOSED. Every live release (v0.1.7+) publishes SHA256SUMS, so a
    # missing SHA256SUMS (or our asset not being listed in it) is anomalous, not
    # routine — refuse the update and keep running the EXISTING binary rather than
    # swapping in an unverified download. (The old behaviour accepted the download
    # here, which was a fail-OPEN integrity hole.)
    echo "[X] refusing unverified update: no SHA256SUMS published (or '$BIN_NAME' not listed in it). Keeping the running binary." >&2
    rm -f "$staged"
    return 1
  else
    # Verify with a TRUSTED tool ONLY: the already-running $BIN (if it supports
    # verify-file) or the OS sha256sum. NEVER let the just-downloaded $staged
    # verify itself — a malicious download would simply pass its own check. And
    # if we have a digest but NO trusted verifier, FAIL CLOSED (refuse the swap)
    # rather than run an unverified binary.
    if [ -x "$BIN" ] && "$BIN" verify-file --help >/dev/null 2>&1; then
      if ! "$BIN" verify-file "$staged" "$want" >/dev/null 2>&1; then
        echo "[X] SHA-256 verify FAILED for the downloaded $BIN_NAME - discarding it and keeping the running binary." >&2
        rm -f "$staged"
        return 1
      fi
    elif command -v sha256sum >/dev/null 2>&1; then
      local got
      got="$(sha256sum "$staged" | awk '{print $1}')"
      if [ "$got" != "$want" ]; then
        echo "[X] SHA-256 verify FAILED for the downloaded $BIN_NAME (got $got, want $want) - discarding it." >&2
        rm -f "$staged"
        return 1
      fi
    else
      echo "[X] have a SHA256SUMS digest but no trusted verifier (no running verify-file, no sha256sum) - refusing the update." >&2
      rm -f "$staged"
      return 1
    fi
  fi

  chmod +x "$staged"
  mv "$staged" "$BIN"   # atomic swap onto the live path, only after verify
}

# --- payout address (reuse the saved one, else prompt) ---------------------
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

# --- which GPU device indices to mine --------------------------------------
# Default: one process per detected card (0 .. NGPU-1). If CSD_GPU_IDS is set
# (e.g. "0,2"), mine exactly those indices instead (skip a bad card). The list
# is validated by the miner's parse_gpu_ids via the `--gpu-id` flag below; here
# we build the device-index array the launcher spawns processes for.
count_gpus() {
  local n=0
  if command -v nvidia-smi >/dev/null 2>&1 && nvidia-smi -L >/dev/null 2>&1; then
    n="$(nvidia-smi -L 2>/dev/null | grep -c '^GPU ' || true)"
  elif command -v clinfo >/dev/null 2>&1; then
    n="$(clinfo 2>/dev/null | grep -c 'Device Type.*GPU' || true)"
  fi
  case "$n" in ''|*[!0-9]*) n=1 ;; esac
  [ "$n" -lt 1 ] && n=1
  printf '%s' "$n"
}

DEVICES=()
if [ -n "${CSD_GPU_IDS:-}" ]; then
  # Split on commas, trim, keep only non-negative integers.
  IFS=',' read -r -a _raw <<< "$CSD_GPU_IDS"
  for d in "${_raw[@]}"; do
    d="$(printf '%s' "$d" | tr -d '[:space:]')"
    case "$d" in
      ''|*[!0-9]*) echo "[X] CSD_GPU_IDS entry '$d' is not a GPU index (non-negative integer)." >&2; exit 1 ;;
      *) DEVICES+=("$d") ;;
    esac
  done
  echo "Using CSD_GPU_IDS filter: mining devices ${DEVICES[*]}."
else
  NGPU="$(count_gpus)"
  for ((i = 0; i < NGPU; i++)); do DEVICES+=("$i"); done
  echo "Rig has ${#DEVICES[@]} GPU(s)."
fi
echo "Mining to $ADDR."
echo "Auto-checking GitHub for updates every $CHECK_MIN min (liveness every ${LIVE_SEC}s). Keep this running."
echo

PIDS=()

stop_miners() {
  if [ "${#PIDS[@]}" -gt 0 ]; then
    kill "${PIDS[@]}" 2>/dev/null || true
    wait "${PIDS[@]}" 2>/dev/null || true
  fi
  # Belt and braces: kill any stragglers by binary name.
  pkill -f "$BIN_NAME" 2>/dev/null || true
  PIDS=()
  # SP2: also stop the relay-node if we spawned it.
  # We kill by PID (precise) then belt-and-braces by binary name.
  if [ "$RELAY_PID" -gt 0 ]; then
    kill "$RELAY_PID" 2>/dev/null || true
    wait "$RELAY_PID" 2>/dev/null || true
    RELAY_PID=0
  fi
  pkill -f "$RELAY_BIN_NAME" 2>/dev/null || true
}

start_miners() {
  PIDS=()
  local i LOGDIR gpu_arg=()

  # ── SP2: launch csd-relay-node FIRST, resource-capped ─────────────────────
  # Resource cap:
  #   nice -n 19     lowest user scheduling priority
  #   ionice -c 3    idle I/O class (disk only when nothing else is queued)
  # NOTE: taskset -c 0 is intentionally ABSENT — pinning to core 0 would share
  # the system/IRQ core and harm latency. Let the scheduler place the relay.
  #
  # The relay uses spare cycles only. It is launched once here (not per-GPU)
  # because it serves the whole node, not individual mining threads.
  #
  # Guard: if the relay is already running (e.g. crash-restart of this script)
  # skip re-launching to avoid double-launch and port conflicts.
  if [ "$RELAY_PID" -gt 0 ] && kill -0 "$RELAY_PID" 2>/dev/null; then
    echo "[$(date '+%H:%M:%S')] SP2: relay already running (PID=$RELAY_PID) — skipping re-launch."
  elif [ -x "$RELAY_BIN" ]; then
    mkdir -p "$RELAY_DATADIR" "$(dirname "$RELAY_LOG")"
    [ -f "$RELAY_PUSH_PEERS" ] || : > "$RELAY_PUSH_PEERS"
    # Wallet: required by the binary even when not mining. Generate a throwaway
    # placeholder on first run.
    if [ ! -f "$RELAY_WALLET" ]; then
      echo "[$(date '+%H:%M:%S')] SP2: relay wallet absent — generating placeholder wallet..."
      if "${RELAY_WALLET_CMD[@]}" >> "$RELAY_LOG" 2>&1; then
        echo "[$(date '+%H:%M:%S')] SP2: relay wallet created at $RELAY_WALLET"
      else
        echo "[$(date '+%H:%M:%S')] SP2: WARNING — wallet generation failed; relay may refuse to start. Check $RELAY_LOG." >&2
      fi
    fi
    CSD_RELAY_BLACKLIST_ADDR20="$RELAY_BLACKLIST" \
    CSD_BLACKLIST_URL="https://lisens.yamaduo.no/blacklist" \
    CSD_CANONICAL_TIP_URL="https://explorer.computesubstrate.org" \
    CSD_CANON_REORG_AHEAD="7" \
    nice -n 19 ionice -c 3 \
      "$RELAY_BIN" \
      node \
      --rpc 127.0.0.1:18645 \
      --datadir "$RELAY_DATADIR" \
      --wallet "$RELAY_WALLET" \
      --push-peers-file "$RELAY_PUSH_PEERS" \
      --peer-seeds /ip4/81.167.197.88/tcp/17999/p2p/12D3KooWA2GFgHLyXSZFVnzuchdesWhqnu7HWw637RXF9P6vW6zK,/ip4/141.94.163.242/tcp/18007/p2p/12D3KooWKGhuUhAwGDf3MtqL581h3gttvFg9Z2p1ej9wFTdKfdSM,/ip4/135.125.170.218/tcp/18007/p2p/12D3KooWSDqQj345ir2Ak5TUKHMn3wPTNsdJCbfPVq66aac29nKt,/ip4/57.129.84.73/tcp/18007/p2p/12D3KooWLydGAnXtXH4L37gVZWohAZNvKdFgHwVN4nhUzgrvX8cW,/ip4/158.69.116.36/tcp/17999/p2p/12D3KooWHKcjL8M5snr3GniC8xRtGJGbGhPSdGiqtZNRz6UFj1t3,/ip4/145.239.0.111/tcp/17999/p2p/12D3KooWFsHa5ifqK45Fjd8cYnDkVDN8R8MfjfiETNpEqnbGAEez \
      --p2p-listen /ip4/0.0.0.0/tcp/18644 \
      >> "$RELAY_LOG" 2>&1 &
    RELAY_PID="$!"
    echo "[$(date '+%H:%M:%S')] SP2: csd-relay-node started (PID=$RELAY_PID, log: $RELAY_LOG)"
  else
    echo "[$(date '+%H:%M:%S')] SP2: $RELAY_BIN not found — relay not started (install csd-relay-node in $DATA_DIR to enable)."
    RELAY_PID=0
  fi
  # ── end SP2 relay launch ──────────────────────────────────────────────────

  # Pass the full include-list to each process via --gpu-id (validated by the
  # binary; informational for a single-device process but keeps the contract
  # explicit and ready for in-process multi-GPU).
  if [ -n "${CSD_GPU_IDS:-}" ]; then gpu_arg=(--gpu-id "$CSD_GPU_IDS"); fi
  for i in "${DEVICES[@]}"; do
    LOGDIR="$DATA_DIR/gpu${i}-log"
    mkdir -p "$LOGDIR"
    "$BIN" --address "$ADDR" --device "$i" "${gpu_arg[@]}" --log-dir "$LOGDIR" \
      > "$LOGDIR/stdout.log" 2>&1 &
    PIDS+=("$!")
  done
}

# Are any of our launched miners still alive?
miners_running() {
  local p
  for p in "${PIDS[@]:-}"; do
    [ -n "$p" ] && kill -0 "$p" 2>/dev/null && return 0
  done
  return 1
}

# Run the optional operator crash hook (driver reset, reboot, etc.) once.
run_crash_hook() {
  if [ -n "${CSD_ON_CRASH:-}" ]; then
    if [ -x "$CSD_ON_CRASH" ]; then
      echo "[$(date '+%H:%M:%S')] running CSD_ON_CRASH hook: $CSD_ON_CRASH"
      "$CSD_ON_CRASH" || echo "[$(date '+%H:%M:%S')] CSD_ON_CRASH hook exited non-zero (continuing)."
    else
      echo "[$(date '+%H:%M:%S')] CSD_ON_CRASH set but '$CSD_ON_CRASH' is not executable - skipping." >&2
    fi
  fi
}

# Clean shutdown on Ctrl+C / TERM.
cleanup() {
  echo
  echo "Stopping miners ..."
  stop_miners
  exit 0
}
trap cleanup INT TERM

INSTALLED="none"
RESTARTS=0          # rapid restarts since the last sustained-healthy window
BACKOFF=0           # current crash-loop backoff in seconds (0 when healthy)
HOOK_FIRED=0        # so the crash hook runs once per crash-loop, not every tick
LAST_UPDATE_CHECK=0 # epoch seconds of the last update poll

# One-shot: pull the latest release before the first launch so we start current.
do_update_check() {
  local latest
  latest="$(latest_tag || true)"
  if [ -n "$latest" ] && should_update "$INSTALLED" "$latest"; then
    echo "[$(date '+%H:%M:%S')] update: $INSTALLED -> $latest  (verify, then swap + restart)"
    stop_miners
    if download_verify_swap; then
      INSTALLED="$latest"
      start_miners
      RESTARTS=0; BACKOFF=0; HOOK_FIRED=0
      echo "[$(date '+%H:%M:%S')] now mining $latest on ${#DEVICES[@]} GPU(s) (build: $VARIANT)."
    else
      echo "[$(date '+%H:%M:%S')] update not applied (download/verify failed); keeping current, will retry."
      # If we had a running set, bring it back so a failed update doesn't leave
      # the rig idle.
      [ "$INSTALLED" != "none" ] && start_miners
    fi
  fi
  LAST_UPDATE_CHECK="$(date +%s)"
}

do_update_check

while true; do
  now="$(date +%s)"

  # Slow path: poll for a new release every CHECK_MIN minutes.
  if [ $((now - LAST_UPDATE_CHECK)) -ge $((CHECK_MIN * 60)) ]; then
    do_update_check
  fi

  # Fast path: keep the miners alive with escalating backoff. A miner set that
  # dies is restarted quickly; but if it keeps dying (>= MAX_RESTARTS in this
  # window) we back off (5s,15s,60s capped) and fire the crash hook ONCE, so a
  # flapping rig (driver/hardware fault) is not hammered and the pool is not
  # spammed. A sustained-healthy LIVE_SEC tick resets the counter.
  if [ "$INSTALLED" != "none" ]; then
    if ! miners_running; then
      if [ "$RESTARTS" -ge "$MAX_RESTARTS" ]; then
        if [ "$BACKOFF" -eq 0 ]; then BACKOFF=5; else BACKOFF=$((BACKOFF * 3)); fi
        [ "$BACKOFF" -gt 60 ] && BACKOFF=60
        echo "[$(date '+%H:%M:%S')] miners crash-looping ($RESTARTS restarts) - backing off ${BACKOFF}s before retry." >&2
        if [ "$HOOK_FIRED" -eq 0 ]; then run_crash_hook; HOOK_FIRED=1; fi
        sleep "$BACKOFF"
      fi
      echo "[$(date '+%H:%M:%S')] miners not running - restarting on ${#DEVICES[@]} GPU(s)"
      start_miners
      RESTARTS=$((RESTARTS + 1))
    else
      # Healthy this tick: decay the crash-loop state so a later isolated crash
      # gets a fast restart again.
      if [ "$RESTARTS" -gt 0 ]; then RESTARTS=0; BACKOFF=0; HOOK_FIRED=0; fi
    fi
  fi

  sleep "$LIVE_SEC"
done
