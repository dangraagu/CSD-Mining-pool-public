#!/usr/bin/env bash
set -euo pipefail

# Original invocation argv, captured BEFORE anything consumes $1, so the
# staged-launcher handoff (reexec_new_launcher) can exec the refreshed launcher
# with the exact same arguments this run was started with.
ORIG_ARGS=("$@")

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

# --- Shared GPU auto-detection (identical in install-csd-miner.sh /
# mine-all-gpus.sh). Returns nvidia | amd | cpu. NVIDIA wins on ANY of three
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
#
# Optional $3 = max_time (seconds): when set, bound the transfer with curl
# --max-time / wget --timeout so a hung CDN cannot stall the caller (HAZARD-2,
# used by the relay fetch). When OMITTED, behaviour is byte-identical to the
# original unbounded download (the binary + launcher-self-update paths rely on
# that, and so do their tests). Keeping ONE download primitive also means the
# hermetic tests' single `download()` override intercepts the relay fetch too.
download() {
  local url="$1" out="$2" max_time="${3:-}" tmp
  tmp="$out.tmp"
  # Harden the predictable staging path: drop any pre-existing entry and create
  # $tmp fresh under noclobber (O_EXCL) BEFORE fetching, so curl/wget cannot
  # follow a planted symlink at $tmp and write THROUGH it to an arbitrary target.
  # $tmp sits beside $out (same filesystem) so the final mv stays an atomic
  # rename(2). The verify-then-swap trust flow downstream is unchanged.
  rm -f "$tmp"
  ( set -C; : > "$tmp" ) 2>/dev/null || { echo "[X] download: staging path '$tmp' not creatable as a fresh regular file" >&2; return 1; }
  if command -v curl >/dev/null 2>&1; then
    if [ -n "$max_time" ]; then
      curl -fL --retry 3 --max-time "$max_time" -o "$tmp" "$url" && mv "$tmp" "$out"
    else
      curl -fL --retry 3 -o "$tmp" "$url" && mv "$tmp" "$out"
    fi
  elif command -v wget >/dev/null 2>&1; then
    if [ -n "$max_time" ]; then
      wget --timeout="$max_time" -O "$tmp" "$url" && mv "$tmp" "$out"
    else
      wget -O "$tmp" "$url" && mv "$tmp" "$out"
    fi
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
  local asset="$1" max="${2:-}" sums
  sums="$DATA_DIR/SHA256SUMS.tmp"
  if download "https://github.com/$REPO/releases/latest/download/SHA256SUMS" "$sums" "$max" 2>/dev/null; then
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

# ── SP2: standalone relay auto-install ────────────────────────────────────────
# Make sure a TRUSTED csd-relay-node binary is present in $DATA_DIR so this
# standalone Linux rig contributes a relay (HiveOS rigs already get it bundled).
# This is purely a BONUS to the network: it MUST NEVER block, delay, or abort the
# GPU miner (which is the revenue). Idempotent + sourceable (sits before the
# CSD_SOURCE_ONLY cutoff so tests can drive it).
#
# THREE BINDING RULES:
#   1. BEST-EFFORT  — every failure path prints a message and `return 0`; we
#      never `exit` and never propagate a non-zero up to the (set -e) caller.
#      The call site is `ensure_relay || true` as a second belt.
#   2. FAIL-CLOSED  — we reuse the EXACT trusted-verifier discipline of
#      download_verify_swap(): verify the staged download with the already-on-disk
#      relay's `verify-file` OR the OS sha256sum — NEVER let the just-downloaded
#      binary verify itself. No published digest, or a digest but no trusted
#      verifier, or a mismatch ⇒ discard the download and do NOT place/exec it.
#   3. OPT-OUT      — CSD_NO_RELAY=1 skips the whole thing (download AND, via
#      start_relay's guard, the start) with a clear message.
#
# HAZARD-2 mitigation: the relay fetch is bounded with `curl --max-time` (passed
# as the optional 3rd arg to the shared download()) so a hung CDN cannot stall
# the GPU-miner start. We download to a `.new` staging path and only atomically
# mv it into $RELAY_BIN after a successful trusted-verify.
RELAY_DL_MAX_TIME="${RELAY_DL_MAX_TIME:-60}"   # seconds; bound the relay fetch (HAZARD-2)

ensure_relay() {
  # Rule 3: explicit opt-out — no download, no install.
  if [ "${CSD_NO_RELAY:-0}" = "1" ]; then
    echo "[$(date '+%H:%M:%S')] relay: relay helper disabled (CSD_NO_RELAY=1) — mining only, all good."
    return 0
  fi
  # Idempotent: a usable relay is already on disk — nothing to do (no re-download).
  if [ -x "$RELAY_BIN" ]; then
    return 0
  fi

  local staged="$RELAY_BIN.new" want
  echo "[$(date '+%H:%M:%S')] relay: relay helper (optional — boosts pool propagation) installing… mining is unaffected."

  # 1. Download to a staging path (bounded via max-time; never onto live $RELAY_BIN).
  if ! download "https://github.com/$REPO/releases/latest/download/$RELAY_BIN_NAME" "$staged" "$RELAY_DL_MAX_TIME"; then
    echo "[$(date '+%H:%M:%S')] relay: relay helper unavailable right now (offline?) — no problem, mining continues; will retry next run." >&2
    rm -f "$staged"
    return 0
  fi

  # 2. Expected SHA-256 from the SAME release SHA256SUMS, keyed by basename.
  want="$(expected_sha "$RELAY_BIN_NAME" "$RELAY_DL_MAX_TIME")"
  if [ -z "$want" ]; then
    # Rule 2 (fail-closed): no published digest for the relay ⇒ refuse to install.
    echo "[$(date '+%H:%M:%S')] relay: can't safely verify the relay helper right now (no SHA-256 listed for '$RELAY_BIN_NAME') — skipping it; mining continues." >&2
    rm -f "$staged"
    return 0
  fi

  # 3. Verify the staged copy with a TRUSTED verifier ONLY (never the download
  #    itself): prefer the already-on-disk relay's verify-file, else OS sha256sum;
  #    a digest with no trusted verifier ⇒ FAIL CLOSED. (Mirrors download_verify_swap.)
  if [ -x "$RELAY_BIN" ] && "$RELAY_BIN" verify-file --help >/dev/null 2>&1; then
    if ! "$RELAY_BIN" verify-file "$staged" "$want" >/dev/null 2>&1; then
      echo "[$(date '+%H:%M:%S')] relay: relay helper didn't pass its SHA-256 safety check — skipping it for safety; mining continues." >&2
      rm -f "$staged"
      return 0
    fi
  elif command -v sha256sum >/dev/null 2>&1; then
    local got
    got="$(sha256sum "$staged" | awk '{print $1}')"
    if [ "$got" != "$want" ]; then
      echo "[$(date '+%H:%M:%S')] relay: relay helper didn't pass its SHA-256 safety check (got $got, want $want) — skipping it for safety; mining continues." >&2
      rm -f "$staged"
      return 0
    fi
  else
    echo "[$(date '+%H:%M:%S')] relay: can't safely verify the relay helper right now (no SHA-256 verifier available) — skipping it; mining continues." >&2
    rm -f "$staged"
    return 0
  fi

  # 4. Verified: chmod +x and atomic mv into place (only after verify).
  chmod +x "$staged"
  if mv "$staged" "$RELAY_BIN"; then
    echo "[$(date '+%H:%M:%S')] relay: relay helper ready (verified)."
  else
    echo "[$(date '+%H:%M:%S')] relay: relay helper couldn't be set up this run — no problem, mining continues; will retry next run." >&2
    rm -f "$staged"
  fi
  return 0
}

# ── SP2: start the relay-node (factored out of start_miners so it is sourceable
# and so the opt-out lives in ONE place). Resource-capped (nice/ionice) so it
# never starves the GPU miner; launched once (serves the whole node). Honours
# CSD_NO_RELAY=1 as its FIRST branch so opt-out skips the start even if a binary
# is already on disk (e.g. a HiveOS-bundled relay). PRESERVES the existing final
# else (calm "relay helper not running" — optional) mines-on behaviour.
# Best-effort: any issue here must never disturb mining.
start_relay() {
  # Rule 3: opt-out wins over everything — never start, even if a binary exists.
  if [ "${CSD_NO_RELAY:-0}" = "1" ]; then
    echo "[$(date '+%H:%M:%S')] relay: relay helper disabled (CSD_NO_RELAY=1) — mining only, all good."
    RELAY_PID=0
    return 0
  fi
  # Guard: if the relay is already running (e.g. crash-restart of this script)
  # skip re-launching to avoid double-launch and port conflicts.
  if [ "$RELAY_PID" -gt 0 ] && kill -0 "$RELAY_PID" 2>/dev/null; then
    echo "[$(date '+%H:%M:%S')] relay: relay helper already running (PID=$RELAY_PID) — leaving it as is."
    return 0
  elif [ -x "$RELAY_BIN" ]; then
    mkdir -p "$RELAY_DATADIR" "$(dirname "$RELAY_LOG")"
    [ -f "$RELAY_PUSH_PEERS" ] || : > "$RELAY_PUSH_PEERS"
    # Wallet: required by the binary even when not mining. Generate a throwaway
    # placeholder on first run.
    if [ ! -f "$RELAY_WALLET" ]; then
      echo "[$(date '+%H:%M:%S')] relay: first-time setup — preparing the relay helper’s placeholder wallet…"
      if "${RELAY_WALLET_CMD[@]}" >> "$RELAY_LOG" 2>&1; then
        echo "[$(date '+%H:%M:%S')] relay: relay helper wallet ready at $RELAY_WALLET"
      else
        echo "[$(date '+%H:%M:%S')] relay: couldn't set up the relay helper’s wallet this run — no problem, mining continues (details in $RELAY_LOG)." >&2
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
    echo "[$(date '+%H:%M:%S')] relay: relay helper running (PID $RELAY_PID) — thanks for helping pool propagation. (log: $RELAY_LOG)"
  else
    echo "[$(date '+%H:%M:%S')] relay: relay helper not running (optional — boosts pool propagation; your mining is unaffected). It auto-installs next run; set CSD_NO_RELAY=1 to skip."
    RELAY_PID=0
  fi
  return 0
}
# ── end SP2 standalone relay auto-install ─────────────────────────────────────

# Absolute path of THIS launcher script, captured before any cd, so the launcher
# self-update writes back to the right file even if $0 was relative. mine-auto.sh
# never cd's, but we resolve defensively. `${BASH_SOURCE[0]:-$0}` (the v0.1.15
# FIX-1 pattern): under a piped run (curl|bash) BASH_SOURCE is empty and $0 is
# "bash" — the fallback keeps `set -u` from aborting, and reexec_new_launcher's
# real-file guard then correctly refuses a re-exec for such runs.
SELF_PATH="${BASH_SOURCE[0]:-$0}"
case "$SELF_PATH" in
  /*) : ;;                                  # already absolute
  *)  SELF_PATH="$(pwd)/$SELF_PATH" ;;      # make relative invocation absolute
esac
SELF_NAME="mine-auto.sh"   # the release-asset basename + the SHA256SUMS key
# Digest of the launcher verified+swapped by the LAST update_launcher_self run
# that returned 0. reexec_new_launcher re-verifies the on-disk file against it
# immediately before exec'ing (guards a racing writer between mv and exec).
LAUNCHER_SWAPPED_SHA=""

# Update the LAUNCHER ITSELF (this mine-auto.sh) in place, fail-closed + no-brick.
#
# WHY: download_verify_swap above swaps only the miner BINARY ($BIN); a fix to
# THIS script (e.g. a relay-launch argv fix) would otherwise never reach a rig
# that only ever runs the old on-disk launcher. So after a verified binary swap
# we also refresh the launcher — but with the SAME three-gate discipline and two
# extra hard safety rules, because a bad launcher is a brick, not just a stale
# miner:
#
#   SAFETY (a) FAIL-CLOSED: a download failure, a missing/!listed SHA256SUMS
#     entry for "$SELF_NAME", a SHA mismatch, or no trusted verifier ALL discard
#     the temp and leave the on-disk launcher byte-for-byte untouched. The rig
#     keeps running the known-good launcher. We never write an unverified script
#     over ourselves.
#   SAFETY (b) NO-BRICK SWAP, RE-EXEC SEPARATE + GATED: this function NEVER
#     execs anything (the launcher_selfupdate.sh static guard F pins that). It
#     only replaces the file ATOMICALLY on disk (write temp → verify → mv),
#     keeping "$SELF_PATH.bak" (the prior launcher) as a fallback. The
#     currently-running process keeps using the already-loaded (old) script
#     text, so the swap itself can never brick this launch. APPLYING the new
#     launcher is a SEPARATE, multiply-guarded step — reexec_new_launcher —
#     which the caller invokes ONLY when this function reports a real byte
#     change (rc=0); every one of its guards fails SAFE back to "keep running
#     the old in-memory launcher, miners keep mining". (Pre-v0.2.0 this fn was
#     no-re-exec-at-all, which meant an always-running rig NEVER picked up a
#     new launcher — the autorestart root cause.)
#
# Verifier trust: we verify with the just-swapped, already-SHA-verified $BIN
# ("$BIN verify-file") or the OS sha256sum — NEVER by letting the downloaded
# script check itself. The caller treats this as best-effort (a launcher-update
# failure must never disturb mining).
#
# RETURN CONTRACT (v0.2.0, mirrors h-run's ua_download_verify_swap):
#   0 = a REAL byte change was verified + swapped onto disk; the verified
#       digest is exported in $LAUNCHER_SWAPPED_SHA for the re-exec re-check.
#   2 = verified, but the on-disk launcher is ALREADY byte-identical (skip):
#       nothing swapped, the caller MUST NOT re-exec — this is the natural
#       re-exec loop-break (after a handoff, the next poll lands here).
#   1 = any download/verify/swap failure (fail-closed; on-disk untouched).
update_launcher_self() {
  local staged want got cur
  staged="$SELF_PATH.new.$$"
  LAUNCHER_SWAPPED_SHA=""   # only a REAL rc=0 swap publishes a digest

  # 1. Download the candidate launcher to a per-pid temp (never onto $SELF_PATH).
  if ! download "https://github.com/$REPO/releases/latest/download/$SELF_NAME" "$staged"; then
    echo "[$(date '+%H:%M:%S')] launcher self-update: download failed; keeping the on-disk launcher." >&2
    rm -f "$staged"
    return 1
  fi

  # 2. Expected SHA-256 from the SAME release SHA256SUMS, keyed by basename.
  want="$(expected_sha "$SELF_NAME")"
  if [ -z "$want" ]; then
    # FAIL CLOSED: no published launcher checksum (or this release doesn't ship
    # the launcher as an asset) → refuse, keep the on-disk launcher untouched.
    echo "[$(date '+%H:%M:%S')] launcher self-update: no SHA256SUMS entry for '$SELF_NAME' — refusing (keeping on-disk launcher)." >&2
    rm -f "$staged"
    return 1
  fi

  # 3. Verify the temp with a TRUSTED verifier (never the downloaded script
  #    itself). Prefer the freshly-verified $BIN; else OS sha256sum; else refuse.
  if [ -x "$BIN" ] && "$BIN" verify-file --help >/dev/null 2>&1; then
    if ! "$BIN" verify-file "$staged" "$want" >/dev/null 2>&1; then
      echo "[$(date '+%H:%M:%S')] launcher self-update: SHA-256 verify FAILED for $SELF_NAME — discarding, keeping on-disk launcher." >&2
      rm -f "$staged"
      return 1
    fi
  elif command -v sha256sum >/dev/null 2>&1; then
    got="$(sha256sum "$staged" | awk '{print $1}')"
    if [ "$got" != "$want" ]; then
      echo "[$(date '+%H:%M:%S')] launcher self-update: SHA-256 verify FAILED for $SELF_NAME (got $got, want $want) — discarding." >&2
      rm -f "$staged"
      return 1
    fi
  else
    echo "[$(date '+%H:%M:%S')] launcher self-update: have a digest but no trusted verifier — refusing." >&2
    rm -f "$staged"
    return 1
  fi

  # 4. Skip the write if the on-disk launcher is already byte-identical (same
  #    SHA) — avoids needless churn + a pointless .bak rewrite every poll.
  #    rc=2 = "verified, already current": the caller must NOT re-exec on it.
  if command -v sha256sum >/dev/null 2>&1; then
    cur="$(sha256sum "$SELF_PATH" 2>/dev/null | awk '{print $1}')"
    if [ -n "$cur" ] && [ "$cur" = "$want" ]; then
      rm -f "$staged"
      return 2
    fi
  fi

  # 5. NO-BRICK atomic on-disk swap. Keep the prior launcher as .bak, preserve the
  #    exec bit, mv into place. We DO NOT exec here — applying it is the caller's
  #    separately-gated reexec_new_launcher step (or the next manual start).
  chmod +x "$staged" 2>/dev/null || true
  cp -p "$SELF_PATH" "$SELF_PATH.bak" 2>/dev/null || true
  if mv "$staged" "$SELF_PATH"; then
    LAUNCHER_SWAPPED_SHA="$want"
    echo "[$(date '+%H:%M:%S')] launcher self-update: refreshed $SELF_NAME on disk (prior kept at $SELF_PATH.bak)."
    return 0
  fi
  echo "[$(date '+%H:%M:%S')] launcher self-update: on-disk swap failed — keeping current launcher." >&2
  rm -f "$staged"
  return 1
}

# Write the operator-visible crumb + log line for a REFUSED/FAILED re-exec,
# then return 1. NEVER exits: the old in-memory launcher keeps running (bash
# holds the old inode), miners keep mining — a refused handoff can never hurt
# the running process; the new launcher still applies on the next manual start.
reexec_refuse() {
  local reason="$1" crumb="$DATA_DIR/launcher-reexec-refused.txt"
  echo "[$(date '+%H:%M:%S')] launcher re-exec: REFUSED — $reason. Staying on the in-memory launcher (the refreshed one applies on the next manual restart)." >&2
  {
    echo "time:   $(date -u '+%Y-%m-%dT%H:%M:%SZ' 2>/dev/null || date)"
    echo "reason: $reason"
    echo "note:   mining was NOT interrupted; restart mine-auto.sh manually to pick up the refreshed launcher"
  } > "$crumb" 2>/dev/null || true
  return 1
}

# Apply an already-swapped launcher by STAGED HANDOFF: stop the miners cleanly,
# then `exec` the refreshed on-disk launcher with the ORIGINAL argv. This is the
# autorestart root-cause fix — on an always-running rig the on-disk swap alone
# never took effect. Ports the safety properties of mine-auto.bat's trampoline
# (stage-time SHA re-check + detached-helper discipline) into bash:
#
#   g1  SELF_PATH must be a real readable file — skips curl|bash piped runs
#       (there $0 is "bash"; nothing sensible to exec).
#   g2  RE-VERIFY the on-disk SHA-256 against the digest verified at swap time
#       ($1 = $LAUNCHER_SWAPPED_SHA) — guards a racing writer between mv+exec.
#   g3  `bash -n` syntax gate — a truncated/corrupt script is refused BEFORE
#       exec (and the .bak is restored over it, best-effort).
#   g4  BOUNDED: CSD_REEXEC_GEN (exported, survives exec) caps chained handoffs
#       at 3 — never a re-exec loop. The NATURAL loop-break also holds: after a
#       handoff the next poll's update_launcher_self returns 2 (skip-if-same),
#       and the caller only invokes us on rc=0.
#   g5  HANDOFF: stop_miners (clean kill of miners + relay), `shopt -s execfail`
#       so a FAILED exec RETURNS instead of killing this shell, exec. On the
#       fall-through: start_miners (recover mining on the old launcher), refuse.
#
# EVERY guard fails SAFE: log + crumb + return 1, old launcher keeps running,
# miners keep mining. Env knobs (CHECK_MIN, CSD_GPU_IDS, …) survive the exec
# natively; the address re-loads from $CFG (no re-prompt); the fresh startup's
# do_update_check reaps any straggler processes before start_miners.
# Called by do_update_check ONLY when update_launcher_self returned 0.
reexec_new_launcher() {
  local swapped_sha="${1:-}" cur
  # g1: a real, readable file (piped runs: $SELF_PATH is "bash"/missing).
  if [ -z "$SELF_PATH" ] || [ ! -f "$SELF_PATH" ] || [ ! -r "$SELF_PATH" ]; then
    reexec_refuse "launcher path '$SELF_PATH' is not a readable file (piped/odd invocation)"
    return 1
  fi
  # g2: the on-disk bytes must still be EXACTLY what was verified at swap time.
  if [ -z "$swapped_sha" ] || ! command -v sha256sum >/dev/null 2>&1; then
    reexec_refuse "no swap-time digest (or no sha256sum) to re-verify the on-disk launcher with"
    return 1
  fi
  cur="$(sha256sum "$SELF_PATH" 2>/dev/null | awk '{print $1}')"
  if [ "$cur" != "$swapped_sha" ]; then
    reexec_refuse "on-disk launcher SHA changed between swap and exec (want $swapped_sha, got ${cur:-<unreadable>})"
    return 1
  fi
  # g3: refuse a syntactically-broken script BEFORE exec; put the .bak back.
  if ! bash -n "$SELF_PATH" 2>/dev/null; then
    [ -f "$SELF_PATH.bak" ] && cp -p "$SELF_PATH.bak" "$SELF_PATH" 2>/dev/null || true
    reexec_refuse "refreshed launcher failed the bash -n syntax gate (restored $SELF_PATH.bak over it)"
    return 1
  fi
  # g4: bounded chained handoffs (exported so it survives the exec).
  export CSD_REEXEC_GEN=$(( ${CSD_REEXEC_GEN:-0} + 1 ))
  if [ "$CSD_REEXEC_GEN" -gt 3 ]; then
    reexec_refuse "re-exec generation cap reached (gen $CSD_REEXEC_GEN > 3) — falling back to next-manual-restart behaviour"
    return 1
  fi
  # g5: staged handoff. stop_miners kills miners + relay cleanly. `shopt -s
  # execfail` makes a FAILED exec of an EXISTING-but-unexecutable file (rc 126:
  # noexec mount, exec-format, missing interpreter) RETURN here instead of
  # exiting — g1 already proved $SELF_PATH exists+readable and g3 proved it's
  # valid bash, so a "command not found" (rc 127, which execfail can't catch and
  # would hard-exit) is unreachable. Under this script's `set -euo pipefail`,
  # `set -e` is already suppressed inside this function because do_update_check
  # invokes it as the left operand of `|| true`; the trailing `|| true` here is
  # defensive depth so the fall-through recovery ALSO holds if the function is
  # ever called bare. On a SUCCESSFUL exec the image is replaced and neither the
  # `|| true` nor the recovery block below is ever reached.
  echo "[$(date '+%H:%M:%S')] launcher re-exec: handing off to the refreshed launcher (generation $CSD_REEXEC_GEN, argv preserved)."
  stop_miners
  shopt -s execfail
  exec "$SELF_PATH" ${ORIG_ARGS[@]+"${ORIG_ARGS[@]}"} || true
  # Reached ONLY when the exec itself FAILED (execfail returned control): recover
  # mining on the old in-memory launcher — the rig must never sit idle because of
  # a handoff — then log + crumb + return non-zero.
  start_miners
  reexec_refuse "exec of the refreshed launcher failed — miners restarted on the old launcher"
  return 1
}

# Test hook: when CSD_SOURCE_ONLY=1, stop here so a test can `source` this script
# to exercise the functions above (download / expected_sha / download_verify_swap
# / update_launcher_self) WITHOUT running the address prompt, GPU detection, or
# the mining loop. Has ZERO effect on a normal `./mine-auto.sh <variant>` run
# (the var is unset there). A sourced script uses `return`; on the (unexpected)
# case of being executed directly with the var set, `exit` is the fallback. The
# `# shellcheck disable` covers SC2317's false "unreachable" on the fallback.
if [ "${CSD_SOURCE_ONLY:-0}" = "1" ]; then
  # shellcheck disable=SC2317
  { return 0 2>/dev/null; exit 0; }
fi

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
# First-run banner: make it obvious WHICH build is running and WHERE its logs are,
# so an operator can confirm the right variant and find the crash reason fast.
echo "Selected build: $VARIANT   (binary: $BIN)"
echo "Per-GPU logs:   $DATA_DIR/gpu<N>-log/stdout.log"
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

  # ── SP2: standalone relay auto-install + launch (BEST-EFFORT) ──────────────
  # 1. ensure_relay: download+SHA-verify+place a TRUSTED csd-relay-node if one is
  #    not already on disk. `|| true` so a failed/blocked install can NEVER abort
  #    the (set -e) miner launch below — mining is revenue, the relay is a bonus.
  # 2. start_relay: launch it resource-capped (nice/ionice), once, BEFORE the GPU
  #    loop. Honours CSD_NO_RELAY=1 (skips install AND start) and the
  #    already-running guard; preserves the calm "relay helper not running"
  #    (optional) mines-on behaviour. The relay launch is backgrounded (&) so the GPU miners
  #    start without ever waiting on the relay network call (HAZARD-2).
  if [ "${CSD_NO_RELAY:-0}" = "1" ]; then
    echo "[$(date '+%H:%M:%S')] relay: relay helper disabled (CSD_NO_RELAY=1) — mining only, all good."
  fi
  ensure_relay || true
  start_relay || true
  # ── end SP2 relay block ────────────────────────────────────────────────────

  # Pass the full include-list to each process via --gpu-id (validated by the
  # binary; informational for a single-device process but keeps the contract
  # explicit and ready for in-process multi-GPU).
  if [ -n "${CSD_GPU_IDS:-}" ]; then gpu_arg=(--gpu-id "$CSD_GPU_IDS"); fi
  for i in "${DEVICES[@]}"; do
    LOGDIR="$DATA_DIR/gpu${i}-log"
    mkdir -p "$LOGDIR"
    "$BIN" --address "$ADDR" --device "$i" "${gpu_arg[@]}" --log-dir "$LOGDIR" \
      >> "$LOGDIR/stdout.log" 2>&1 &
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

# Surface WHY the miners are not running: tail the newest gpu*-log/stdout.log so
# the operator actually sees the crash reason (wrong build, missing driver lib,
# bad device), then print an actionable hint naming the running build and how to
# re-run with a different one. MESSAGE-ONLY — we do NOT auto-swap the build (a
# safety-review decision: never silently change what binary a rig runs).
report_crash_logs() {
  local newest="" f
  # Pick the most-recently-written per-GPU stdout.log under DATA_DIR. We avoid
  # `ls` (SC2012) and compare mtimes ourselves over the glob; `nullglob` keeps the
  # loop from iterating the literal pattern when no log exists yet.
  shopt -s nullglob
  for f in "$DATA_DIR"/gpu*-log/stdout.log; do
    if [ -z "$newest" ] || [ "$f" -nt "$newest" ]; then newest="$f"; fi
  done
  shopt -u nullglob
  if [ -n "$newest" ] && [ -s "$newest" ]; then
    echo "[$(date '+%H:%M:%S')] last miner output ($newest):" >&2
    tail -n 8 "$newest" 2>/dev/null | sed 's/^/    | /' >&2 || true
  fi
  echo "[$(date '+%H:%M:%S')] the ${VARIANT} build keeps exiting — re-run with a different build: nvidia | amd | cpu" >&2
  echo "[$(date '+%H:%M:%S')]   e.g.  ./mine-auto.sh nvidia   (or amd, or cpu)" >&2
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
      # Best-effort: also refresh THIS launcher (so a launcher-side fix reaches
      # the rig). Runs AFTER mining is back up so it can never delay the restart;
      # fail-closed + no-brick (atomic on-disk swap). ONLY a REAL byte change
      # (rc=0) proceeds to the gated staged-handoff re-exec — rc=2 (already
      # current, the post-handoff loop-break) and rc=1 (failure) never do. The
      # re-exec itself is best-effort: every refused guard leaves this (old)
      # launcher running with the miners restarted — never an idle rig.
      local launcher_rc=0
      update_launcher_self || launcher_rc=$?
      if [ "$launcher_rc" -eq 0 ]; then
        reexec_new_launcher "$LAUNCHER_SWAPPED_SHA" || true
      fi
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
      report_crash_logs || true
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
