#!/usr/bin/env bash
# Packaging test for the HiveOS tarball (the coverage gap the variant fix left).
#
# Locks three release.yml invariants that a future edit could silently break:
#   1. The bundled HiveOS binary is the preserved CPU SEED (csd-gpu-miner-hiveos-seed),
#      NOT target/release/csd-gpu-miner — which holds the LAST build (AMD/opencl) and
#      shipped an opencl binary to every rig (the 15-day NVIDIA bug).
#   2. A .installed-variant=cpu marker ships so h-run.sh knows the seed is CPU and
#      must fetch the real GPU variant (even when the version already matches).
#   3. The relay is staged into the SAME dir the tar packs (hiveos-pkg/csdpool/),
#      not hiveos-pkg/csd-pool-miner/ — else it never lands in the tarball.
#
# Static greps over release.yml + a functional replication of the Package logic.
set -u
ROOT="$(cd "$(dirname "$0")/.." && pwd)"
RY="$ROOT/.github/workflows/release.yml"
pass=0; fail=0
ok(){ echo "  PASS: $1"; pass=$((pass + 1)); }
no(){ echo "  FAIL: $1"; fail=$((fail + 1)); }

echo "== static guards on release.yml =="
grep -q 'cp target/x86_64-unknown-linux-gnu/release/csd-gpu-miner csd-gpu-miner-hiveos-seed' "$RY" \
  && ok "CPU build is preserved to the never-overwritten seed path" \
  || no "CPU-seed preservation line missing"
grep -q 'cp csd-gpu-miner-hiveos-seed' "$RY" \
  && ok "HiveOS Package bundles the CPU seed" \
  || no "seed is not copied into the HiveOS stage"
grep -q 'installed-variant' "$RY" \
  && ok ".installed-variant marker is written into the package" \
  || no "variant marker not written"
grep -q "printf 'cpu" "$RY" \
  && ok "marker value is cpu" \
  || no "marker value is not cpu"
grep -q 'mkdir -p dist hiveos-pkg/csdpool' "$RY" \
  && ok "relay mkdir targets hiveos-pkg/csdpool" \
  || no "relay mkdir targets the wrong dir"
grep -q 'cp dist/csd-relay-node hiveos-pkg/csdpool/csd-relay-node' "$RY" \
  && ok "relay staged into hiveos-pkg/csdpool (matches the tar STAGE)" \
  || no "relay staged to the wrong dir"
grep -q 'hiveos-pkg/csd-pool-miner/csd-relay' "$RY" \
  && no "relay STILL staged to the stale hiveos-pkg/csd-pool-miner/ dir" \
  || ok "no stale hiveos-pkg/csd-pool-miner/ relay path remains"
grep -q 'PKG="csdpool"' "$RY" \
  && ok "PKG=csdpool (hyphen-free name HiveOS derives cleanly)" \
  || no "PKG is not csdpool"

# B4: CUSTOM_VERSION must be stamped into the STAGED manifest from the release
# tag, so the HiveOS UI stops showing a hand-edited value that goes stale every
# release (shipped 0.2.1 while the fleet ran 0.2.3).
grep -qF 's|^CUSTOM_VERSION=.*|CUSTOM_VERSION=${VER}|' "$RY" \
  && ok "release.yml stamps CUSTOM_VERSION into the staged manifest from \$VER" \
  || no "CUSTOM_VERSION is NOT stamped at package time (it will go stale again)"
grep -qF 'grep -qx "CUSTOM_VERSION=${VER}"' "$RY" \
  && ok "the stamp is verified fail-closed (a silent no-op sed cannot ship)" \
  || no "no fail-closed verify after the CUSTOM_VERSION stamp"

# The checked-in value is what a hand-built tarball and any repo reader sees, so
# lock it to Cargo.toml. This is the assertion that makes the drift impossible to
# reintroduce silently.
MANIFEST_VER="$(sed -n 's/^CUSTOM_VERSION=//p' "$ROOT/hiveos/h-manifest.conf" | tr -d '\r')"
CARGO_VER="$(grep -m1 '^version' "$ROOT/Cargo.toml" | sed -E 's/.*"([^"]+)".*/\1/')"
[ -n "$MANIFEST_VER" ] && [ "$MANIFEST_VER" = "$CARGO_VER" ] \
  && ok "checked-in CUSTOM_VERSION ($MANIFEST_VER) == Cargo.toml version" \
  || no "checked-in CUSTOM_VERSION ('$MANIFEST_VER') != Cargo.toml version ('$CARGO_VER')"

echo "== functional: replicate the Package seed+marker+tar logic =="
T="$(mktemp -d)"; trap 'rm -rf "$T"' EXIT
mkdir -p "$T/dist" "$T/target/release" "$T/hiveos"
printf 'CPUBIN'     > "$T/csd-gpu-miner-hiveos-seed"     # the preserved CPU seed
printf 'AMDBIN'     > "$T/target/release/csd-gpu-miner"  # last build = AMD/opencl (the trap)
printf 'NVBIN'      > "$T/dist/csd-pool-miner-linux-nvidia"
printf 'relaybytes' > "$T/dist/csd-relay-node"
printf 'dash'       > "$T/csd-dashboard.sh"
for f in h-config.sh h-run.sh h-stats.sh h-stop.sh; do printf '%s\n' "$f" > "$T/hiveos/$f"; done
# A realistic manifest carrying a deliberately STALE version, so the replication
# below proves the package step rewrites it.
printf 'CUSTOM_NAME=csdpool\nCUSTOM_VERSION=0.0.1\nCUSTOM_BUILD=1\n' > "$T/hiveos/h-manifest.conf"

(
  cd "$T" || exit 1
  PKG=csdpool; STAGE="hiveos-pkg/$PKG"; mkdir -p "$STAGE"
  cp dist/csd-relay-node "$STAGE"/csd-relay-node                       # relay (csdpool path)
  cp hiveos/h-manifest.conf hiveos/h-config.sh hiveos/h-run.sh hiveos/h-stats.sh hiveos/h-stop.sh "$STAGE"/
  cp csd-dashboard.sh "$STAGE"/csd-dashboard.sh
  VER=9.9.9
  sed -i -E "s|^CUSTOM_VERSION=.*|CUSTOM_VERSION=${VER}|" "$STAGE/h-manifest.conf"
  grep -qx "CUSTOM_VERSION=${VER}" "$STAGE/h-manifest.conf" || exit 1
  if [ -f csd-gpu-miner-hiveos-seed ]; then
    cp csd-gpu-miner-hiveos-seed "$STAGE"/csd-gpu-miner                # seed, NOT target/release
  else
    cp target/release/csd-gpu-miner "$STAGE"/csd-gpu-miner
  fi
  printf 'cpu\n' > "$STAGE"/.installed-variant
  tar -czf "$PKG.tar.gz" -C hiveos-pkg "$PKG"
)

got="$(cat "$T/hiveos-pkg/csdpool/csd-gpu-miner" 2>/dev/null)"
[ "$got" = "CPUBIN" ] \
  && ok "bundled binary is the CPU SEED (not the AMD/opencl last build)" \
  || no "bundled binary is '$got' (expected CPUBIN)"
[ "$(cat "$T/hiveos-pkg/csdpool/.installed-variant" 2>/dev/null)" = "cpu" ] \
  && ok ".installed-variant == cpu in the staged dir" \
  || no "marker content wrong"
[ "$(sed -n 's/^CUSTOM_VERSION=//p' "$T/hiveos-pkg/csdpool/h-manifest.conf" 2>/dev/null)" = "9.9.9" ] \
  && ok "staged h-manifest.conf CUSTOM_VERSION stamped to the release version" \
  || no "staged CUSTOM_VERSION not stamped (still stale)"
[ "$(sed -n 's/^CUSTOM_VERSION=//p' "$T/hiveos/h-manifest.conf" 2>/dev/null)" = "0.0.1" ] \
  && ok "the stamp rewrites the COPY, leaving the repo source untouched" \
  || no "the stamp mutated the repo-source manifest (must only touch \$STAGE)"

TL="$(tar -tzf "$T/csdpool.tar.gz" 2>/dev/null)"
printf '%s\n' "$TL" | grep -q '^csdpool/.installed-variant$' && ok "tar contains csdpool/.installed-variant" || no "tar missing the marker"
printf '%s\n' "$TL" | grep -q '^csdpool/csd-gpu-miner$'      && ok "tar contains csdpool/csd-gpu-miner"       || no "tar missing the binary"
printf '%s\n' "$TL" | grep -q '^csdpool/csd-relay-node$'     && ok "tar contains csdpool/csd-relay-node (relay staging fix)" || no "tar MISSING the relay"
printf '%s\n' "$TL" | grep -q '^csdpool/h-stop.sh$'          && ok "tar contains csdpool/h-stop.sh (HiveOS stop hook)" || no "tar MISSING the stop hook"

echo
echo "  Passed: $pass  Failed: $fail"
[ "$fail" -eq 0 ] && { echo "ALL HIVEOS SEED/VARIANT PACKAGING ASSERTIONS PASSED"; exit 0; } || { echo "PACKAGING TEST FAILED"; exit 1; }
