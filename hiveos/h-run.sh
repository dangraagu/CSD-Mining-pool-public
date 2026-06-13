#!/usr/bin/env bash
# HiveOS custom-miner launcher for csd-pool-miner (P4 §1).
#
# HiveOS runs this to start mining. It MUST `exec` the real binary so the
# process argv is `csd-gpu-miner …` and NOT `h-run.sh` — HiveOS refuses to start
# a custom miner whose argv contains h-run.sh. `exec` replaces this shell; we
# never background with `&`.
#
# Forced flags (cannot be overridden from the flightsheet, by design):
#   --stats-port $CUSTOM_API_PORT --stats-bind 127.0.0.1
#     so h-stats.sh can scrape /1/summary on localhost only (never exposed).
# Everything else (address via --config, --pool, --backend, --gpu-id, …) comes
# from h-config.sh's output.

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
