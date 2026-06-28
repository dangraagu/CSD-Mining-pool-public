#!/usr/bin/env bash
# HiveOS custom-miner stats reporter for csd-pool-miner (P4 §1).
#
# HiveOS sources this every ~10s and reads two shell variables afterwards:
#   $khs    total hashrate in kH/s (a number)
#   $stats  a JSON object: { hs:[...], hs_units, ar:[...], uptime, algo, ... }
#
# The ENTIRE transform (scrape /1/summary, convert H/s -> kH/s with the correct
# ÷1000 divisor, shape the JSON) lives in the miner's OWN `hiveos-stats`
# subcommand — pure, unit-tested Rust (src/hiveos.rs). That is deliberate: the
# §G7 kH/s-clamp bug ("every card shows 1000 GH") is exactly the kind of error
# that silently reappears in shell jq/awk, so this script does NO arithmetic.
# It just runs the subcommand and pulls khs out of the JSON it prints.
#
# On ANY failure the subcommand itself prints a valid zero h-stats object, so
# the rig reads as alive-but-zero rather than broken.

cd "$(dirname "$0")" || exit 1
# shellcheck source=/dev/null
[ -e h-manifest.conf ] && . ./h-manifest.conf

PORT="${CUSTOM_API_PORT:-3380}"
BIN="${CUSTOM_BIN:-$(dirname "$0")/csd-gpu-miner}"   # install-path-agnostic fallback (we already cd'd here)

# Ask the miner to scrape its own /1/summary and print the HiveOS h-stats JSON.
# The subcommand never hangs and never panics; on error it prints a zero object.
stats="$("$BIN" hiveos-stats --stats-port "$PORT" 2>/dev/null)"

# Compute the total kH/s and finalise $stats. This script is SOURCED by HiveOS
# (it reads $khs/$stats from the calling shell), so we set the two variables
# rather than printing — and use a single if/else (no early return) so the body
# is reachable whether sourced or run standalone.
if [ -z "$stats" ]; then
  # Binary missing or produced nothing → report a zero (but valid) rig.
  khs=0
  stats='{"hs":[0],"hs_units":"khs","temp":[],"fan":[],"uptime":0,"ar":[0,0,0,0],"algo":"sha256d","ver":"0"}'
else
  # Total kH/s = sum of the hs[] array (already in kH/s from the subcommand).
  # Prefer jq; fall back to a tiny awk extractor if jq is absent on the rig.
  if command -v jq >/dev/null 2>&1; then
    khs="$(printf '%s' "$stats" | jq -r '[.hs[]?] | add // 0')"
  else
    # Pull the bracketed hs array and sum its comma-separated numbers.
    khs="$(printf '%s' "$stats" \
      | sed -n 's/.*"hs":\[\([^]]*\)\].*/\1/p' \
      | awk -F',' '{s=0; for(i=1;i<=NF;i++) s+=$i; print s}')"
  fi
  [ -z "$khs" ] && khs=0
fi
