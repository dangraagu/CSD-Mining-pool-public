#!/usr/bin/env bash
set -euo pipefail

# ============================================================================
#  csd-pool-miner — systemd auto-update helper (FIX #1).
#
#  The packaged systemd units (csd-pool-miner.service / csd-pool-miner@.service)
#  run the BARE binary with no update path, so a systemd rig stays frozen on
#  whatever version was installed. This oneshot script — driven by
#  csd-pool-miner-update.timer every ~15 min — keeps it current using the SAME
#  proven logic the mine-auto.* launchers use:
#    1. resolve "latest" from the releases/latest/download/ CDN asset
#       latest-version.txt (NOT the 60-req/hr api.github.com endpoint that
#       silently freezes whole farms behind one public IP),
#    2. numeric semver gate via the binary's own `check-update`,
#    3. download to a TEMP path — never onto the live binary,
#    4. SHA-256 verify the temp against the release SHA256SUMS with a TRUSTED
#       verifier (the installed binary's `verify-file`, else OS sha256sum)
#       BEFORE the atomic swap; FAIL CLOSED on a missing SHA256SUMS / unlisted
#       asset / mismatch / no verifier,
#    5. only on a verified swap: `systemctl restart csd-pool-miner` (and any
#       enabled csd-pool-miner@<i> instances).
#
#  FAIL-SAFE IS RULE #1. This script is SEPARATE from the miner unit. ANY
#  failure (no network, rate-limit, 404, SHA mismatch, disk full, missing
#  verifier) logs and exits 0 — it NEVER stops, restarts, or otherwise disturbs
#  the running miner, and it NEVER swaps in an unverified binary. A failed update
#  simply leaves the rig mining on its existing, known-good binary. The script
#  only ever touches the miner via `systemctl restart` AFTER a verified swap.
#
#  Env knobs (set in /etc/csd-pool-miner.env, read by the .service):
#    CSD_UPDATE_VARIANT   nvidia | amd | cpu   asset to track (default: auto-
#                         detect, else cpu). A non-cpu 404 falls back to cpu.
#    CSD_BIN              path to the installed binary
#                         (default /usr/local/bin/csd-pool-miner)
#    CSD_UPDATE_REPO      GitHub owner/repo (default dangraagu/CSD-Mining-pool-public)
# ============================================================================

REPO="${CSD_UPDATE_REPO:-dangraagu/CSD-Mining-pool-public}"
BIN="${CSD_BIN:-/usr/local/bin/csd-pool-miner}"
SERVICE="csd-pool-miner"            # base unit to restart after a verified swap
STAGE_DIR="${CSD_UPDATE_STAGEDIR:-/var/lib/csd-pool-miner}"
mkdir -p "$STAGE_DIR" 2>/dev/null || true

log() { echo "[csd-update] $*"; }

# Resolve the asset variant to track. Explicit env wins; otherwise auto-detect a
# GPU (best-effort) and fall back to cpu, which is always published.
resolve_variant() {
  case "${CSD_UPDATE_VARIANT:-}" in
    nvidia|amd|cpu) echo "$CSD_UPDATE_VARIANT"; return ;;
  esac
  if command -v nvidia-smi >/dev/null 2>&1 && nvidia-smi -L >/dev/null 2>&1; then
    echo "nvidia"
  elif command -v clinfo >/dev/null 2>&1 && clinfo 2>/dev/null | grep -q 'Device Type.*GPU'; then
    echo "amd"
  else
    echo "cpu"
  fi
}

# Download $1 -> $2 atomically (temp then move), curl or wget. Non-zero on fail.
download() {
  local url="$1" out="$2" tmp="$2.dl"
  if command -v curl >/dev/null 2>&1; then
    curl -fL --retry 3 -o "$tmp" "$url" && mv "$tmp" "$out"
  elif command -v wget >/dev/null 2>&1; then
    wget -O "$tmp" "$url" && mv "$tmp" "$out"
  else
    return 1
  fi
}

# FIX #8: latest version from the CDN asset (NOT api.github.com). Bare version,
# no leading 'v'. Empty on offline/404 → caller no-ops (keeps the miner as-is).
latest_version() {
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

# Installed version string for the semver compare; "none" if unreadable.
installed_version() {
  local v
  if [ -x "$BIN" ]; then
    v="$("$BIN" --version 2>/dev/null | grep -Eo '[0-9]+\.[0-9]+\.[0-9]+' | head -n1)"
    [ -n "$v" ] && { echo "$v"; return; }
  fi
  echo "none"
}

# Is $2 (latest) newer than $1 (installed)? Prefer the binary's own numeric
# check-update; string fallback (v-stripped) only when no usable binary exists.
should_update() {
  local installed="$1" latest="$2"
  if [ -x "$BIN" ] && "$BIN" check-update --current "$installed" --latest "$latest" >/dev/null 2>&1; then
    return 0
  fi
  if [ -x "$BIN" ] && "$BIN" check-update --help >/dev/null 2>&1; then
    return 1   # subcommand present, exited non-zero == up-to-date/older
  fi
  [ "${installed#v}" != "${latest#v}" ]
}

# Expected SHA-256 hex for asset $1 from the release SHA256SUMS (empty if no
# checksums published or the asset isn't listed → caller fails closed).
expected_sha() {
  local asset="$1" sums="$STAGE_DIR/SHA256SUMS.update.tmp"
  if download "https://github.com/$REPO/releases/latest/download/SHA256SUMS" "$sums" 2>/dev/null; then
    awk -v a="$asset" '$2==a || $2=="*"a {print $1; exit}' "$sums"
    rm -f "$sums"
  fi
}

# Download the matching variant build, VERIFY it, and atomically swap it onto
# $BIN. NEVER writes an unverified binary onto the live path. On a non-cpu 404,
# falls back to the cpu asset. Returns non-zero (leaving $BIN untouched) if no
# verified binary could be staged. Echoes nothing on the happy path.
download_verify_swap() {
  local variant asset staged want got
  variant="$(resolve_variant)"
  asset="csd-pool-miner-linux-$variant"
  staged="$BIN.new"
  if ! download "https://github.com/$REPO/releases/latest/download/$asset" "$staged"; then
    if [ "$variant" != "cpu" ]; then
      log "'$variant' build unavailable (404/failed); falling back to the cpu build."
      asset="csd-pool-miner-linux-cpu"
      download "https://github.com/$REPO/releases/latest/download/$asset" "$staged" || { rm -f "$staged"; return 1; }
    else
      rm -f "$staged"; return 1
    fi
  fi

  want="$(expected_sha "$asset")"
  if [ -z "$want" ]; then
    # FIX #9: FAIL CLOSED. Live releases (v0.1.7+) always publish SHA256SUMS, so a
    # missing one (or the asset not listed) is anomalous — refuse and keep the
    # running binary rather than installing+restarting onto an unverified binary.
    log "refusing unverified update: no SHA256SUMS published (or '$asset' not listed in it). Keeping the running binary."
    rm -f "$staged"; return 1
  fi

  # Verify with a TRUSTED tool ONLY: the installed $BIN's verify-file, else OS
  # sha256sum. NEVER let the just-downloaded file verify itself. With a digest but
  # no trusted verifier → FAIL CLOSED.
  if [ -x "$BIN" ] && "$BIN" verify-file --help >/dev/null 2>&1; then
    if ! "$BIN" verify-file "$staged" "$want" >/dev/null 2>&1; then
      log "SHA-256 verify FAILED for $asset — discarding it, keeping the running binary."
      rm -f "$staged"; return 1
    fi
  elif command -v sha256sum >/dev/null 2>&1; then
    got="$(sha256sum "$staged" | awk '{print $1}')"
    if [ "$got" != "$want" ]; then
      log "SHA-256 verify FAILED for $asset (got $got, want $want) — discarding it."
      rm -f "$staged"; return 1
    fi
  else
    log "have a SHA256SUMS digest but no trusted verifier (no verify-file, no sha256sum) — refusing the update."
    rm -f "$staged"; return 1
  fi

  chmod +x "$staged" 2>/dev/null || true
  mv "$staged" "$BIN"   # atomic swap onto the live path, only after verify
}

# Restart the miner so it picks up the freshly-swapped binary. Restart the base
# unit and any ENABLED templated per-GPU instances. Best-effort: a restart
# hiccup is logged but never fails the script (the new binary is already in
# place; systemd's own Restart=always will bring the miner back regardless).
restart_miner() {
  if systemctl restart "$SERVICE" 2>/dev/null; then
    log "restarted $SERVICE onto the new binary."
  else
    log "WARNING: 'systemctl restart $SERVICE' did not succeed (unit not active?); the new binary is staged and will be used on the next (auto-)restart."
  fi
  # Templated multi-GPU instances, if any are enabled.
  local inst
  for inst in $(systemctl list-units --type=service --state=running --no-legend "${SERVICE}@*" 2>/dev/null | awk '{print $1}'); do
    if systemctl restart "$inst" 2>/dev/null; then
      log "restarted $inst."
    else
      log "WARNING: restart of $inst did not succeed; it will pick up the new binary on its next restart."
    fi
  done
}

main() {
  local latest installed
  latest="$(latest_version || true)"
  if [ -z "$latest" ]; then
    log "no version pointer (offline / 404) — leaving the miner on its installed binary."
    return 0
  fi
  installed="$(installed_version)"
  if ! should_update "$installed" "$latest"; then
    log "up to date (installed=$installed, latest=$latest); nothing to do."
    return 0
  fi
  log "update available: $installed -> $latest (verify, then swap + restart)."
  if download_verify_swap; then
    log "swapped in $latest; restarting the miner."
    restart_miner
  else
    log "update not applied (download/verify refused or failed); miner keeps running on $installed."
  fi
  return 0
}

# Run, but NEVER fail the oneshot: any unexpected error still exits 0 so the
# timer doesn't mark the unit failed and the running miner is never disturbed.
main || true
exit 0
