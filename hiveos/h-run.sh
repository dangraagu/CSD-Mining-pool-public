#!/usr/bin/env bash
# HiveOS custom-miner launcher for csd-pool-miner (P4 §1 / SP2).
#
# HiveOS runs this to start mining. It MUST `exec` the real binary so the
# process argv is `csd-gpu-miner …` and NOT `h-run.sh` — HiveOS refuses to start
# a custom miner whose argv contains h-run.sh. `exec` replaces this shell; we
# never background with `&`.
#
# SP2 — csd-relay-node canonical-anchor relay
# ────────────────────────────────────────────────────────────────────────────────
# Before exec-ing the miner this script launches csd-relay-node in the background
# with strict resource caps so it NEVER starves the GPU miner:
#
#   nice -n 19     lowest user-space CPU scheduling priority
#   ionice -c 3    idle I/O class — disk only when nothing else needs it
# NOTE: taskset -c 0 is intentionally ABSENT — pinning to core 0 shares the
# system/IRQ core and degrades both the relay and system interrupts. Let the
# scheduler place it; nice+ionice provide sufficient yield.
#
# The relay is launched with `&` (background) before `exec`, which replaces this
# shell process. The relay process is an orphan owned by HiveOS's init; HiveOS
# restarts the whole slot (and therefore this script) when the miner exits, which
# naturally re-launches the relay too. No separate PID tracking is needed here.
#
# Launch args (REAL flag names, confirmed against binary):
#   --rpc             127.0.0.1:18645
#   --peer-seeds      <multiaddrs>
#   CSD_RELAY_BLACKLIST_ADDR20 env (blacklist delivered via environment, not a CLI flag)
#   --p2p-listen      /ip4/0.0.0.0/tcp/18644  (multiaddr)
# Fill in real peer-seeds in the constants block below.
#
# Forced miner flags (cannot be overridden from the flightsheet, by design):
#   --stats-port $CUSTOM_API_PORT --stats-bind 127.0.0.1
#     so h-stats.sh can scrape /1/summary on localhost only (never exposed).
# Everything else (address via --config, --backend, --gpu-id, …) comes from
# h-config.sh's output. The pool endpoint is compiled in and not configurable.

cd "$(dirname "$0")" || exit 1
# shellcheck source=/dev/null
[ -e h-manifest.conf ] && . ./h-manifest.conf

CONF="${CUSTOM_CONFIG_FILENAME:-config.toml}"
PORT="${CUSTOM_API_PORT:-3380}"
LOG="${CUSTOM_LOG_BASENAME:-/var/log/miner/csd-pool-miner/csd-pool-miner}.log"
mkdir -p "$(dirname "$LOG")"

# Re-render the config/flags from the current flightsheet (HiveOS also calls
# h-config.sh, but doing it here makes a manual `h-run.sh` run self-contained).
[ -x ./h-config.sh ] && ./h-config.sh >/dev/null 2>&1

# Load the extra flags h-config.sh wrote (word-split intentionally so each
# token becomes its own argv entry).
EXTRA_FLAGS=""
EXTRA_FLAGS_FILE="$(dirname "$CONF")/extra-flags"
[ -f "$EXTRA_FLAGS_FILE" ] && EXTRA_FLAGS="$(cat "$EXTRA_FLAGS_FILE")"

# ── SP2: csd-relay-node — canonical-anchor relay ─────────────────────────────
# Must launch BEFORE exec (exec replaces this shell; once exec runs we cannot
# background anything). The relay runs as an orphan owned by HiveOS init; it is
# naturally killed and re-spawned when HiveOS restarts the miner slot.
#
# Resource cap (nice+ionice only; taskset intentionally absent — see header):
#   nice -n 19   lowest user scheduling priority
#   ionice -c 3  idle I/O class (disk only when nothing else is queued)
#
# SP2 relay-node launch args (REAL flags confirmed against binary):
#   --rpc             127.0.0.1:18645         local RPC port
#   --datadir         /var/lib/csd-relay       relay chain data directory
#   --wallet          /var/lib/csd-relay/wallet.json  placeholder wallet (required by binary)
#   --peer-seeds      <comma-sep multiaddrs>   well-known honest peers
#   --p2p-listen      /ip4/0.0.0.0/tcp/18644  p2p listen (multiaddr)
#   CSD_RELAY_BLACKLIST_ADDR20 env             addr20 blacklist file path (node writes it)
#   CSD_BLACKLIST_URL env                      signed-blacklist source; ENABLES the node's
#                                              built-in 15-min Ed25519-signed fetcher (pulls,
#                                              verifies, writes the addr20 file fail-closed)
#   CSD_CANONICAL_TIP_URL env                 canonical oracle
#   CSD_CANON_REORG_AHEAD env                 SP1.1 auth-reorg depth (= 7)
#
# *** OPERATOR ACTION REQUIRED ***
# Replace the --peer-seeds value below with real operator seed multiaddrs.
# The placeholder seeds below are from the anchor.service peer-seeds list.
# TODO(operator): confirm wallet-new subcommand name (`csd-relay-node wallet new --out <path>`)
# against `csd-relay-node --help`; update if it differs.
#
RELAY_BIN="$(dirname "$0")/csd-relay-node"
RELAY_DATADIR="/var/lib/csd-relay"
RELAY_WALLET="$RELAY_DATADIR/wallet.json"
RELAY_BLACKLIST="$RELAY_DATADIR/blacklist.txt"
RELAY_LOG="/var/log/miner/csd-pool-miner/csd-relay-node.log"
mkdir -p "$RELAY_DATADIR" "$(dirname "$RELAY_LOG")"

if [ -x "$RELAY_BIN" ]; then
  # Orphan cleanup: kill any leftover relay from a previous HiveOS slot restart
  # before we launch a fresh one (avoids port conflicts on :18645/:18644).
  pkill -f csd-relay-node 2>/dev/null || true

  # Wallet: required by the binary even when not mining. Generate a throwaway
  # placeholder on first run.
  if [ ! -f "$RELAY_WALLET" ]; then
    echo "[h-run] SP2: relay wallet absent — generating placeholder wallet..." | tee -a "$LOG"
    # TODO(operator): confirm exact subcommand against `csd-relay-node --help`.
    if "$RELAY_BIN" wallet new --out "$RELAY_WALLET" >> "$RELAY_LOG" 2>&1; then
      echo "[h-run] SP2: relay wallet created at $RELAY_WALLET" | tee -a "$LOG"
    else
      echo "[h-run] SP2: WARNING — wallet generation failed; relay may refuse to start. Check $RELAY_LOG." | tee -a "$LOG"
    fi
  fi

  echo "[h-run] SP2: launching csd-relay-node (nice 19 / ionice idle)" | tee -a "$LOG"
  CSD_RELAY_BLACKLIST_ADDR20="$RELAY_BLACKLIST" \
  CSD_BLACKLIST_URL="https://lisens.yamaduo.no/blacklist" \
  CSD_CANONICAL_TIP_URL="https://explorer.computesubstrate.org" \
  CSD_CANON_REORG_AHEAD="7" \
  nice -n 19 ionice -c 3 \
    "$RELAY_BIN" \
    --rpc 127.0.0.1:18645 \
    --datadir "$RELAY_DATADIR" \
    --wallet "$RELAY_WALLET" \
    --peer-seeds /ip4/81.167.197.88/tcp/17999/p2p/12D3KooWA2GFgHLyXSZFVnzuchdesWhqnu7HWw637RXF9P6vW6zK,/ip4/141.94.163.242/tcp/18007/p2p/12D3KooWKGhuUhAwGDf3MtqL581h3gttvFg9Z2p1ej9wFTdKfdSM,/ip4/135.125.170.218/tcp/18007/p2p/12D3KooWSDqQj345ir2Ak5TUKHMn3wPTNsdJCbfPVq66aac29nKt \
    --p2p-listen /ip4/0.0.0.0/tcp/18644 \
    >> "$RELAY_LOG" 2>&1 &
  echo "[h-run] SP2: csd-relay-node PID=$! (log: $RELAY_LOG)" | tee -a "$LOG"
else
  echo "[h-run] SP2: csd-relay-node not found at $RELAY_BIN — skipping relay launch." | tee -a "$LOG"
fi
# ── end SP2 relay launch ──────────────────────────────────────────────────────

# Driver check: if the flightsheet asks for a GPU backend, make sure the driver
# is actually present, else log a clear line. We do NOT hard-exit (auto-fallback
# in the binary will drop to CPU), but the operator sees why.
case "$EXTRA_FLAGS" in
  *"--backend cuda"*)
    if ! command -v nvidia-smi >/dev/null 2>&1 || ! nvidia-smi -L >/dev/null 2>&1; then
      echo "[h-run] WARNING: --backend cuda requested but nvidia-smi found no GPU; the miner will auto-fall back to CPU." | tee -a "$LOG"
    fi
    ;;
  *"--backend opencl"*)
    if ! command -v clinfo >/dev/null 2>&1 || ! clinfo 2>/dev/null | grep -q 'Device Type.*GPU'; then
      echo "[h-run] WARNING: --backend opencl requested but clinfo listed no GPU; the miner will auto-fall back to CPU." | tee -a "$LOG"
    fi
    ;;
esac

echo "[h-run] starting csd-gpu-miner (stats on 127.0.0.1:$PORT)" | tee -a "$LOG"

# EXEC the real binary. Word-splitting $EXTRA_FLAGS is intentional (each token
# is a separate argv entry), so shellcheck SC2086 is suppressed for that one
# expansion only.
# shellcheck disable=SC2086
exec "$CUSTOM_BIN" --config "$CONF" \
  --stats-port "$PORT" --stats-bind 127.0.0.1 \
  $EXTRA_FLAGS
