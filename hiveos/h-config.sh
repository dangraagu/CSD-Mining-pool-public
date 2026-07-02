#!/usr/bin/env bash
# HiveOS custom-miner config renderer for csd-pool-miner (P4 §1).
#
# HiveOS calls this before h-run.sh to turn the flightsheet fields into the
# miner's config + run flags. The flightsheet exposes these variables (HiveOS
# substitutes %WAL% etc. into them before sourcing the manifest):
#   $CUSTOM_TEMPLATE      the "wallet and worker template" box -> our addr20
#   $CUSTOM_USER_CONFIG   the free-form "extra config arguments" box
#                                                               -> appended verbatim
# The flightsheet "pool URL" box ($CUSTOM_URL) is intentionally ignored: the
# pool endpoint is compiled into the binary and cannot be repointed.
# We emit two files that h-run.sh consumes:
#   $CUSTOM_CONFIG_FILENAME   a TOML config holding the address
#   <config dir>/extra-flags  the extra CLI flags, one shell-word per token
#
# POSIX-sh + shellcheck-clean.

cd "$(dirname "$0")" || exit 1
# shellcheck source=/dev/null
[ -e h-manifest.conf ] && . ./h-manifest.conf

CONF="${CUSTOM_CONFIG_FILENAME:-config.toml}"
CONF_DIR="$(dirname "$CONF")"
mkdir -p "$CONF_DIR"

# Strip ONE matched leading+trailing quote (a pair of single OR a pair of double
# quotes) from $1, echoing the result. HiveOS operators often paste the address
# QUOTED — e.g. --address "<40hex>" or in the wallet box as "<40hex>" — and a
# literal quote left in the value makes the miner's strict 40-hex check fail
# ("got 42 chars" / non-hex). Only a MATCHED pair is removed (so a stray single
# quote is left for the hex guard to reject), and removal happens BEFORE the
# 0x/.worker normalisation below. POSIX-sh, no bashisms.
_strip_one_quote() {
  _q="$1"
  case "$_q" in
    \"*\") _q="${_q#\"}"; _q="${_q%\"}" ;;   # "value" -> value
    \'*\') _q="${_q#\'}"; _q="${_q%\'}" ;;   # 'value' -> value
  esac
  printf '%s' "$_q"
}

# The flightsheet "wallet and worker template" is the addr20 payout address. Put
# the LITERAL CSD address here, NOT %WAL% — CSD is not a HiveOS-known coin, so the
# %WAL% macro is never expanded and would leave the address empty. An optional
# ".worker" suffix (HiveOS appends %WORKER_NAME%) is dropped. Strip whitespace +
# a 0x prefix; the miner validates the result strictly (40 hex, or 42 with 0x).
ADDR="$(printf '%s' "${CUSTOM_TEMPLATE:-}" | tr -d '[:space:]')"
ADDR="$(_strip_one_quote "$ADDR")"  # strip a matched leading+trailing "" or '' pair
ADDR="${ADDR%%.*}"   # drop a ".worker" suffix, if any
ADDR="${ADDR#0[xX]}" # drop a 0x/0X prefix, if any
# If any HiveOS macro (%WAL%, %WORKER_NAME%, …) was left UNEXPANDED, blank it so
# the miner emits its clear "address must be 40 hex chars" error rather than
# trying to mine to a literal "%WAL%".
case "$ADDR" in *%*) ADDR="" ;; esac

# FALLBACK — recover the address from the "extra config arguments" box. HiveOS does
# NOT populate the wallet template ($CUSTOM_TEMPLATE) for a custom miner that has no
# HiveOS-known coin/wallet, so a coinless setup (CSD) cannot supply the address via
# the wallet field — confirmed on real rigs. But the extra-args box IS passed
# ($CUSTOM_USER_CONFIG), so if the operator put `--address <addr>` there, take it.
# This makes the documented `--address <addr>` workaround actually populate the
# config (the miner validates the config address, so a blank config kills it even
# when --address is ALSO on the CLI).
if [ -z "$ADDR" ]; then
  _flags="$(printf '%s' "${CUSTOM_USER_CONFIG:-}" | tr '\n\t' '  ')"
  case "$_flags" in
    *--address*)
      _a="$(printf '%s' "$_flags" | sed -n 's/.*--address[ =]*\([^ ]*\).*/\1/p')"
      _a="$(_strip_one_quote "$_a")"  # strip a matched leading+trailing "" or '' pair
      _a="${_a%%.*}"      # drop a ".worker" suffix, if any
      _a="${_a#0[xX]}"    # drop a 0x/0X prefix, if any
      case "$_a" in
        '' | *[!0-9a-fA-F]*) : ;;   # not clean hex → ignore
        *) ADDR="$_a" ;;
      esac
      ;;
  esac
fi

# IDEMPOTENCY GUARD (v0.1.18 P0). HiveOS runs h-config.sh WITH the flight-sheet env
# to bake config.toml, then runs h-run.sh — which does NOT receive
# $CUSTOM_USER_CONFIG / $CUSTOM_TEMPLATE. If h-run.sh (or anything) re-invokes us
# env-less, ADDR is empty here; recover the address already baked into config.toml
# instead of CLOBBERING it with "" (which made the miner exit "got 0 chars"). A
# second env-less call must be a no-op, never a blank.
if [ -z "$ADDR" ] && [ -f "$CONF" ]; then
  _prev="$(sed -n 's/^address = "\([^"]*\)".*/\1/p' "$CONF" 2>/dev/null | head -1)"
  case "$_prev" in
    '' | *[!0-9a-fA-F]*) : ;;                       # absent / not clean hex -> ignore
    *) [ "${#_prev}" -eq 40 ] && ADDR="$_prev" ;;   # a valid 40-hex addr -> recover
  esac
fi

# Rig/worker name (v0.2.0): HiveOS exports $WORKER_NAME (the rig's name) into
# the env h-config runs under. Sanitize with the SAME rule as the binary (cut
# at the first '.', keep only [A-Za-z0-9_-], max 24 chars) and bake
# `worker = "<name>"` next to the address, so the miner authorizes as
# "<address>.<name>" for per-rig dashboard stats. DISPLAY-ONLY — payouts always
# go to the bare address. When nothing valid survives, NO worker line is
# written and the binary's own env/hostname fallback applies.
WNAME="$(printf '%s' "${WORKER_NAME:-}" | tr -d '[:space:]')"
WNAME="${WNAME%%.*}"   # cut at the first '.' (FQDN-ish rig names)
WNAME="$(printf '%s' "$WNAME" | tr -cd 'A-Za-z0-9_-' | cut -c1-24)"

# WORKER IDEMPOTENCY (mirrors the address recovery above): an env-less re-run
# (h-run.sh's backstop re-invocation has no $WORKER_NAME) must PRESERVE a
# previously-baked worker line, never blank it.
if [ -z "$WNAME" ] && [ -f "$CONF" ]; then
  _prevw="$(sed -n 's/^worker = "\([^"]*\)".*/\1/p' "$CONF" 2>/dev/null | head -1)"
  case "$_prevw" in
    '' | *[!A-Za-z0-9_-]*) : ;;                       # absent / not clean -> ignore
    *) [ "${#_prevw}" -le 24 ] && WNAME="$_prevw" ;;  # a valid baked name -> recover
  esac
fi

# Build the TOML config. address is the only required key; the pool endpoint is
# compiled into the binary and cannot be overridden from the flightsheet.
{
  echo "# Auto-generated by HiveOS h-config.sh — do not edit by hand."
  echo "address = \"$ADDR\""
  if [ -n "$WNAME" ]; then
    echo "worker = \"$WNAME\""
  fi
} > "$CONF"

# Extra run flags for h-run.sh: whatever the operator typed in the "extra config
# arguments" box verbatim (e.g. --backend cuda --cpu-threads 0 --gpu-id 0,1).
# h-run.sh forces --stats-port/--stats-bind so h-stats.sh can always scrape the
# miner on 127.0.0.1:$CUSTOM_API_PORT. clap is last-value-wins, so an operator
# who pastes --stats-port/--stats-bind into the extra-args box would otherwise
# override the forced value (miner serves stats on a port h-stats doesn't scrape
# -> dashboard shows 0 H/s; or --stats-bind 0.0.0.0 undoes loopback-only). Strip
# both flags (and any "=value" form) out here so they CANNOT be overridden.
EXTRA_FLAGS_FILE="$CONF_DIR/extra-flags"
if [ -n "${CUSTOM_USER_CONFIG:-}" ]; then
  # If the operator NAMED a --backend, sanity-check the token: valid ones are
  # auto|cuda|opencl|cpu (auto = explicit "let the rig detect"). A typo (e.g.
  # --backend nvidia / --backend gpu) is NOT mapped by h-run.sh's update_variant,
  # so it falls through to auto-detect — which is usually fine, but the operator
  # likely meant a specific backend, so warn so they can fix the typo rather than
  # silently getting auto-detect. Handles both the space and the =value forms.
  _be="$(printf '%s' "$CUSTOM_USER_CONFIG" | tr '\n\t' '  ' \
          | sed -n 's/.*--backend[ =]\{1,\}\([^ ]*\).*/\1/p')"
  if [ -n "$_be" ]; then
    case "$_be" in
      auto|cuda|opencl|cpu) : ;;  # recognised
      *) echo "h-config: WARNING — unrecognised --backend '$_be' (expected one of: auto, cuda, opencl, cpu). The rig will AUTO-DETECT the GPU instead; fix the token if you meant a specific backend." ;;
    esac
  fi
  # Flight-sheet extras present this run -> (re)write them, stripping the forced
  # --stats-port/--stats-bind so they cannot be overridden.
  {
    printf '%s' "$CUSTOM_USER_CONFIG" \
      | sed -E 's/(^|[[:space:]])--stats-port([[:space:]]+|=)[^[:space:]]*/\1/g; s/(^|[[:space:]])--stats-bind([[:space:]]+|=)[^[:space:]]*/\1/g'
    printf '\n'
  } > "$EXTRA_FLAGS_FILE"
elif [ ! -s "$EXTRA_FLAGS_FILE" ]; then
  # No extras AND no prior non-empty file -> create an empty one for h-run to read.
  printf '\n' > "$EXTRA_FLAGS_FILE"
fi
# else: an env-less re-run with extras already baked -> PRESERVE them (same
# idempotency guarantee as the address recovery above). Without this, the dropped
# --backend made the variant-aware updater fetch the CPU build instead of the GPU one.

if [ -z "$ADDR" ]; then
  echo "h-config: WARNING — empty address. HiveOS does not pass the 'Wallet and worker template' field for a coinless custom miner (CSD), so set your CSD addr20 (40-hex) via '--address <addr>' in the 'Extra config arguments' box. The miner refuses to start until this is set."
else
  echo "h-config: wrote $CONF (address=$ADDR${WNAME:+ worker=$WNAME}) and $EXTRA_FLAGS_FILE"
fi
