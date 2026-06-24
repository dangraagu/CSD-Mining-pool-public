#!/usr/bin/env bash
# tests/sp2_relay_bundle.sh — SP2 relay-node bundle integration tests
#
# Tests that can be verified without a real csd-relay-node binary:
#   1.  h-run.sh syntax check (bash -n)
#   2.  mine-auto.sh syntax check (bash -n)
#   3.  shellcheck (skipped gracefully if not installed)
#   4.  Real launch flags in h-run.sh:
#         --rpc (not --rpc-bind-addr), --peer-seeds (not --addnode),
#         --p2p-listen (not --p2p-bind), CSD_RELAY_BLACKLIST_ADDR20 env (not --relay-blacklist flag)
#   5.  Real launch flags in mine-auto.sh (same set)
#   6.  NO taskset -c 0 in actual relay launch commands (h-run.sh / mine-auto.sh)
#   7.  nice -n 19 and ionice -c 3 still present (yield caps kept)
#   8.  h-run.sh: pkill csd-relay-node present (orphan cleanup before launch)
#   9.  mine-auto.sh start_miners: relay-already-running guard present
#   10. mine-auto.sh: relay launch uses --rpc BEFORE GPU loop
#   11. h-manifest.conf lists csd-relay-node; taskset absent from resource-cap note
#   12. mine-auto.sh stop_miners kills $RELAY_PID + pkills relay binary
#   13. release.yml SP2 fetch step present + tarball includes csd-relay-node + chmod +x
#   14. release.yml: SHA256 integrity check on relay binary download
#   15. release.yml: FILL_ME_IN hard-gates a tag build (error:: on tag with FILL_ME_IN version)
#   16. RELAY_NODE_VERSION constant present in release.yml
#   17. mine-auto.bat: /LOW /B relay launch; relay stop on update; relay log redirect
#   18. mine-auto.bat: relay-already-running guard (tasklist check)
#   19. CSD_CANONICAL_TIP_URL + CSD_CANON_REORG_AHEAD set on relay env in h-run.sh and mine-auto.sh
#
# Run:   bash tests/sp2_relay_bundle.sh
# Exit:  0 = all pass, non-zero = at least one failure printed to stderr

set -euo pipefail

REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"

PASS=0
FAIL=0

ok() {
  local name="$1"
  echo "  [PASS] $name"
  PASS=$((PASS + 1))
}

fail() {
  local name="$1" reason="$2"
  echo "  [FAIL] $name" >&2
  echo "         $reason" >&2
  FAIL=$((FAIL + 1))
}

assert_contains() {
  local name="$1" file="$2" pattern="$3"
  if grep -qE "$pattern" "$file" 2>/dev/null; then
    ok "$name"
  else
    fail "$name" "'$pattern' not found in $file"
  fi
}

# Byte-exact "does FILE contain exactly the bytes of OTHER FILE" (sha256 compare).
# Used by the ensure_relay hermetic tests to assert the placed relay binary is
# byte-identical to the staged "release" payload.
file_is_string_er() {
  local a b
  a="$(sha256sum "$1" 2>/dev/null | awk '{print $1}')"
  b="$(sha256sum "$2" 2>/dev/null | awk '{print $1}')"
  [ -n "$a" ] && [ "$a" = "$b" ]
}

assert_not_contains() {
  local name="$1" file="$2" pattern="$3"
  if grep -qE "$pattern" "$file" 2>/dev/null; then
    fail "$name" "Unwanted pattern '$pattern' found in $file"
  else
    ok "$name"
  fi
}

echo
echo "=== SP2 relay-node bundle tests (v0.1.9 — real flags + pre-review fixes) ==="
echo

# ── 1+2. Syntax checks ───────────────────────────────────────────────────────
echo "-- Syntax checks --"
if bash -n "$REPO_ROOT/hiveos/h-run.sh" 2>/dev/null; then
  ok "h-run.sh bash -n (syntax clean)"
else
  fail "h-run.sh bash -n" "syntax error in h-run.sh"
fi

if bash -n "$REPO_ROOT/mine-auto.sh" 2>/dev/null; then
  ok "mine-auto.sh bash -n (syntax clean)"
else
  fail "mine-auto.sh bash -n" "syntax error in mine-auto.sh"
fi

# ── 3. shellcheck (optional) ─────────────────────────────────────────────────
echo
echo "-- shellcheck (skipped if not available) --"
if command -v shellcheck >/dev/null 2>&1; then
  if shellcheck -x "$REPO_ROOT/hiveos/h-run.sh" 2>/dev/null; then
    ok "h-run.sh shellcheck"
  else
    fail "h-run.sh shellcheck" "shellcheck reported errors (run shellcheck manually for details)"
  fi
  if shellcheck -x "$REPO_ROOT/mine-auto.sh" 2>/dev/null; then
    ok "mine-auto.sh shellcheck"
  else
    fail "mine-auto.sh shellcheck" "shellcheck reported errors (run shellcheck manually for details)"
  fi
else
  echo "  [SKIP] shellcheck not installed — skipping SC lint (install shellcheck to enable)"
fi

# ── 4. Real flags in h-run.sh ────────────────────────────────────────────────
echo
echo "-- Real relay-node CLI flags in h-run.sh --"
assert_contains "h-run.sh: --rpc flag (real; not --rpc-bind-addr)" \
  "$REPO_ROOT/hiveos/h-run.sh" '\-\-rpc [0-9]'
assert_not_contains "h-run.sh: NO --rpc-bind-addr (old non-existent flag)" \
  "$REPO_ROOT/hiveos/h-run.sh" '\-\-rpc-bind-addr'
assert_contains "h-run.sh: --peer-seeds flag (real; not --addnode)" \
  "$REPO_ROOT/hiveos/h-run.sh" '\-\-peer-seeds'
assert_not_contains "h-run.sh: NO --addnode (non-existent flag)" \
  "$REPO_ROOT/hiveos/h-run.sh" '\-\-addnode'
assert_contains "h-run.sh: --p2p-listen flag (real multiaddr; not --p2p-bind)" \
  "$REPO_ROOT/hiveos/h-run.sh" '\-\-p2p-listen'
assert_not_contains "h-run.sh: NO --p2p-bind (non-existent flag)" \
  "$REPO_ROOT/hiveos/h-run.sh" '\-\-p2p-bind'
assert_not_contains "h-run.sh: NO --relay-blacklist flag (use env instead)" \
  "$REPO_ROOT/hiveos/h-run.sh" '\-\-relay-blacklist'
assert_contains "h-run.sh: CSD_RELAY_BLACKLIST_ADDR20 env set on relay launch" \
  "$REPO_ROOT/hiveos/h-run.sh" 'CSD_RELAY_BLACKLIST_ADDR20='

# ── 5. Real flags in mine-auto.sh ────────────────────────────────────────────
echo
echo "-- Real relay-node CLI flags in mine-auto.sh --"
assert_contains "mine-auto.sh: --rpc flag (real; not --rpc-bind-addr)" \
  "$REPO_ROOT/mine-auto.sh" '\-\-rpc [0-9]'
assert_not_contains "mine-auto.sh: NO --rpc-bind-addr (old non-existent flag)" \
  "$REPO_ROOT/mine-auto.sh" '\-\-rpc-bind-addr'
assert_contains "mine-auto.sh: --peer-seeds flag (real; not --addnode)" \
  "$REPO_ROOT/mine-auto.sh" '\-\-peer-seeds'
assert_not_contains "mine-auto.sh: NO --addnode (non-existent flag)" \
  "$REPO_ROOT/mine-auto.sh" '\-\-addnode'
assert_contains "mine-auto.sh: --p2p-listen flag (real; not --p2p-bind)" \
  "$REPO_ROOT/mine-auto.sh" '\-\-p2p-listen'
assert_not_contains "mine-auto.sh: NO --p2p-bind (non-existent flag)" \
  "$REPO_ROOT/mine-auto.sh" '\-\-p2p-bind'
assert_not_contains "mine-auto.sh: NO --relay-blacklist flag (use env instead)" \
  "$REPO_ROOT/mine-auto.sh" '\-\-relay-blacklist'
assert_contains "mine-auto.sh: CSD_RELAY_BLACKLIST_ADDR20 env set on relay launch" \
  "$REPO_ROOT/mine-auto.sh" 'CSD_RELAY_BLACKLIST_ADDR20='

# ── 6. NO taskset in relay launch lines ──────────────────────────────────────
echo
echo "-- NO taskset in relay launch commands (dropped; harmful on core 0) --"
# taskset is allowed in COMMENTS only; the actual relay exec line must not use it.
# We check the live launch command block (the line with 'nice -n 19 ionice -c 3').
# Strategy: extract lines from the relay launch section and assert no 'taskset'.
# Simpler: assert that the pattern 'taskset' does NOT appear on a non-comment line
# alongside 'nice -n 19' (the launch block). We check the whole file and rely on
# the "NO --rpc-bind-addr" checks above to confirm we're not accidentally looking
# at the old code; the comment-vs-code distinction is enforced by also checking
# that no actual command line (not a # comment) contains taskset near the relay.

# Check that 'taskset' does not appear as a command in h-run.sh (outside comments).
if grep -vE '^\s*#' "$REPO_ROOT/hiveos/h-run.sh" | grep -qE 'taskset'; then
  fail "h-run.sh: NO taskset in non-comment relay launch lines" \
    "Found 'taskset' on a non-comment line; it must only appear in comments"
else
  ok "h-run.sh: taskset absent from non-comment relay launch lines"
fi

if grep -vE '^\s*#' "$REPO_ROOT/mine-auto.sh" | grep -qE 'taskset'; then
  fail "mine-auto.sh: NO taskset in non-comment relay launch lines" \
    "Found 'taskset' on a non-comment line; it must only appear in comments"
else
  ok "mine-auto.sh: taskset absent from non-comment relay launch lines"
fi

# ── 7. nice + ionice still present ───────────────────────────────────────────
echo
echo "-- nice -n 19 and ionice -c 3 present (yield caps kept) --"
assert_contains "h-run.sh: nice -n 19 present"  "$REPO_ROOT/hiveos/h-run.sh"  'nice -n 19'
assert_contains "h-run.sh: ionice -c 3 present" "$REPO_ROOT/hiveos/h-run.sh"  'ionice -c 3'
assert_contains "mine-auto.sh: nice -n 19 present"  "$REPO_ROOT/mine-auto.sh" 'nice -n 19'
assert_contains "mine-auto.sh: ionice -c 3 present" "$REPO_ROOT/mine-auto.sh" 'ionice -c 3'

# ── 8. h-run.sh pkill orphan cleanup ─────────────────────────────────────────
echo
echo "-- h-run.sh: pkill csd-relay-node orphan cleanup --"
assert_contains "h-run.sh: pkill -f csd-relay-node present" \
  "$REPO_ROOT/hiveos/h-run.sh" 'pkill.*csd-relay-node'

# The pkill must precede the exec line (ordering check).
PKILL_LINE=$(grep -n 'pkill.*csd-relay-node' "$REPO_ROOT/hiveos/h-run.sh" | head -1 | cut -d: -f1)
EXEC_LINE=$(grep -n 'exec "\$CUSTOM_BIN"' "$REPO_ROOT/hiveos/h-run.sh" | head -1 | cut -d: -f1)
if [ -n "$PKILL_LINE" ] && [ -n "$EXEC_LINE" ] && [ "$PKILL_LINE" -lt "$EXEC_LINE" ]; then
  ok "h-run.sh: pkill (line $PKILL_LINE) before exec miner (line $EXEC_LINE)"
else
  fail "h-run.sh: pkill ordering" "pkill (line ${PKILL_LINE:-?}) must appear before exec (line ${EXEC_LINE:-?})"
fi

# ── 9. mine-auto.sh relay already-running guard ───────────────────────────────
echo
echo "-- mine-auto.sh: relay already-running guard in start_miners --"
assert_contains "mine-auto.sh: RELAY_PID guard (kill -0 check)" \
  "$REPO_ROOT/mine-auto.sh" 'kill -0.*RELAY_PID|RELAY_PID.*kill -0'
assert_contains "mine-auto.sh: guard skips re-launch when relay running" \
  "$REPO_ROOT/mine-auto.sh" 'already running'

# ── 10. mine-auto.sh relay launch BEFORE GPU loop ────────────────────────────
echo
echo "-- mine-auto.sh relay launch order --"
# Look for the actual relay nice/ionice launch line (no taskset now).
RELAY_START_LINE=$(grep -n 'nice -n 19 ionice -c 3' "$REPO_ROOT/mine-auto.sh" | head -1 | cut -d: -f1)
GPU_LOOP_LINE=$(grep -n 'for i in.*DEVICES' "$REPO_ROOT/mine-auto.sh" | head -1 | cut -d: -f1)
if [ -n "$RELAY_START_LINE" ] && [ -n "$GPU_LOOP_LINE" ] && [ "$RELAY_START_LINE" -lt "$GPU_LOOP_LINE" ]; then
  ok "mine-auto.sh: relay launch (line $RELAY_START_LINE) before GPU loop (line $GPU_LOOP_LINE)"
else
  fail "mine-auto.sh: relay ordering" \
    "relay nice/ionice line (${RELAY_START_LINE:-?}) must precede GPU for-loop (${GPU_LOOP_LINE:-?})"
fi

# The relay launch uses & (background) before the GPU loop.
RELAY_BG_LINE=$(grep -n 'RELAY_LOG.*2>&1 &' "$REPO_ROOT/mine-auto.sh" | head -1 | cut -d: -f1)
if [ -n "$RELAY_BG_LINE" ] && [ -n "$GPU_LOOP_LINE" ] && [ "$RELAY_BG_LINE" -lt "$GPU_LOOP_LINE" ]; then
  ok "mine-auto.sh: relay backgrounded (2>&1 &, line $RELAY_BG_LINE) before GPU loop (line $GPU_LOOP_LINE)"
else
  fail "mine-auto.sh: relay & ordering" \
    "relay 2>&1 & line (${RELAY_BG_LINE:-?}) must precede GPU for-loop (${GPU_LOOP_LINE:-?})"
fi

# ── 11. h-manifest.conf ───────────────────────────────────────────────────────
echo
echo "-- h-manifest.conf layout --"
assert_contains "h-manifest.conf: csd-relay-node in layout" \
  "$REPO_ROOT/hiveos/h-manifest.conf" 'csd-relay-node'
assert_contains "h-manifest.conf: nice -n 19 documented" \
  "$REPO_ROOT/hiveos/h-manifest.conf" 'nice -n 19'
assert_contains "h-manifest.conf: ionice -c 3 documented" \
  "$REPO_ROOT/hiveos/h-manifest.conf" 'ionice -c 3'
# taskset must NOT appear in the resource-cap documentation (it was removed).
assert_not_contains "h-manifest.conf: NO taskset in cap documentation" \
  "$REPO_ROOT/hiveos/h-manifest.conf" 'taskset -c 0'

# ── 12. mine-auto.sh stop_miners kills RELAY_PID ─────────────────────────────
echo
echo "-- mine-auto.sh stop_miners kills relay PID --"
assert_contains "mine-auto.sh: stop_miners kills RELAY_PID" \
  "$REPO_ROOT/mine-auto.sh" 'kill.*RELAY_PID|\$RELAY_PID.*kill'
assert_contains "mine-auto.sh: stop_miners pkills relay binary" \
  "$REPO_ROOT/mine-auto.sh" 'pkill.*RELAY_BIN_NAME'

# ── 13. release.yml: fetch step + tarball + chmod ────────────────────────────
echo
echo "-- release.yml SP2 relay-node integration --"
assert_contains "release.yml: SP2 fetch step present" \
  "$REPO_ROOT/.github/workflows/release.yml" 'csd-relay-node.*SP2|SP2.*csd-relay-node'
assert_contains "release.yml: tarball step copies csd-relay-node" \
  "$REPO_ROOT/.github/workflows/release.yml" 'csd-relay-node'
assert_contains "release.yml: relay binary chmod +x" \
  "$REPO_ROOT/.github/workflows/release.yml" 'chmod.*csd-relay-node'

# ── 14. release.yml: SHA256 integrity check ──────────────────────────────────
echo
echo "-- release.yml: SHA256 integrity check on relay binary --"
assert_contains "release.yml: RELAY_NODE_SHA256 constant defined" \
  "$REPO_ROOT/.github/workflows/release.yml" 'RELAY_NODE_SHA256'
assert_contains "release.yml: sha256sum verification present" \
  "$REPO_ROOT/.github/workflows/release.yml" 'sha256sum.*csd-relay-node|sha256sum dist'
assert_contains "release.yml: SHA256 mismatch error block present" \
  "$REPO_ROOT/.github/workflows/release.yml" 'SHA256 mismatch|sha256 mismatch|mismatch.*csd-relay-node'

# ── 15. release.yml: FILL_ME_IN hard-gates a tag build ───────────────────────
echo
echo "-- release.yml: FILL_ME_IN hard-gates tag builds --"
assert_contains "release.yml: FILL_ME_IN placeholder present" \
  "$REPO_ROOT/.github/workflows/release.yml" 'FILL_ME_IN'
assert_contains "release.yml: tag build hard-errors on FILL_ME_IN version" \
  "$REPO_ROOT/.github/workflows/release.yml" 'GITHUB_REF_TYPE.*tag|refs/tags'
assert_contains "release.yml: error:: emitted for FILL_ME_IN on tag" \
  "$REPO_ROOT/.github/workflows/release.yml" '::error::'

# ── 16. release.yml: RELAY_NODE_VERSION constant ─────────────────────────────
echo
echo "-- release.yml: RELAY_NODE_VERSION constant --"
assert_contains "release.yml: RELAY_NODE_VERSION constant" \
  "$REPO_ROOT/.github/workflows/release.yml" 'RELAY_NODE_VERSION'

# ── 17. mine-auto.bat relay launch + log redirect + relay stop on update ─────
echo
echo "-- mine-auto.bat Windows relay launch --"
assert_contains "mine-auto.bat: start /LOW /B relay" \
  "$REPO_ROOT/mine-auto.bat" '/LOW.*RELAY_BIN|/LOW /B.*RELAY|RELAY.*\/LOW'
assert_contains "mine-auto.bat: relay log redirect present" \
  "$REPO_ROOT/mine-auto.bat" 'RELAY_LOG'
assert_contains "mine-auto.bat: relay stop on update (taskkill RELAY_EXE)" \
  "$REPO_ROOT/mine-auto.bat" 'taskkill.*RELAY_EXE'
# Real flags in bat
assert_not_contains "mine-auto.bat: NO --rpc-bind-addr" \
  "$REPO_ROOT/mine-auto.bat" '\-\-rpc-bind-addr'
assert_contains "mine-auto.bat: --rpc flag (real)" \
  "$REPO_ROOT/mine-auto.bat" '\-\-rpc 127'
assert_not_contains "mine-auto.bat: NO --addnode" \
  "$REPO_ROOT/mine-auto.bat" '\-\-addnode'
assert_contains "mine-auto.bat: --peer-seeds (real)" \
  "$REPO_ROOT/mine-auto.bat" '\-\-peer-seeds'
assert_not_contains "mine-auto.bat: NO --p2p-bind" \
  "$REPO_ROOT/mine-auto.bat" '\-\-p2p-bind'
assert_contains "mine-auto.bat: --p2p-listen (real)" \
  "$REPO_ROOT/mine-auto.bat" '\-\-p2p-listen'
assert_contains "mine-auto.bat: CSD_RELAY_BLACKLIST_ADDR20 env set" \
  "$REPO_ROOT/mine-auto.bat" 'CSD_RELAY_BLACKLIST_ADDR20'

# ── 18. mine-auto.bat relay-already-running guard ────────────────────────────
echo
echo "-- mine-auto.bat: relay-already-running guard --"
assert_contains "mine-auto.bat: tasklist guard for relay" \
  "$REPO_ROOT/mine-auto.bat" 'tasklist.*RELAY_EXE|IMAGENAME eq.*RELAY_EXE'

# ── 19. CSD env vars on relay launch ─────────────────────────────────────────
echo
echo "-- CSD_CANONICAL_TIP_URL + CSD_CANON_REORG_AHEAD on relay env --"
assert_contains "h-run.sh: CSD_CANONICAL_TIP_URL set" \
  "$REPO_ROOT/hiveos/h-run.sh" 'CSD_CANONICAL_TIP_URL='
assert_contains "h-run.sh: CSD_CANON_REORG_AHEAD set" \
  "$REPO_ROOT/hiveos/h-run.sh" 'CSD_CANON_REORG_AHEAD='
assert_contains "mine-auto.sh: CSD_CANONICAL_TIP_URL set" \
  "$REPO_ROOT/mine-auto.sh" 'CSD_CANONICAL_TIP_URL='
assert_contains "mine-auto.sh: CSD_CANON_REORG_AHEAD set" \
  "$REPO_ROOT/mine-auto.sh" 'CSD_CANON_REORG_AHEAD='
assert_contains "mine-auto.bat: CSD_CANONICAL_TIP_URL set" \
  "$REPO_ROOT/mine-auto.bat" 'CSD_CANONICAL_TIP_URL'
assert_contains "mine-auto.bat: CSD_CANON_REORG_AHEAD set" \
  "$REPO_ROOT/mine-auto.bat" 'CSD_CANON_REORG_AHEAD'

# ── 20. `node` subcommand precedes relay flags (REGRESSION: fix/relay-launch-node-subcmd) ─
# The relay binary (csd-node) is subcommand-based: --rpc/--peer-seeds/--push-peers-file
# belong under the `node` subcommand. Without `node` as the first arg, the binary exits 2
# ("unexpected argument '--rpc'") and the relay NEVER STARTS fleet-wide. These assertions
# lock in: (a) `node` appears as its own arg BEFORE --rpc and --peer-seeds in every launcher,
# (b) the required --push-peers-file flag is present, (c) all 6 real seed multiaddrs are
# present, (d) the broken "binary immediately followed by --rpc" shape is absent.
echo
echo "-- node subcommand precedes relay flags + push-peers-file + 6 seeds --"

# assert_before NAME FILE EARLIER_PATTERN LATER_PATTERN — both must exist and EARLIER must
# be on a strictly lower line number than LATER (first match of each).
assert_before() {
  local name="$1" file="$2" earlier="$3" later="$4"
  local el ll
  el=$(grep -nE "$earlier" "$file" 2>/dev/null | head -1 | cut -d: -f1)
  ll=$(grep -nE "$later"   "$file" 2>/dev/null | head -1 | cut -d: -f1)
  if [ -n "$el" ] && [ -n "$ll" ] && [ "$el" -lt "$ll" ]; then
    ok "$name (node@$el before flag@$ll)"
  else
    fail "$name" "expected '$earlier' (line ${el:-MISSING}) before '$later' (line ${ll:-MISSING})"
  fi
}

# h-run.sh: the relay launch block is anchored by `nice -n 19 ionice -c 3`; `node` must
# appear after the relay binary and before --rpc/--peer-seeds. We scope to the relay launch
# region by searching from the nice/ionice anchor onward.
for f in "hiveos/h-run.sh:    node \\\\:h-run.sh" "mine-auto.sh:      node \\\\:mine-auto.sh"; do
  path="${f%%:*}"; rest="${f#*:}"; nodepat="${rest%%:*}"; label="${rest##*:}"
  # `node \` standalone line present in the relay launch
  assert_contains "$label: standalone 'node' arg line in relay launch" \
    "$REPO_ROOT/$path" '^[[:space:]]+node \\$'
  # node line precedes the relay --rpc and --peer-seeds
  assert_before "$label: node precedes --rpc" "$REPO_ROOT/$path" '^[[:space:]]+node \\$' '^[[:space:]]+--rpc 127'
  assert_before "$label: node precedes --peer-seeds" "$REPO_ROOT/$path" '^[[:space:]]+node \\$' '^[[:space:]]+--peer-seeds /ip4'
  # required --push-peers-file flag present in launch
  assert_contains "$label: --push-peers-file flag in relay launch" \
    "$REPO_ROOT/$path" '^[[:space:]]+--push-peers-file '
  # the broken shape (relay binary line immediately followed by --rpc, no node) must be ABSENT.
  # grep is line-based, so use -A1 on the "$RELAY_BIN" \ line and confirm the NEXT line is not --rpc.
  if grep -A1 '"\$RELAY_BIN" \\$' "$REPO_ROOT/$path" 2>/dev/null | grep -qE '^[[:space:]]+--rpc'; then
    fail "$label: NO broken '\$RELAY_BIN then --rpc' (missing node)" \
      "the relay binary line is immediately followed by --rpc — 'node' subcommand is missing"
  else
    ok "$label: relay binary line not immediately followed by --rpc (node present)"
  fi
done

# mine-auto.bat: relay launch anchored by `start ... /LOW /B ... !RELAY_BIN!`; `node ^`
# must precede --rpc / --peer-seeds.
assert_contains "mine-auto.bat: standalone 'node' arg line in relay launch" \
  "$REPO_ROOT/mine-auto.bat" '^[[:space:]]+node \^'
assert_before "mine-auto.bat: node precedes --rpc" \
  "$REPO_ROOT/mine-auto.bat" '^[[:space:]]+node \^' '^[[:space:]]+--rpc 127'
assert_before "mine-auto.bat: node precedes --peer-seeds" \
  "$REPO_ROOT/mine-auto.bat" '^[[:space:]]+node \^' '^[[:space:]]+--peer-seeds /ip4'
assert_contains "mine-auto.bat: --push-peers-file flag in relay launch" \
  "$REPO_ROOT/mine-auto.bat" '^[[:space:]]+--push-peers-file '
# bat: the binary launch line must NOT be immediately followed by --rpc (i.e. node missing).
if grep -A1 'RELAY_BIN!" \^' "$REPO_ROOT/mine-auto.bat" 2>/dev/null | grep -qE '^[[:space:]]+--rpc'; then
  fail "mine-auto.bat: NO broken '!RELAY_BIN! ^ then --rpc' (missing node)" \
    "the relay binary launch line is immediately followed by --rpc — 'node' subcommand is missing"
else
  ok "mine-auto.bat: relay binary launch line not immediately followed by --rpc (node present)"
fi

# All 6 real seed multiaddrs must be present in every launcher (the 3 NEW ones below were
# previously missing). We check the 3 newly-added PeerIds specifically.
echo
echo "-- all 6 seed multiaddrs present (3 were previously missing) --"
for newseed in \
  '12D3KooWLydGAnXtXH4L37gVZWohAZNvKdFgHwVN4nhUzgrvX8cW' \
  '12D3KooWHKcjL8M5snr3GniC8xRtGJGbGhPSdGiqtZNRz6UFj1t3' \
  '12D3KooWFsHa5ifqK45Fjd8cYnDkVDN8R8MfjfiETNpEqnbGAEez' ; do
  assert_contains "h-run.sh: seed $newseed present"     "$REPO_ROOT/hiveos/h-run.sh"  "$newseed"
  assert_contains "mine-auto.sh: seed $newseed present" "$REPO_ROOT/mine-auto.sh"     "$newseed"
  assert_contains "mine-auto.bat: seed $newseed present" "$REPO_ROOT/mine-auto.bat"   "$newseed"
done

# ── 21. ensure_relay(): standalone relay auto-install (HERMETIC) ──────────────
# The standalone Linux launcher must now AUTO-DOWNLOAD + SHA-verify + start the
# relay binary (csd-relay-node), not just print "not found". These tests source
# mine-auto.sh with CSD_SOURCE_ONLY=1 (so the prompt/GPU-probe/mining-loop don't
# run), then drive the new idempotent `ensure_relay` function against a LOCAL
# fake "release" dir. The single `download()` override controls BOTH the relay
# fetch and the SHA256SUMS lookup (expected_sha uses download() too). We force
# the OS-sha256sum verifier path (BIN → a stub whose `verify-file --help` exits
# non-zero), exercising the SAME trusted-verifier discipline $BIN uses.
#
# THREE BINDING RULES under test:
#   1. BEST-EFFORT: a relay download/SHA/start failure NEVER blocks the miner.
#      Each behavioural case (R1-R5) drives `ensure_relay || true` followed by a
#      MINER stub that drops a marker; the marker MUST exist in every case
#      (proves the miner launch was reached even when the relay path fails).
#   2. FAIL-CLOSED: never chmod+exec an unverified / SHA-mismatched relay binary.
#   3. OPT-OUT: CSD_NO_RELAY=1 skips download AND start with a clear message.
echo
echo "-- ensure_relay(): standalone relay auto-install (hermetic) --"

LAUNCHER="$REPO_ROOT/mine-auto.sh"

# A stub "$BIN": present + executable but does NOT understand `verify-file`
# (its --help exits non-zero) → ensure_relay falls through to OS sha256sum (the
# path a standalone rig without a verify-file-capable binary actually takes).
ER_SANDBOX="$(mktemp -d)"
ER_BIN_STUB="$ER_SANDBOX/bin-stub"
printf '#!/usr/bin/env bash\nexit 2\n' > "$ER_BIN_STUB"
chmod +x "$ER_BIN_STUB"

# Shim dir: stub `nice` and `ionice` so the relay-start block (which wraps the
# relay in `nice -n 19 ionice -c 3 …`) runs hermetically — ionice/pkill do NOT
# exist on Git Bash for Windows. Each shim strips its own flags then execs the
# remaining argv (so `nice -n 19 ionice -c 3 RELAY node …` ends up exec-ing the
# relay stub). pkill is a harmless no-op stub (no relay procs in a sandbox).
ER_SHIMDIR="$ER_SANDBOX/shims"
mkdir -p "$ER_SHIMDIR"
cat > "$ER_SHIMDIR/nice" <<'SH'
#!/usr/bin/env bash
# drop "-n <N>" then exec the rest
while [ "${1:-}" = "-n" ]; do shift 2; done
exec "$@"
SH
cat > "$ER_SHIMDIR/ionice" <<'SH'
#!/usr/bin/env bash
# drop "-c <N>" then exec the rest
while [ "${1:-}" = "-c" ]; do shift 2; done
exec "$@"
SH
cat > "$ER_SHIMDIR/pkill" <<'SH'
#!/usr/bin/env bash
exit 0
SH
chmod +x "$ER_SHIMDIR/nice" "$ER_SHIMDIR/ionice" "$ER_SHIMDIR/pkill"

# A stub relay binary payload (the "release" bytes) and a stub MINER. The relay
# stub, when started, appends to relay-started.log; the miner stub drops a marker.
ER_RELAY_PAYLOAD="$ER_SANDBOX/relay-payload"
cat > "$ER_RELAY_PAYLOAD" <<'RELAYSTUB'
#!/usr/bin/env bash
# Relay stub: emulates the two subcommands the launcher invokes.
#   `wallet new --out <path>` → write a placeholder wallet file (exit 0)
#   `node …`                  → log that the relay node started (exit 0)
if [ "${1:-}" = "wallet" ] && [ "${2:-}" = "new" ]; then
  out=""; shift 2
  while [ $# -gt 0 ]; do [ "$1" = "--out" ] && { out="$2"; shift 2; continue; }; shift; done
  [ -n "$out" ] && printf 'stub-wallet\n' > "$out"
  exit 0
fi
if [ "${1:-}" = "node" ]; then
  echo "relay-node up args:[$*]" >> "$CSD_TEST_RELAY_RAN"
  exit 0
fi
exit 0
RELAYSTUB
ER_RELAY_SHA="$(sha256sum "$ER_RELAY_PAYLOAD" | awk '{print $1}')"
ER_BAD_SHA="0000000000000000000000000000000000000000000000000000000000000000"

# run_relay_case TAG MODE  → echoes a result block the caller greps.
#   MODE:
#     FRESH   no relay on disk; SHA256SUMS lists the correct relay digest
#     BADSHA  no relay on disk; SHA256SUMS lists an all-zeros (wrong) digest
#     DLFAIL  relay download() returns non-zero (404); SHA256SUMS fine
#     NORELAY CSD_NO_RELAY=1; download() is a TRIPWIRE that fails the test if hit
#     PRESENT relay ALREADY on disk + correct SHA; download() is a re-download
#             TRIPWIRE (any fetch of the relay asset fails the test)
# Each scenario runs in its OWN bash to avoid global-state bleed. After
# ensure_relay we run a MINER stub that drops "$work/miner-marker" — its presence
# proves control flow reached the miner launch (best-effort rule #1).
run_relay_case() {
  local tag="$1" mode="$2"
  local work="$ER_SANDBOX/case-$tag"
  mkdir -p "$work"

  # Stage the fake-release SHA256SUMS for this case (keyed by the relay basename).
  local sums="$work/SHA256SUMS"
  case "$mode" in
    FRESH|DLFAIL|NORELAY|PRESENT) printf '%s  csd-relay-node\n' "$ER_RELAY_SHA" > "$sums" ;;
    BADSHA)                       printf '%s  csd-relay-node\n' "$ER_BAD_SHA"   > "$sums" ;;
  esac

  # PRESENT: pre-place a byte-identical, already-executable relay on disk.
  if [ "$mode" = "PRESENT" ]; then
    cp "$ER_RELAY_PAYLOAD" "$work/csd-relay-node"
    chmod +x "$work/csd-relay-node"
  fi

  CSD_SOURCE_ONLY=1 \
  CASE_DIR="$work" CASE_MODE="$mode" \
  RELAY_PAYLOAD="$ER_RELAY_PAYLOAD" BIN_STUB="$ER_BIN_STUB" \
  CSD_TEST_RELAY_RAN="$work/relay-started.log" \
  PATH="$ER_SHIMDIR:$PATH" \
  bash -c '
    set -uo pipefail
    source "'"$LAUNCHER"'" >/dev/null 2>&1

    # Redirect ALL relay state into the sandbox.
    DATA_DIR="$CASE_DIR"
    CFG_DIR="$CASE_DIR"
    BIN="$BIN_STUB"
    RELAY_BIN_NAME="csd-relay-node"
    RELAY_BIN="$CASE_DIR/csd-relay-node"
    RELAY_DATADIR="$CASE_DIR/relay-data"
    RELAY_WALLET="$CASE_DIR/relay-wallet.json"
    RELAY_BLACKLIST="$CASE_DIR/relay-blacklist.txt"
    RELAY_LOG="$CASE_DIR/relay.log"
    RELAY_PUSH_PEERS="$CASE_DIR/relay-push-peers.txt"
    RELAY_WALLET_CMD=("$RELAY_BIN" wallet new --out "$RELAY_WALLET")
    RELAY_PID=0
    if [ "$CASE_MODE" = "NORELAY" ]; then export CSD_NO_RELAY=1; fi

    # download() override → serve the LOCAL fake release.
    #   .../csd-relay-node → copy payload (FRESH/BADSHA) | FAIL (DLFAIL) |
    #                        TRIPWIRE-FAIL (NORELAY/PRESENT: must never be fetched)
    #   .../SHA256SUMS     → copy the staged sums
    download() {
      local url="$1" out="$2"
      case "$url" in
        *"/csd-relay-node")
          case "$CASE_MODE" in
            DLFAIL)          return 1 ;;
            NORELAY|PRESENT) echo "TRIPWIRE: relay asset was fetched (CASE_MODE=$CASE_MODE)"; touch "$CASE_DIR/TRIPWIRE_HIT"; return 1 ;;
            *)               cp "$RELAY_PAYLOAD" "$out" ;;
          esac ;;
        *"/SHA256SUMS")
          cp "$CASE_DIR/SHA256SUMS" "$out" ;;
        *) return 1 ;;
      esac
    }

    # Sourcing inherits set -e; guard so a fail-closed return(0/1) does not abort.
    # Mirror the real call-site contract from start_miners():
    #   ensure_relay || true      (install: download/verify/place — best-effort)
    #   start_relay  || true       (launch the relay if a usable binary is present)
    rc=0
    ensure_relay || rc=$?
    start_relay  || true

    # BEST-EFFORT proof: the GPU-miner launch must be reached regardless. The real
    # start_miners() continues into the GPU loop after the relay block; we drop a
    # marker here to prove control flow was never aborted by a relay failure. This
    # marker is set BEFORE we wait on the (backgrounded) relay — proving the miner
    # launch never waits on the relay (HAZARD-2: relay start is `&` in start_relay).
    : > "$CASE_DIR/miner-marker"

    # The relay is launched in the BACKGROUND (&), so its stub may not have written
    # its start-log yet. Reap it (bounded) so RELAY_STARTED reflects reality. A
    # plain `wait` blocks until the (fast-exiting) relay stub finishes; cap with a
    # short poll as a belt so a hang here can never wedge the test run.
    if [ "$RELAY_PID" -gt 0 ]; then
      n=0
      while [ ! -s "$CASE_DIR/relay-started.log" ] && [ "$n" -lt 50 ]; do
        kill -0 "$RELAY_PID" 2>/dev/null || break
        sleep 0.1; n=$((n + 1))
      done
      wait "$RELAY_PID" 2>/dev/null || true
    fi

    echo "RC=$rc"
    [ -x "$RELAY_BIN" ] && echo "RELAY_PRESENT=1" || echo "RELAY_PRESENT=0"
    [ -e "$CASE_DIR/relay-started.log" ] && echo "RELAY_STARTED=1" || echo "RELAY_STARTED=0"
    [ -e "$CASE_DIR/miner-marker" ]      && echo "MINER_LAUNCHED=1" || echo "MINER_LAUNCHED=0"
    [ -e "$CASE_DIR/TRIPWIRE_HIT" ]      && echo "TRIPWIRE=1" || echo "TRIPWIRE=0"
  ' 2>&1
  # ^ merge stderr→stdout: ensure_relay/start_relay emit their failure messages on
  #   stderr (verify-FAILED, download-failed, etc.); the message assertions below
  #   grep the combined stream. The KEY=VAL status lines are on stdout regardless.
}

# Helper to pull a KEY=VAL line out of a case's output.
er_val() { printf '%s\n' "$1" | grep -oE "^$2=[0-9]+" | tail -1 | cut -d= -f2; }

# ── R1. Fresh rig (no relay) → download + verify + place + start ──────────────
R1_OUT="$(run_relay_case R1 FRESH || true)"
R1_PRESENT="$(er_val "$R1_OUT" RELAY_PRESENT)"
R1_STARTED="$(er_val "$R1_OUT" RELAY_STARTED)"
R1_MINER="$(er_val "$R1_OUT" MINER_LAUNCHED)"
if [ "$R1_PRESENT" = "1" ] && file_is_string_er "$ER_SANDBOX/case-R1/csd-relay-node" "$ER_RELAY_PAYLOAD"; then
  ok "R1 fresh rig: relay downloaded, verified, placed (exists + exec + byte-correct)"
else
  fail "R1 fresh rig: relay placed" "expected \$RELAY_BIN present & byte-identical to payload; out=[$R1_OUT]"
fi
if [ "$R1_STARTED" = "1" ]; then
  ok "R1 fresh rig: relay STARTED from the freshly-installed binary"
else
  fail "R1 fresh rig: relay started" "expected relay-started.log; out=[$R1_OUT]"
fi
if [ "$R1_MINER" = "1" ]; then
  ok "R1 fresh rig: GPU miner launch reached (best-effort)"
else
  fail "R1 fresh rig: miner launched" "miner marker absent; out=[$R1_OUT]"
fi

# ── R2. SHA mismatch (all-zeros) → relay ABSENT + verify-FAILED + MINER lives ─
# CORE SAFETY: never chmod+exec a SHA-mismatched relay; the miner must still run.
R2_OUT="$(run_relay_case R2 BADSHA || true)"
R2_PRESENT="$(er_val "$R2_OUT" RELAY_PRESENT)"
R2_MINER="$(er_val "$R2_OUT" MINER_LAUNCHED)"
if [ "$R2_PRESENT" = "0" ] && [ ! -e "$ER_SANDBOX/case-R2/csd-relay-node" ]; then
  ok "R2 SHA mismatch: relay binary ABSENT (fail-closed; never placed)"
else
  fail "R2 SHA mismatch: relay absent" "a relay binary was placed despite a SHA mismatch; out=[$R2_OUT]"
fi
if printf '%s\n' "$R2_OUT" | grep -qiE 'verify (FAILED|failed)|SHA-?256'; then
  ok "R2 SHA mismatch: verify-FAILED message emitted"
else
  fail "R2 SHA mismatch: message" "expected a SHA verify-FAILED message; out=[$R2_OUT]"
fi
if [ "$R2_MINER" = "1" ]; then
  ok "R2 SHA mismatch: GPU miner STILL launched (best-effort, relay failure non-fatal)"
else
  fail "R2 SHA mismatch: miner launched" "miner marker absent — a relay verify failure blocked mining; out=[$R2_OUT]"
fi

# ── R3. Download 404/fail → relay ABSENT + calm "unavailable" notice + MINER lives ─
# Messages were softened (miner-facing): the download-unavailable path now reads
# "relay helper unavailable right now (offline?) … will retry next run" instead of
# "relay not started / download failed". This regex matches the NEW calm wording
# while staying DISTINCT from R2 (SHA-256 verify-fail) and R4 (CSD_NO_RELAY opt-out).
R3_OUT="$(run_relay_case R3 DLFAIL || true)"
R3_PRESENT="$(er_val "$R3_OUT" RELAY_PRESENT)"
R3_MINER="$(er_val "$R3_OUT" MINER_LAUNCHED)"
if [ "$R3_PRESENT" = "0" ] && [ ! -e "$ER_SANDBOX/case-R3/csd-relay-node" ]; then
  ok "R3 download fail: relay binary ABSENT"
else
  fail "R3 download fail: relay absent" "a relay binary was placed despite a failed download; out=[$R3_OUT]"
fi
if printf '%s\n' "$R3_OUT" | grep -qiE 'relay helper unavailable|unavailable right now|offline\?|will retry next run'; then
  ok "R3 download fail: calm 'relay helper unavailable / will retry' message emitted"
else
  fail "R3 download fail: message" "expected a calm 'relay helper unavailable (offline?) … will retry next run' message; out=[$R3_OUT]"
fi
if [ "$R3_MINER" = "1" ]; then
  ok "R3 download fail: GPU miner STILL launched (best-effort)"
else
  fail "R3 download fail: miner launched" "miner marker absent; out=[$R3_OUT]"
fi

# ── R4. CSD_NO_RELAY=1 → no download, no start, clean opt-out, MINER lives ────
# download() is a TRIPWIRE for the relay asset: if the URL is fetched, TRIPWIRE=1.
R4_OUT="$(run_relay_case R4 NORELAY || true)"
R4_PRESENT="$(er_val "$R4_OUT" RELAY_PRESENT)"
R4_STARTED="$(er_val "$R4_OUT" RELAY_STARTED)"
R4_MINER="$(er_val "$R4_OUT" MINER_LAUNCHED)"
R4_TRIP="$(er_val "$R4_OUT" TRIPWIRE)"
if [ "$R4_TRIP" = "0" ] && [ ! -e "$ER_SANDBOX/case-R4/TRIPWIRE_HIT" ]; then
  ok "R4 CSD_NO_RELAY=1: relay asset was NOT fetched (download tripwire untouched)"
else
  fail "R4 CSD_NO_RELAY=1: no fetch" "the relay URL was fetched despite opt-out; out=[$R4_OUT]"
fi
if [ "$R4_PRESENT" = "0" ] && [ "$R4_STARTED" = "0" ]; then
  ok "R4 CSD_NO_RELAY=1: relay neither placed nor started (opt-out honoured)"
else
  fail "R4 CSD_NO_RELAY=1: skipped" "relay present/started despite opt-out; out=[$R4_OUT]"
fi
if printf '%s\n' "$R4_OUT" | grep -qiE 'CSD_NO_RELAY|opt-?out|relay.*(skip|disabled)|skip.*relay'; then
  ok "R4 CSD_NO_RELAY=1: clean opt-out message emitted"
else
  fail "R4 CSD_NO_RELAY=1: message" "expected an opt-out/skip message; out=[$R4_OUT]"
fi
if [ "$R4_MINER" = "1" ]; then
  ok "R4 CSD_NO_RELAY=1: GPU miner launched"
else
  fail "R4 CSD_NO_RELAY=1: miner launched" "miner marker absent; out=[$R4_OUT]"
fi

# ── R5. Relay already present + correct SHA → NO re-download, byte-unchanged ──
# download() is a re-download TRIPWIRE: any fetch of the relay asset fails the test.
R5_PRE_SHA="$(sha256sum "$ER_RELAY_PAYLOAD" | awk '{print $1}')"
R5_OUT="$(run_relay_case R5 PRESENT || true)"
R5_PRESENT="$(er_val "$R5_OUT" RELAY_PRESENT)"
R5_STARTED="$(er_val "$R5_OUT" RELAY_STARTED)"
R5_MINER="$(er_val "$R5_OUT" MINER_LAUNCHED)"
R5_TRIP="$(er_val "$R5_OUT" TRIPWIRE)"
R5_POST_SHA="$(sha256sum "$ER_SANDBOX/case-R5/csd-relay-node" 2>/dev/null | awk '{print $1}')"
if [ "$R5_TRIP" = "0" ] && [ ! -e "$ER_SANDBOX/case-R5/TRIPWIRE_HIT" ]; then
  ok "R5 relay present: NO re-download (tripwire untouched — idempotent)"
else
  fail "R5 relay present: no re-download" "the relay asset was re-fetched though a good binary exists; out=[$R5_OUT]"
fi
if [ -n "$R5_POST_SHA" ] && [ "$R5_POST_SHA" = "$R5_PRE_SHA" ]; then
  ok "R5 relay present: on-disk relay byte-UNCHANGED"
else
  fail "R5 relay present: unchanged" "on-disk relay changed (pre=$R5_PRE_SHA post=$R5_POST_SHA); out=[$R5_OUT]"
fi
if [ "$R5_STARTED" = "1" ]; then
  ok "R5 relay present: relay STARTED from the existing binary"
else
  fail "R5 relay present: started" "expected relay-started.log from existing binary; out=[$R5_OUT]"
fi
if [ "$R5_MINER" = "1" ]; then
  ok "R5 relay present: GPU miner launched"
else
  fail "R5 relay present: miner launched" "miner marker absent; out=[$R5_OUT]"
fi

rm -rf "$ER_SANDBOX"

# ── 22. STATIC: ensure_relay call site is best-effort (|| true / if-guard) ────
# Regression lock on the set -e stall path: a relay-install failure must never
# abort start_miners. The call site must be `ensure_relay || true` (or guarded by
# an if/||). A bare `ensure_relay` under `set -e` would kill the launcher.
echo
echo "-- static: ensure_relay call site is best-effort + watchdog decoupling --"
if grep -nE 'ensure_relay[[:space:]]*\|\|[[:space:]]*true' "$REPO_ROOT/mine-auto.sh" >/dev/null 2>&1; then
  ok "S1 ensure_relay invoked best-effort (\`ensure_relay || true\`) — no set -e stall"
elif grep -nE '(if[[:space:]]+ensure_relay|ensure_relay[[:space:]]*\|\|)' "$REPO_ROOT/mine-auto.sh" >/dev/null 2>&1; then
  ok "S1 ensure_relay invoked under an if/|| guard — no set -e stall"
else
  fail "S1 ensure_relay best-effort" "the ensure_relay call site must be \`|| true\` or if-guarded (set -e safety)"
fi

# ── 23. STATIC: watchdog `while true … miners_running` loop has ZERO RELAY_PID ─
# Locks the decoupling: the liveness/restart loop must not key off the relay PID,
# so a relay crash never restarts (or blocks) the GPU miners. Extract the loop
# body (from the final `while true; do` to EOF) and assert no RELAY_PID token.
echo
WHILE_LINE=$(grep -nE '^while true; do' "$REPO_ROOT/mine-auto.sh" | tail -1 | cut -d: -f1)
if [ -n "$WHILE_LINE" ]; then
  LOOP_BODY="$(tail -n +"$WHILE_LINE" "$REPO_ROOT/mine-auto.sh")"
  if printf '%s\n' "$LOOP_BODY" | grep -qE 'RELAY_PID'; then
    fail "S2 watchdog decoupling" "the 'while true … miners_running' loop (from line $WHILE_LINE) references RELAY_PID — relay must not drive miner liveness"
  else
    ok "S2 watchdog loop (from line $WHILE_LINE) contains ZERO RELAY_PID references (relay/miner decoupled)"
  fi
else
  fail "S2 watchdog decoupling" "could not locate the 'while true; do' watchdog loop"
fi

# ── 24. ensure_relay is sourceable (defined BEFORE the CSD_SOURCE_ONLY cutoff) ─
echo
if CSD_SOURCE_ONLY=1 bash -c 'source "'"$REPO_ROOT/mine-auto.sh"'" >/dev/null 2>&1; declare -F ensure_relay >/dev/null'; then
  ok "S3 ensure_relay is defined + sourceable under CSD_SOURCE_ONLY=1"
else
  fail "S3 ensure_relay sourceable" "ensure_relay not defined after sourcing (must sit before the CSD_SOURCE_ONLY cutoff)"
fi

# ── 25. download() refuses to write THROUGH a pre-planted symlink ─────────────
# The shared download() stages to a PREDICTABLE path ($out.tmp). A symlink
# planted there must NOT redirect the fetch to an arbitrary target (curl -o /
# wget -O both FOLLOW a symlink at the output path and write through it). We stub
# curl to write the payload to its -o target, plant a symlink at $out.tmp -> a
# "secret", call the REAL download(), and assert the secret is UNTOUCHED and the
# real destination is a regular file. (Does not touch the verify-then-swap flow.)
echo
DL_SB="$(mktemp -d)"
printf 'TOP-SECRET-ORIGINAL\n' > "$DL_SB/secret"
ln -s "$DL_SB/secret" "$DL_SB/asset.tmp" 2>/dev/null
if [ ! -L "$DL_SB/asset.tmp" ]; then
  # This platform's `ln -s` did not create a real symlink (Git-Bash/MSYS without
  # winsymlinks copies instead), so the symlink-follow vuln cannot be exercised
  # here. Skip rather than false-pass; the path runs for real on Linux (the fleet).
  echo "  [SKIP] S4/S5 download() symlink hardening — real symlinks unavailable on this platform; verified on Linux"
  rm -rf "$DL_SB"
else
  DL_SHIM="$DL_SB/shim"; mkdir -p "$DL_SHIM"
  cat > "$DL_SHIM/curl" <<'CURLEOF'
#!/usr/bin/env bash
out=""; while [ $# -gt 0 ]; do case "$1" in -o) out="$2"; shift 2;; *) shift;; esac; done
printf 'DOWNLOADED-BYTES\n' > "$out"
CURLEOF
  chmod +x "$DL_SHIM/curl"
  PATH="$DL_SHIM:$PATH" CSD_SOURCE_ONLY=1 bash -c '
    source "'"$REPO_ROOT/mine-auto.sh"'" >/dev/null 2>&1
    download "http://example.invalid/asset" "'"$DL_SB/asset"'" >/dev/null 2>&1
  ' >/dev/null 2>&1
  DL_SECRET_AFTER="$(cat "$DL_SB/secret" 2>/dev/null)"
  if [ "$DL_SECRET_AFTER" = "TOP-SECRET-ORIGINAL" ]; then
    ok "S4 download() does NOT write through a planted \$out.tmp symlink (secret untouched)"
  else
    fail "S4 download symlink write-through" "the planted \$out.tmp symlink redirected the fetch; secret now: '$DL_SECRET_AFTER'"
  fi
  if grep -q DOWNLOADED-BYTES "$DL_SB/asset" 2>/dev/null && [ ! -L "$DL_SB/asset" ]; then
    ok "S5 download() still delivers bytes to the real destination (regular file)"
  else
    fail "S5 download delivers" "\$out did not receive the bytes as a regular file"
  fi
  rm -rf "$DL_SB"
fi

# ── Summary ──────────────────────────────────────────────────────────────────
echo
echo "========================================"
echo "  Passed: $PASS  Failed: $FAIL"
echo "========================================"
echo

if [ "$FAIL" -gt 0 ]; then
  exit 1
fi
exit 0
