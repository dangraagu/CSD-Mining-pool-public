#!/usr/bin/env bash
# tests/installer_bootstrap_verify.sh — hermetic tests for the FIRST (bootstrap)
# binary download in install-csd-miner.sh.
#
# THE GAP THIS LOCKS DOWN: the one-shot installer fetches the initial miner
# binary from releases/latest/download/<variant> and historically chmod+x'd /
# handed it off to mine-auto.sh WITHOUT SHA-256 verifying it first (fail-OPEN).
# A tampered or truncated bootstrap binary would have been run. The steady-state
# launcher (mine-auto.sh download_verify_swap) already fails CLOSED; the bootstrap
# must match that discipline: download to a TEMP, look the variant's digest up in
# the release SHA256SUMS, verify with the OS sha256sum, and only move-into-place +
# proceed on a match — otherwise discard and abort, never run the unverified file.
#
# HERMETIC, NO NETWORK: we run the REAL installer but point its release base URL
# (CSD_BASE_URL) at a LOCAL fake-release directory served over file://, feed the
# address via CSD_ADDR (no prompt), and set CSD_INSTALL_NO_EXEC=1 so the installer
# stops right before the mine-auto.sh hand-off. We then assert on the on-disk $BIN:
#   - GOOD  (binary matches SHA256SUMS)         -> installed (present, +x, right bytes)
#   - TAMPER(binary bytes != SHA256SUMS digest) -> ABORTED, $BIN absent (never placed)
#   - TRUNC (interrupted/short download)        -> ABORTED, $BIN absent
#   - NO SUMS / asset not listed in SUMS        -> ABORTED fail-closed, $BIN absent
#   - NO HASHER (sha256sum hidden from PATH)    -> ABORTED fail-closed, $BIN absent
#
# Run:   bash tests/installer_bootstrap_verify.sh
# Exit:  0 = all pass, non-zero = at least one failure printed to stderr

set -uo pipefail

REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
INSTALLER="$REPO_ROOT/install-csd-miner.sh"

PASS=0
FAIL=0
ok()   { echo "  [PASS] $1"; PASS=$((PASS + 1)); }
fail() { echo "  [FAIL] $1" >&2; echo "         $2" >&2; FAIL=$((FAIL + 1)); }

sha_of_file() { sha256sum "$1" 2>/dev/null | awk '{print $1}'; }

# file:// URL (forward-slash Windows path) curl can open on this box.
file_url() { local p; p="$(cygpath -w "$1" 2>/dev/null || printf '%s' "$1")"; printf 'file:///%s' "$(printf '%s' "$p" | sed 's#\\#/#g')"; }

echo
echo "=== installer bootstrap SHA-256 verify (install-csd-miner.sh — fail-closed) ==="
echo

SANDBOX="$(mktemp -d)"
trap 'rm -rf "$SANDBOX"' EXIT

VARIANT="cpu"
BIN_NAME="csd-pool-miner-linux-$VARIANT"

# Run ONE scenario end-to-end against the REAL installer.
#   $1 tag
#   $2 mode: GOOD | TAMPER | TRUNC | NOSUMS | NOTLISTED | NOHASHER
# Builds a fake release dir, points the installer at it via CSD_BASE_URL, and
# returns the on-disk paths for the caller to assert against.
run_case() {
  local tag="$1" mode="$2"
  local work="$SANDBOX/case-$tag"
  rm -rf "$work"; mkdir -p "$work"
  local rel="$work/fake_release"; mkdir -p "$rel"
  local data="$work/data" cfg="$work/cfg"; mkdir -p "$data" "$cfg"

  # The "real" published binary bytes + their true digest.
  printf '#!/bin/sh\necho FAKE-CSD-MINER-%s\n' "$tag" > "$rel/$BIN_NAME"
  local good_sha; good_sha="$(sha_of_file "$rel/$BIN_NAME")"

  # The launcher assets the installer also fetches (must succeed so the run
  # reaches the hand-off gate; content is irrelevant to the bootstrap check).
  printf '#dummy mine-all-gpus.sh\n' > "$rel/mine-all-gpus.sh"
  printf '#dummy mine-auto.sh\n'     > "$rel/mine-auto.sh"

  # Stage SHA256SUMS per the scenario.
  local sums="$rel/SHA256SUMS"
  case "$mode" in
    GOOD|NOHASHER) printf '%s  %s\n' "$good_sha" "$BIN_NAME" > "$sums" ;;
    TAMPER)
      # SUMS holds the digest of the ORIGINAL good bytes, but we then overwrite
      # the served binary with DIFFERENT bytes (the tamper) — classic swap.
      printf '%s  %s\n' "$good_sha" "$BIN_NAME" > "$sums"
      printf '#!/bin/sh\necho TAMPERED-PAYLOAD-%s\n' "$tag" > "$rel/$BIN_NAME"
      ;;
    TRUNC)
      # SUMS holds the FULL digest; serve a truncated (short, non-empty) binary.
      printf '%s  %s\n' "$good_sha" "$BIN_NAME" > "$sums"
      printf '#!/bi' > "$rel/$BIN_NAME"
      ;;
    NOSUMS)    : ;;   # no SHA256SUMS file at all
    NOTLISTED) printf '%s  some-other-asset\n' "$good_sha" > "$sums" ;;  # our asset absent
  esac

  local base; base="$(file_url "$rel")"
  local raw;  raw="$(file_url "$rel")"   # launcher raw fetch also served locally

  # PATH that hides sha256sum only for the NOHASHER case (and a verify-file-less
  # world: no installed BIN yet, so the OS hasher is the only verifier).
  local run_path="$PATH"
  if [ "$mode" = "NOHASHER" ]; then
    local nohash="$work/nohash-bin"; mkdir -p "$nohash"
    # Symlink the few tools the installer needs, but NOT sha256sum/shasum.
    for t in bash sh curl wget cygpath awk grep sed tr printf cat mv rm mkdir chmod cut head dirname env coreutils; do
      local p; p="$(command -v "$t" 2>/dev/null || true)"
      [ -n "$p" ] && ln -sf "$p" "$nohash/$t" 2>/dev/null || true
    done
    run_path="$nohash"
  fi

  # ISOLATION: run a COPY of the installer placed INSIDE the sandbox. The installer
  # computes SCRIPT_DIR from its own path and writes the fetched launchers next to
  # itself; running the copy keeps those writes in $work and NEVER touches the real
  # repo's mine-auto.sh / mine-all-gpus.sh.
  local inst="$work/install-csd-miner.sh"
  cp "$INSTALLER" "$inst"

  # Drive the installer copy, hermetic + no exec + no prompt.
  CSD_BASE_URL="$base" CSD_RAW_BASE_URL="$raw" \
  CSD_ADDR="00112233445566778899aabbccddeeff00112233" \
  CSD_INSTALL_NO_EXEC=1 \
  XDG_DATA_HOME="$data" XDG_CONFIG_HOME="$cfg" \
  PATH="$run_path" \
  bash "$inst" "$VARIANT" >"$work/out.log" 2>&1
  echo "rc=$?" > "$work/rc"

  BIN_PATH="$data/csd-pool-miner/$BIN_NAME"
  GOOD_SHA="$good_sha"
  WORK="$work"
}

# ── GOOD: matching binary is installed ───────────────────────────────────────
run_case good GOOD
if [ -f "$BIN_PATH" ] && [ -x "$BIN_PATH" ] && [ "$(sha_of_file "$BIN_PATH")" = "$GOOD_SHA" ]; then
  ok "GOOD: verified bootstrap binary installed (present, +x, SHA matches SHA256SUMS)"
else
  fail "GOOD install" "BIN present=$( [ -f "$BIN_PATH" ] && echo y || echo n) x=$( [ -x "$BIN_PATH" ] && echo y || echo n) sha=$(sha_of_file "$BIN_PATH") want=$GOOD_SHA; log: $(tail -3 "$WORK/out.log" | tr '\n' '|')"
fi

# ── TAMPER: wrong-SHA binary is rejected, never placed ───────────────────────
run_case tamper TAMPER
if [ ! -e "$BIN_PATH" ]; then
  ok "TAMPER: bootstrap binary with wrong SHA REJECTED — \$BIN never moved into place (fail-closed)"
else
  fail "TAMPER reject" "TAMPERED binary was installed at $BIN_PATH (sha=$(sha_of_file "$BIN_PATH")) — fail-OPEN bug; log: $(tail -4 "$WORK/out.log" | tr '\n' '|')"
fi

# ── TRUNC: interrupted/short download is rejected ────────────────────────────
run_case trunc TRUNC
if [ ! -e "$BIN_PATH" ]; then
  ok "TRUNC: truncated bootstrap download REJECTED — \$BIN never placed (no partial binary left behind)"
else
  fail "TRUNC reject" "TRUNCATED binary left at $BIN_PATH — fail-OPEN; log: $(tail -4 "$WORK/out.log" | tr '\n' '|')"
fi

# ── NOSUMS: no SHA256SUMS published -> fail closed ───────────────────────────
run_case nosums NOSUMS
if [ ! -e "$BIN_PATH" ]; then
  ok "NO SHA256SUMS: bootstrap REFUSED (no checksums to verify against) — \$BIN absent"
else
  fail "NOSUMS reject" "binary installed despite no SHA256SUMS — fail-OPEN; log: $(tail -4 "$WORK/out.log" | tr '\n' '|')"
fi

# ── NOTLISTED: asset missing from SHA256SUMS -> fail closed ──────────────────
run_case notlisted NOTLISTED
if [ ! -e "$BIN_PATH" ]; then
  ok "ASSET NOT LISTED in SHA256SUMS: bootstrap REFUSED — \$BIN absent"
else
  fail "NOTLISTED reject" "binary installed though asset not in SHA256SUMS — fail-OPEN; log: $(tail -4 "$WORK/out.log" | tr '\n' '|')"
fi

# ── NOHASHER: no sha256sum on PATH -> fail closed (never run unverified) ──────
run_case nohasher NOHASHER
if [ ! -e "$BIN_PATH" ]; then
  ok "NO HASHER (sha256sum absent): bootstrap REFUSED rather than run unverified — \$BIN absent"
else
  fail "NOHASHER reject" "binary installed with NO available hasher — fail-OPEN; log: $(tail -4 "$WORK/out.log" | tr '\n' '|')"
fi

echo
echo "========================================"
echo "  Passed: $PASS  Failed: $FAIL"
echo "========================================"
echo
[ "$FAIL" -gt 0 ] && exit 1
exit 0
