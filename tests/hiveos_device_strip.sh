#!/usr/bin/env bash
# tests/hiveos_device_strip.sh — a stray --device in the flight-sheet extras box
# must NEVER reach the miner argv.
#
# THE BUG: the binary is one-process-one-GPU (`--device N`, src/main.rs:226-227).
# h-run.sh fans a multi-GPU rig out itself: hive_launch_extra_gpus() emits
#   "$CUSTOM_BIN" --config "$CONF" --device "$_dev" --stats-port … $EXTRA_FLAGS
# (h-run.sh:275-276) for each extra card, and then execs device 0 the same way
# (h-run.sh:772-774). In BOTH launch sites the launcher's own --device is emitted
# BEFORE $EXTRA_FLAGS, and clap is LAST-VALUE-WINS — so a `--device 0` typed into
# the "Extra config arguments" box overrides every one of them and collapses the
# whole rig onto a single card. Six processes fight over GPU 0; the rig LOOKS
# like it is mining 6 GPUs and mines ~1.
#
# THE FIX (hiveos/h-config.sh): strip --device out of the extras box before it is
# written to <config dir>/extra-flags — exactly the way --stats-port/--stats-bind
# are already stripped (h-config.sh:152-156) — and WARN so the operator can see
# why their flag was ignored.
#
# NOT stripped: --gpu-id. That one is legitimate — it is the launcher's include-
# list (src/main.rs:305-306; the miner only parses+logs it) and h-run.sh's
# hive_gpu_id_list() reads it out of EXTRA_FLAGS to decide WHICH cards to fan out
# to. Stripping it would break operator card selection.
#
# ON "--device inside a quoted string": there is no such thing here. h-run.sh
# word-splits $EXTRA_FLAGS unquoted (the deliberate SC2086 suppression at
# h-run.sh:271/771), so quote characters in the extras box are literal bytes, not
# shell grouping. `--worker "rig --device 0"` therefore reaches clap as the four
# tokens `--worker` `"rig` `--device` `0"` — i.e. a REAL --device flag. Stripping
# it is correct; leaving it would reintroduce the exact bug.
#
# Runs the REAL hiveos/h-config.sh in a temp dir with a stub manifest, then
# asserts the extra-flags file it wrote (and the warning it printed).
#
# Run:  bash tests/hiveos_device_strip.sh
# Exit: 0 = all pass

set -u
ROOT="$(cd "$(dirname "${BASH_SOURCE[0]:-$0}")/.." && pwd)"
HCONF="$ROOT/hiveos/h-config.sh"
ADDR40="da408d177dba334ad18c479d84eba8a0a723b7a8"   # 40-hex sample
pass=0; fail=0
ok(){ echo "  PASS: $1"; pass=$((pass + 1)); }
no(){ echo "  FAIL: $1 (got '$2', want '$3')"; fail=$((fail + 1)); }

# render_flags <CUSTOM_USER_CONFIG> -> the extra-flags line h-config.sh wrote,
# whitespace-squeezed and trimmed (same normalisation tests/hiveos_address.sh uses).
# WORKER_NAME is blanked so a host env leak cannot alter the render.
render_flags() {
  local u="$1" d
  d="$(mktemp -d)"
  cp "$HCONF" "$d/h-config.sh"
  printf 'CUSTOM_CONFIG_FILENAME=%s/config.toml\n' "$d" > "$d/h-manifest.conf"
  ( cd "$d" && CUSTOM_TEMPLATE="$ADDR40" CUSTOM_USER_CONFIG="$u" WORKER_NAME="" \
      bash ./h-config.sh >/dev/null 2>&1 )
  tr -s ' ' < "$d/extra-flags" 2>/dev/null | sed 's/^ //; s/ $//'
  rm -rf "$d"
}

# render_out <CUSTOM_USER_CONFIG> -> h-config.sh's stdout+stderr (for warnings)
render_out() {
  local u="$1" d
  d="$(mktemp -d)"
  cp "$HCONF" "$d/h-config.sh"
  printf 'CUSTOM_CONFIG_FILENAME=%s/config.toml\n' "$d" > "$d/h-manifest.conf"
  ( cd "$d" && CUSTOM_TEMPLATE="$ADDR40" CUSTOM_USER_CONFIG="$u" WORKER_NAME="" \
      bash ./h-config.sh 2>&1 )
  rm -rf "$d"
}

checkflags() { # desc  userconfig  expected-flags
  local got; got="$(render_flags "$2")"
  [ "$got" = "$3" ] && ok "$1" || no "$1" "$got" "$3"
}

echo "== --device is stripped out of the flight-sheet extras (the multi-GPU collapse) =="
checkflags "--device <space> N stripped" \
  "--device 0 --backend cuda"                       "--backend cuda"
checkflags "--device=N stripped" \
  "--backend cuda --device=0"                       "--backend cuda"
checkflags "--device stripped mid-string (surrounding flags intact)" \
  "--backend cuda --device 3 --cpu-threads 0"       "--backend cuda --cpu-threads 0"
checkflags "--device AND --stats-port both stripped" \
  "--device 0 --stats-port 4000 --backend cuda"     "--backend cuda"
checkflags "--device=N AND --stats-bind=IP both stripped" \
  "--backend opencl --device=2 --stats-bind=0.0.0.0" "--backend opencl"
checkflags "multiple --device occurrences all stripped" \
  "--device 0 --backend cuda --device=1"            "--backend cuda"
# The value token here is `0"` (the closing quote is part of the WORD, because
# h-run.sh word-splits unquoted), so the strip removes `--device` together with
# `0"` and leaves `--worker "rig`. That is the right outcome: clap then sees
# --worker with the value `"rig` (sanitised to `rig` by the miner's own
# [A-Za-z0-9_-] rule) and the rig MINES. Left unstripped, clap would instead see
# a real `--device 0"` — which is both the collapse bug and an invalid usize, so
# the miner would refuse to start at all.
checkflags "--device inside what the operator quoted is STILL stripped (h-run word-splits)" \
  "--backend cuda --worker \"rig --device 0\""      "--backend cuda --worker \"rig"

echo "== --gpu-id MUST survive (launcher include-list, not a device selector) =="
checkflags "--gpu-id alone survives" \
  "--backend cuda --gpu-id 0,2"                     "--backend cuda --gpu-id 0,2"
checkflags "--gpu-id survives while --device next to it is stripped" \
  "--backend cuda --gpu-id 0,2 --device 0"          "--backend cuda --gpu-id 0,2"
checkflags "--gpu-id=<val> form survives too" \
  "--gpu-id=0,1,2 --device=1"                       "--gpu-id=0,1,2"

echo "== everything else passes through untouched =="
checkflags "legit flags untouched (no --device present)" \
  "--backend cuda --cpu-threads 0 --gpu-id 0,1"     "--backend cuda --cpu-threads 0 --gpu-id 0,1"
checkflags "no extras at all -> empty extra-flags, no crash" \
  ""                                                ""

echo "== the operator is TOLD their --device was dropped =="
_o="$(render_out "--device 0 --backend cuda")"
case "$_o" in
  *"WARNING"*"--device"*) ok "warns when a --device is stripped" ;;
  *) no "no warning emitted for a stripped --device" "$_o" "a h-config WARNING mentioning --device" ;;
esac
_o2="$(render_out "--backend cuda --gpu-id 0,1")"
case "$_o2" in
  *"WARNING"*"--device"*) no "spurious --device warning with no --device present" "$_o2" "no --device warning" ;;
  *) ok "no --device warning when the extras box has none" ;;
esac
# --gpu-id must NOT be mistaken for a device selector by the warning path either.
_o3="$(render_out "--gpu-id 0,2 --backend cuda")"
case "$_o3" in
  *"WARNING"*"--device"*) no "--gpu-id wrongly warned about as a --device" "$_o3" "no --device warning" ;;
  *) ok "--gpu-id does not trip the --device warning" ;;
esac

echo "== NEWLINES in the extras box do not smuggle a --device past the strip =="
# THE SECOND BUG (MEDIUM-4). The HiveOS "Extra config arguments" box is a
# TEXTAREA and preserves newlines — h-config.sh's own defensive `tr '\n\t' '  '`
# on the DETECTION path (h-config.sh:165/169) is the standing evidence that
# multi-line input is expected here. But the WRITE path used a bare `sed -E`,
# and sed is LINE-oriented: [[:space:]] matches a space or a tab WITHIN a line
# and can never match the newline that ended it. So an operator who typed
#     --backend cuda --device
#     0
# got the flag DETECTED (warned about) but NOT stripped — `--device` and `0`
# both reached clap as a real device selector, i.e. exactly the collapse this
# file exists to prevent, with a warning printed claiming it had been removed.
#
# The mirror-image case is nastier still: `--device ` with a TRAILING space and
# the value on the next line stripped the flag but ORPHANED the `0`, which then
# reached clap as a stray positional argument ("unexpected argument '0'") and
# stopped the miner from starting at all.
#
# These assert on the surviving TOKENS with all whitespace squeezed, because
# h-run.sh word-splits $EXTRA_FLAGS on IFS — space, tab AND newline are the same
# separator to it, so a token is smuggled through regardless of which one
# preceded it.
render_flags_ws() {
  local u="$1" d
  d="$(mktemp -d)"
  cp "$HCONF" "$d/h-config.sh"
  printf 'CUSTOM_CONFIG_FILENAME=%s/config.toml\n' "$d" > "$d/h-manifest.conf"
  ( cd "$d" && CUSTOM_TEMPLATE="$ADDR40" CUSTOM_USER_CONFIG="$u" WORKER_NAME="" \
      bash ./h-config.sh >/dev/null 2>&1 )
  tr '\n\t' '  ' < "$d/extra-flags" 2>/dev/null | tr -s ' ' | sed 's/^ //; s/ $//'
  rm -rf "$d"
}
checkflags_ws() { # desc  userconfig  expected-surviving-tokens
  local got; got="$(render_flags_ws "$2")"
  [ "$got" = "$3" ] && ok "$1" || no "$1" "$got" "$3"
}

checkflags_ws "--device at EOL, value on the next line, is stripped" \
  "$(printf -- '--backend cuda --device\n0')"               "--backend cuda"
checkflags_ws "--device<space> at EOL leaves NO orphan value token" \
  "$(printf -- '--backend cuda --device \n0')"              "--backend cuda"
checkflags_ws "--device=N on its own line is stripped" \
  "$(printf -- '--device=0\n--backend cuda')"               "--backend cuda"
checkflags_ws "--device N on its own line among other flags is stripped" \
  "$(printf -- '--backend cuda\n--device 0\n--cpu-threads 0')" "--backend cuda --cpu-threads 0"
checkflags_ws "one flag per line (the way the HiveOS textarea is actually used)" \
  "$(printf -- '--backend cuda\n--gpu-id 0,2\n--device\n1\n')" "--backend cuda --gpu-id 0,2"
checkflags_ws "multiple newline-split --device occurrences all stripped" \
  "$(printf -- '--device\n0\n--backend cuda\n--device=1')"  "--backend cuda"
checkflags_ws "trailing newline alone does not disturb legit flags" \
  "$(printf -- '--backend cuda --cpu-threads 0\n')"         "--backend cuda --cpu-threads 0"

# The written file must be a SINGLE line. This is the invariant that keeps the
# line-oriented sed honest: if extra-flags can never contain an interior
# newline, no token can hide from the strip on a line of its own.
_nl="$(mktemp -d)"; cp "$HCONF" "$_nl/h-config.sh"
printf 'CUSTOM_CONFIG_FILENAME=%s/config.toml\n' "$_nl" > "$_nl/h-manifest.conf"
( cd "$_nl" && CUSTOM_TEMPLATE="$ADDR40" \
    CUSTOM_USER_CONFIG="$(printf -- '--backend cuda\n--gpu-id 0,2\n--device\n1')" \
    WORKER_NAME="" bash ./h-config.sh >/dev/null 2>&1 )
_lines="$(wc -l < "$_nl/extra-flags" | tr -d '[:space:]')"
[ "$_lines" = "1" ] && ok "multi-line extras are flattened to a single extra-flags line" \
  || no "extra-flags kept interior newlines (tokens can hide from the strip)" "$_lines lines" "1 line"
rm -rf "$_nl"

_o4="$(render_out "$(printf -- '--backend cuda --device\n0')")"
case "$_o4" in
  *"WARNING"*"--device"*) ok "warns about a newline-split --device too" ;;
  *) no "no warning for a newline-split --device" "$_o4" "a h-config WARNING mentioning --device" ;;
esac

echo "== the literal text '--device' inside a NAME is not mangled =="
# "--device" is only a flag at a word boundary. A rig or worker name that merely
# CONTAINS the substring must survive intact — both when it arrives as the rig
# name HiveOS exports ($WORKER_NAME, which is baked into config.toml and never
# goes through the extras strip at all) and when it is the value of a --worker
# flag in the extras box (where the strip must not fire mid-word).
checkflags_ws "--worker value containing '--device' survives the strip" \
  "--worker rig--device-01 --backend cuda"    "--worker rig--device-01 --backend cuda"
checkflags_ws "…and survives when the extras are newline-separated" \
  "$(printf -- '--worker rig--device-01\n--backend cuda')" "--worker rig--device-01 --backend cuda"

_w="$(mktemp -d)"; cp "$HCONF" "$_w/h-config.sh"
printf 'CUSTOM_CONFIG_FILENAME=%s/config.toml\n' "$_w" > "$_w/h-manifest.conf"
( cd "$_w" && CUSTOM_TEMPLATE="$ADDR40" CUSTOM_USER_CONFIG="--backend cuda" \
    WORKER_NAME="rig--device-01" bash ./h-config.sh >/dev/null 2>&1 )
_gotw="$(sed -n 's/^worker = "\([^"]*\)".*/\1/p' "$_w/config.toml" 2>/dev/null | head -1)"
[ "$_gotw" = "rig--device-01" ] \
  && ok "a rig name containing '--device' is baked into config.toml unmangled" \
  || no "rig name containing '--device' was mangled" "$_gotw" "rig--device-01"
rm -rf "$_w"

echo "== the stripped value never reaches the miner argv (h-run.sh contract) =="
# h-run.sh emits its own --device BEFORE $EXTRA_FLAGS at BOTH launch sites, and
# clap is last-wins, so this file is the ONLY thing standing between the extras
# box and a collapsed rig. Pin that h-run.sh still relies on the sanitised file.
grep -qF 'EXTRA_FLAGS="$(cat "$EXTRA_FLAGS_FILE")"' "$ROOT/hiveos/h-run.sh" 2>/dev/null \
  && ok "h-run.sh still sources its extras from the h-config-sanitised extra-flags file" \
  || no "h-run.sh no longer reads extra-flags (sanitisation bypassed)" "unmatched" "EXTRA_FLAGS=\$(cat \$EXTRA_FLAGS_FILE)"

echo
echo "  Passed: $pass  Failed: $fail"
[ "$fail" -eq 0 ] && { echo "ALL HIVEOS --device STRIP ASSERTIONS PASSED"; exit 0; } || { echo "DEVICE-STRIP TEST FAILED"; exit 1; }
