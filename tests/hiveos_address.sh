#!/usr/bin/env bash
# Address-rendering test for hiveos/h-config.sh — the now-PRIMARY onboarding path.
#
# HiveOS does NOT populate $CUSTOM_TEMPLATE (the "Wallet and worker template" box)
# for a coinless custom miner like CSD (confirmed on real rigs), so the documented
# address path is `--address <addr>` in the "Extra config arguments" box, which
# h-config.sh recovers from $CUSTOM_USER_CONFIG. This locks both paths + the
# normalisation (0x/0X strip, .worker strip, unexpanded-macro blanking, hex guard).
#
# Runs the REAL h-config.sh in a temp dir with a stub manifest pointing CONF at a
# writable file, then asserts the address line it wrote.
set -u
ROOT="$(cd "$(dirname "$0")/.." && pwd)"
HCONF="$ROOT/hiveos/h-config.sh"
ADDR40="da408d177dba334ad18c479d84eba8a0a723b7a8"   # 40-hex sample
pass=0; fail=0
ok(){ echo "  PASS: $1"; pass=$((pass + 1)); }
no(){ echo "  FAIL: $1 (got '$2', want '$3')"; fail=$((fail + 1)); }

# render <CUSTOM_TEMPLATE> <CUSTOM_USER_CONFIG> -> echoes the address value written
render() {
  local t="$1" u="$2" d
  d="$(mktemp -d)"
  cp "$HCONF" "$d/h-config.sh"
  # Minimal stub manifest: point CONF at a writable file in the temp dir.
  printf 'CUSTOM_CONFIG_FILENAME=%s/config.toml\n' "$d" > "$d/h-manifest.conf"
  ( cd "$d" && CUSTOM_TEMPLATE="$t" CUSTOM_USER_CONFIG="$u" bash ./h-config.sh >/dev/null 2>&1 )
  sed -n 's/^address = "\(.*\)"$/\1/p' "$d/config.toml" 2>/dev/null
  rm -rf "$d"
}
check() { # desc  template  userconfig  expected
  local got; got="$(render "$2" "$3")"
  [ "$got" = "$4" ] && ok "$1" || no "$1" "$got" "$4"
}

echo "== wallet-template path =="
check "literal 40-hex template -> address"                "$ADDR40"          ""                                "$ADDR40"
check "0x prefix on template stripped"                    "0x$ADDR40"        ""                                "$ADDR40"
check ".worker suffix on template stripped"               "$ADDR40.rig1"     ""                                "$ADDR40"
check "unexpanded %WAL% macro -> blank (no address)"      "%WAL%"            ""                                ""

echo "== extra-args --address fallback (the documented CSD path) =="
check "--address in extras -> address"                    ""                 "--backend cuda --address $ADDR40" "$ADDR40"
check "--address with .worker suffix -> stripped"         ""                 "--address $ADDR40.rig1"          "$ADDR40"
check "--address with 0X (uppercase) prefix -> stripped"  ""                 "--address 0X$ADDR40"             "$ADDR40"
check "extras with NO --address -> blank"                 ""                 "--backend cuda"                  ""
check "template present wins over extras --address"       "$ADDR40"          "--address ffffffffffffffffffffffffffffffffffffffff" "$ADDR40"

# render the extra-flags file h-config writes from the extras box
render_flags() {
  local u="$1" d
  d="$(mktemp -d)"
  cp "$HCONF" "$d/h-config.sh"
  printf 'CUSTOM_CONFIG_FILENAME=%s/config.toml\n' "$d" > "$d/h-manifest.conf"
  ( cd "$d" && CUSTOM_TEMPLATE="$ADDR40" CUSTOM_USER_CONFIG="$u" bash ./h-config.sh >/dev/null 2>&1 )
  tr -s ' ' < "$d/extra-flags" 2>/dev/null | sed 's/^ //; s/ $//'
  rm -rf "$d"
}
checkflags() { # desc  userconfig  expected-flags
  local got; got="$(render_flags "$2")"
  [ "$got" = "$3" ] && ok "$1" || no "$1" "$got" "$3"
}

echo "== forced stats flags cannot be overridden from the extras box =="
checkflags "legit flags pass through untouched"          "--backend cuda --gpu-id 0,1"                 "--backend cuda --gpu-id 0,1"
checkflags "--stats-port <space> stripped"               "--stats-port 4000 --backend cuda"            "--backend cuda"
checkflags "--stats-port=<val> stripped"                 "--backend cuda --stats-port=5000"            "--backend cuda"
checkflags "--stats-bind stripped (loopback-only kept)"  "--backend opencl --stats-bind 0.0.0.0 --gpu-id 0,1" "--backend opencl --gpu-id 0,1"

echo
echo "  Passed: $pass  Failed: $fail"
[ "$fail" -eq 0 ] && { echo "ALL HIVEOS ADDRESS ASSERTIONS PASSED"; exit 0; } || { echo "ADDRESS TEST FAILED"; exit 1; }
