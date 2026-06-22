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
