#!/usr/bin/env bash
# tests/hiveos_stats_multigpu.sh — whole-rig stats reporting in hiveos/h-stats.sh.
#
# THE BUG: h-stats.sh scraped exactly ONE stats port ($CUSTOM_API_PORT, default
# 3380) = device 0. But the binary is one-process-one-device, and h-run.sh
# (hive_launch_extra_gpus) puts every extra card on PORT+1, PORT+2, … So a 6-GPU
# rig reported ~1/6 of its real hashrate to HiveOS.
#
# WHY IT IS NOT COSMETIC: HiveOS's own hashrate watchdog compares the reported
# rate against a threshold. Under-reporting 6x can make HiveOS kill and restart a
# perfectly healthy rig, in a loop. This is an UPTIME bug.
#
# THE FIX: probe PORT, PORT+1, … , scrape every LIVE port, and merge the objects
# into one h-stats payload with one hs[] element PER GPU. The merge is pure
# AGGREGATION of values the Rust subcommand already converted to kH/s — it never
# re-derives a unit, so the §G7 "÷1000 clamp" hazard stays confined to Rust.
#
# WHY LIVENESS IS PROBED SEPARATELY: `hiveos-stats` ALWAYS exits 0 and ALWAYS
# prints a valid object — on a dead port it prints the ZERO object
# (src/main.rs:1099-1112). Its output therefore cannot distinguish a dead port
# from a live-but-idle card, so h-stats.sh TCP-probes first and only scrapes the
# ports that answer.
#
# WHAT THIS PINS (functions sourced out of h-stats.sh under a stub sandbox —
# mirroring tests/hiveos_multigpu.sh:plan()):
#   hs_extract()    — ONE h-stats object -> a flat tab-separated record.
#                     Two interchangeable implementations (jq / awk fallback);
#                     both are exercised, and both must agree byte-for-byte.
#   hs_merge()      — flat records -> ONE merged h-stats object. Pure.
#   hs_live_ports() — the bounded port scan (miss tolerance, cap, budget).
#   plus the end-to-end $stats/$khs contract, run with h-stats.sh SOURCED the way
#   the HiveOS agent sources it.
#
# ASSERTIONS (each FAILS before the fix — h-stats.sh has no such functions, and
# its end-to-end output reports only device 0 — and PASSES after):
#   (a) 1 GPU            => output BYTE-IDENTICAL to the legacy single-port path
#   (b) 6 GPUs           => hs[] has 6 elements, khs = the true rig total
#   (c) 6 GPUs, #3 dead  => the gap does NOT truncate the scan; 5 cards reported
#   (d) all dead         => a valid zero object, never empty / never a crash
#   (e) malformed JSON   => the bad port is dropped, the rest of the rig reports
#   (f) timeout path     => a wedged port cannot hang the hook
#   plus merge-semantics, extractor-parity and structural guards.
#
# Run:  bash tests/hiveos_stats_multigpu.sh
# Exit: 0 = all pass

set -uo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]:-$0}")/.." && pwd)"
HSTATS="$ROOT/hiveos/h-stats.sh"
PASS=0; FAIL=0
ok()   { echo "  [PASS] $1"; PASS=$((PASS + 1)); }
fail() { echo "  [FAIL] $1" >&2; echo "         $2" >&2; FAIL=$((FAIL + 1)); }

eq() { # label want got
  if [ "$2" = "$3" ]; then ok "$1"; else fail "$1" "$(printf 'expected: %s\n     got: %s' "$2" "$3")"; fi
}

# Make an executable stub on the sandbox PATH.
stub() { local dir="$1" name="$2" body="$3"; printf '#!/usr/bin/env bash\n%s\n' "$body" > "$dir/$name"; chmod +x "$dir/$name"; }

# Extract the pure/probe functions from h-stats.sh into a sourceable file.
# A missing function becomes a __NO_<NAME>__ marker so a pre-fix run produces an
# unmistakable diff and the assertion fails cleanly rather than mysteriously.
FNS="$(mktemp)"
: > "$FNS"
for f in hs_log hs_gpu_count hs_extract hs_merge hs_live_ports hs_port_live hs_scrape; do
  if grep -qE "^$f\(\)[[:space:]]*\{" "$HSTATS"; then
    awk -v fn="$f" '$0 ~ "^"fn"\\(\\)[[:space:]]*\\{"{f=1} f{print} f&&/^\}/{exit}' "$HSTATS" >> "$FNS"
  else
    printf '%s() { echo "__NO_%s__"; }\n' "$f" "$(printf '%s' "$f" | tr '[:lower:]' '[:upper:]')" >> "$FNS"
  fi
done

# An h-stats object exactly as the Rust subcommand emits it (alphabetical keys,
# verified against `csd-gpu-miner hiveos-stats` on a dead port).
obj() { # khs good rej stale uptime [temp]
  local khs="$1" g="$2" r="$3" st="$4" up="$5" tmp="${6:-}"
  local tarr="[]"; [ -n "$tmp" ] && tarr="[$tmp]"
  printf '{"algo":"sha256d","ar":[%s,%s,0,%s],"fan":[],"hs":[%s],"hs_units":"khs","temp":%s,"uptime":%s,"ver":"0.2.3"}' \
    "$g" "$r" "$st" "$khs" "$tarr" "$up"
}

echo
echo "=== hiveos_stats_multigpu: report the WHOLE rig, not just GPU 0 ==="
echo

# ── 1. hs_extract: object -> flat record, BOTH implementations ────────────────
echo "-- hs_extract: object -> flat record (awk fallback AND jq) --"

SB="$(mktemp -d)"; mkdir -p "$SB/bin"

# FIXED 2026-07-19. This used to be PATH="$SB/bin:/usr/bin:/bin" and was labelled
# "empty bin => no jq on PATH => awk path". That was FALSE on any host with
# /usr/bin/jq: `command -v jq` found it, so every "awk fallback" case below
# actually exercised jq, and case (4) failed for a reason that had nothing to do
# with awk. $SB/nojq holds ONLY the tools hs_extract's awk path needs, so the
# awk branch is genuinely the one under test — on a dev box and on a bare rig.
mkdir -p "$SB/nojq"
for t in bash sh awk gawk mawk sed grep cat tr head; do
  p="$(command -v "$t" 2>/dev/null)" && ln -sf "$p" "$SB/nojq/$t"
done
if [ -n "$(command -v jq 2>/dev/null)" ] && PATH="$SB/nojq" command -v jq >/dev/null 2>&1; then
  fail "harness: awk-path isolation" "jq is still reachable from \$SB/nojq; the 'awk fallback' cases would silently test jq instead"
fi

extract_awk() { PATH="$SB/nojq" bash -c 'set -uo pipefail; source "$1"; hs_extract' _ "$FNS"; }

R="$(obj 4058000 42 3 5 3600 65 | extract_awk)"
eq "(1) awk extractor pulls khs/temp/uptime/ar/ver" \
   "$(printf '4058000\t65\t3600\t42\t3\t0\t5\t0.2.3')" "$R"

R="$(obj 0 0 0 0 0 | extract_awk)"
eq "(2) awk extractor: empty temp[] => empty temp field (non-nvml fleet build)" \
   "$(printf '0\t\t0\t0\t0\t0\t0\t0.2.3')" "$R"

# Garbage in => nothing out (hs_merge then ignores it).
eq "(3) awk extractor drops a non-JSON line" "" "$(printf 'not json at all\n' | extract_awk)"
eq "(4) awk extractor drops an object with no hs[]" "" \
   "$(printf '{"algo":"sha256d","uptime":5}\n' | extract_awk)"

# The jq path must produce a byte-identical record. Stub a jq only if the real
# one is absent, so this runs on a bare rig and on a dev box alike.
if command -v jq >/dev/null 2>&1; then
  JQSB="$(mktemp -d)"; mkdir -p "$JQSB/bin"
  cp "$(command -v jq)" "$JQSB/bin/jq" 2>/dev/null || ln -s "$(command -v jq)" "$JQSB/bin/jq"
  RJ="$(obj 4058000 42 3 5 3600 65 | PATH="$JQSB/bin:/usr/bin:/bin" bash -c 'set -uo pipefail; source "$1"; hs_extract' _ "$FNS")"
  eq "(5) jq extractor agrees with the awk extractor byte-for-byte" \
     "$(printf '4058000\t65\t3600\t42\t3\t0\t5\t0.2.3')" "$RJ"
  # PARITY ON MALFORMED INPUT — the gap that let a real defect sit green. (5)
  # only compared the two implementations on a WELL-FORMED object, where they
  # trivially agree. On an object with no hs[] they did NOT: jq's `.hs[0]//0`
  # returns 0 for ANY valid JSON, so a foreign service occupying a stats port
  # was extracted as a real card reporting 0 kH/s, while awk correctly dropped
  # it. Fixed in h-stats.sh with a `select((.hs|type)=="array" ...)` guard.
  extract_jq() { PATH="$JQSB/bin:$SB/nojq" bash -c 'set -uo pipefail; source "$1"; hs_extract' _ "$FNS"; }
  eq "(5a) jq extractor ALSO drops an object with no hs[] (parity on malformed input)" \
     "" "$(printf '{"algo":"sha256d","uptime":5}\n' | extract_jq)"
  eq "(5b) jq extractor ALSO drops an empty hs[]" "" \
     "$(printf '{"algo":"sha256d","hs":[],"uptime":5}\n' | extract_jq)"
  eq "(5c) jq extractor drops a non-JSON line" "" "$(printf 'not json at all\n' | extract_jq)"
  eq "(5d) awk extractor drops an empty hs[] (the other half of 5b)" "" \
     "$(printf '{"algo":"sha256d","hs":[],"uptime":5}\n' | extract_awk)"
  rm -rf "$JQSB"
else
  ok "(5) jq extractor parity SKIPPED (no jq on this host; awk path is the rig default)"
fi

# ── 2. hs_merge: flat records -> one object ───────────────────────────────────
echo
echo "-- hs_merge: per-GPU hs[], summed shares, max uptime --"

merge() { PATH="$SB/bin:/usr/bin:/bin" bash -c 'set -uo pipefail; source "$1"; hs_merge' _ "$FNS"; }

R="$(printf '100\t\t10\t1\t0\t0\t0\t0.2.3\n200\t\t20\t2\t1\t0\t3\t0.2.3\n' | merge)"
eq "(6) two cards => hs[] has BOTH rates, ar[] summed, uptime = max" \
   '{"algo":"sha256d","ar":[3,1,0,3],"fan":[],"hs":[100,200],"hs_units":"khs","temp":[],"uptime":20,"ver":"0.2.3"}' "$R"

R="$(printf '100\t60\t10\t0\t0\t0\t0\t0.2.3\n200\t70\t20\t0\t0\t0\t0\t0.2.3\n' | merge)"
eq "(7) temps present => temp[] parallel to hs[], one per card" \
   '{"algo":"sha256d","ar":[0,0,0,0],"fan":[],"hs":[100,200],"hs_units":"khs","temp":[60,70],"uptime":20,"ver":"0.2.3"}' "$R"

# Mixed nvml/non-nvml is the misalignment trap: temp[] MUST stay positionally
# parallel to hs[], so a card with no sensor is padded with 0 rather than shifting
# every later card's temperature onto the wrong GPU.
R="$(printf '100\t60\t10\t0\t0\t0\t0\t0.2.3\n200\t\t20\t0\t0\t0\t0\t0.2.3\n300\t80\t5\t0\t0\t0\t0\t0.2.3\n' | merge)"
eq "(8) mixed temp coverage => padded with 0, NOT shifted out of alignment" \
   '{"algo":"sha256d","ar":[0,0,0,0],"fan":[],"hs":[100,200,300],"hs_units":"khs","temp":[60,0,80],"uptime":20,"ver":"0.2.3"}' "$R"

if printf '' | merge >/dev/null 2>&1; then
  fail "(9) empty input exits non-zero" "hs_merge succeeded on empty input; the caller would emit an empty \$stats"
else
  ok "(9) no records => hs_merge exits non-zero so the caller can fall back"
fi

# ── 3. hs_live_ports: the bounded scan ───────────────────────────────────────
echo
echo "-- hs_live_ports: bounded probe (miss tolerance / cap / budget) --"

# Drive the real scan with a stubbed liveness probe: LIVE_SET lists the live ports.
scan() { # $1 = space-separated live ports, $2.. = env overrides
  local live="$1"; shift
  env "$@" LIVE_SET="$live" PORT=3380 bash -c '
    set -uo pipefail
    source "$1"
    hs_port_live() { case " $LIVE_SET " in *" $1 "*) return 0 ;; *) return 1 ;; esac; }
    HS_MAX_GPUS="${HS_MAX_GPUS:-32}"; HS_MISS_TOL="${HS_MISS_TOL:-4}"; HS_BUDGET="${HS_BUDGET:-8}"
    hs_live_ports
  ' _ "$FNS" | tr '\n' ' ' | sed 's/ $//'
}

eq "(10) 1 GPU  => only the base port" "3380" "$(scan "3380")"
eq "(11) 6 GPUs => all six consecutive ports" \
   "3380 3381 3382 3383 3384 3385" "$(scan "3380 3381 3382 3383 3384 3385")"
eq "(12) 6 GPUs with #3 dead => the GAP does not truncate the scan" \
   "3380 3381 3382 3384 3385" "$(scan "3380 3381 3382 3384 3385")"
eq "(13) all dead => no ports (caller falls back to the zero object)" "" "$(scan "")"
# A --gpu-id 0,2 include-list leaves a one-port hole; tolerance must span it.
eq "(14) --gpu-id style hole (0,2) => both cards still found" \
   "3380 3382" "$(scan "3380 3382")"
# Beyond the miss tolerance the scan stops — that bound is what keeps the hook fast.
eq "(15) a card past HS_MISS_TOL consecutive misses is not scanned (bound holds)" \
   "3380" "$(scan "3380 3390" HS_MISS_TOL=4)"
eq "(16) HS_MAX_GPUS caps the scan" "3380 3381" \
   "$(scan "3380 3381 3382 3383" HS_MAX_GPUS=2)"

# ── 4. End-to-end: source h-stats.sh the way the HiveOS agent does ───────────
echo
echo "-- end-to-end: \$stats / \$khs with h-stats.sh SOURCED --"

# Build a sandbox that looks like a HiveOS install dir: a fake miner binary whose
# `hiveos-stats --stats-port N` answers per-port from a table, and a fake
# h-manifest.conf. LIVE_SET drives the TCP-probe override.
# The probe is bash /dev/tcp — we cannot open real sockets portably in CI, so the
# e2e cases drive it through a stubbed `timeout` that consults LIVE_SET. This is
# the ONLY seam: everything downstream (scrape, extract, merge, khs) is real.
e2e() { # $1 = live ports, $2 = table, $3.. = extra env
  local live="$1" table="$2"; shift 2
  local d; d="$(mktemp -d)"; mkdir -p "$d/bin"
  printf '%s\n' "$table" > "$d/table"
  cat > "$d/csd-gpu-miner" <<'STUB'
#!/usr/bin/env bash
port=""
while [ $# -gt 0 ]; do
  case "$1" in --stats-port) port="$2"; shift 2 ;; *) shift ;; esac
done
row="$(grep -E "^${port}=" "$(dirname "$0")/table" 2>/dev/null | head -1)"
row="${row#*=}"
if [ -z "$row" ]; then
  echo '{"algo":"sha256d","ar":[0,0,0,0],"fan":[],"hs":[0],"hs_units":"khs","temp":[],"uptime":0,"ver":"0.2.3"}'
  exit 0
fi
case "$row" in
  RAWGARBAGE) echo 'this is not json'; exit 0 ;;
  HANG)       sleep 30; exit 0 ;;
esac
IFS=':' read -r khs g r st up tmp <<< "$row"
tarr="[]"; [ -n "${tmp:-}" ] && tarr="[$tmp]"
printf '{"algo":"sha256d","ar":[%s,%s,0,%s],"fan":[],"hs":[%s],"hs_units":"khs","temp":%s,"uptime":%s,"ver":"0.2.3"}\n' \
  "$g" "$r" "$st" "$khs" "$tarr" "$up"
exit 0
STUB
  chmod +x "$d/csd-gpu-miner"
  : > "$d/h-manifest.conf"
  cp "$HSTATS" "$d/h-stats.sh"
  # `timeout` stub: h-stats.sh probes with `timeout N bash -c 'exec 3<>/dev/tcp/…P'`
  # and scrapes with `timeout N <bin> hiveos-stats …`. We intercept the probe form
  # (answer from LIVE_SET) and pass the scrape form through to the real timeout.
  cat > "$d/bin/timeout" <<'TSTUB'
#!/usr/bin/env bash
secs="$1"; shift
if [ "${1:-}" = "bash" ]; then
  # the /dev/tcp liveness probe — resolve it from LIVE_SET instead of a real
  # socket. NB: take the LAST argument then strip; "${*##*/}" would apply the
  # removal per-argument and rejoin ("bash -c 3381").
  p="${*: -1}"; p="${p##*/}"
  [ -n "${HS_PROBE_COUNT_FILE:-}" ] && echo "$p" >> "$HS_PROBE_COUNT_FILE"
  case " ${LIVE_SET:-} " in *" $p "*) exit 0 ;; *) exit 1 ;; esac
fi
exec /usr/bin/timeout "$secs" "$@"
TSTUB
  chmod +x "$d/bin/timeout"
  # `nvidia-smi` stub: h-stats.sh now bounds the scan by the REAL device count
  # (NEW-C), which it learns exactly the way h-run.sh:hive_gpu_count does. With
  # NVSMI_GPUS unset it prints no "GPU N:" lines => count 0 => h-stats.sh keeps
  # the legacy blind-32 bound, so every pre-existing case below is unaffected.
  cat > "$d/bin/nvidia-smi" <<'NSTUB'
#!/usr/bin/env bash
n="${NVSMI_GPUS:-0}"
if [ "${1:-}" = "-L" ]; then
  i=0; while [ "$i" -lt "$n" ]; do echo "GPU $i: NVIDIA Stub (UUID: GPU-stub-$i)"; i=$((i + 1)); done
fi
exit 0
NSTUB
  chmod +x "$d/bin/nvidia-smi"
  # Count every liveness probe so the scan bound can be asserted directly.
  # e2e is called inside $( ), so any variable it sets is lost with the subshell.
  # Side-channel stderr and the probe count through $SB, which outlives it.
  : > "$SB/last-probes"
  env "$@" LIVE_SET="$live" HS_PROBE_COUNT_FILE="$SB/last-probes" PATH="$d/bin:/usr/bin:/bin" bash -c '
    set -uo pipefail
    cd "$1"
    source ./h-stats.sh
    printf "%s\n%s\n" "$khs" "$stats"
  ' _ "$d" 2>"$SB/last-err"
  rm -rf "$d"
}
e2e_err()    { cat "$SB/last-err" 2>/dev/null; }
e2e_probes() { wc -l < "$SB/last-probes" 2>/dev/null | tr -d ' '; }

# (a) 1 GPU: the output must be BYTE-IDENTICAL to the legacy single-port path —
# i.e. exactly what `hiveos-stats --stats-port 3380` printed, verbatim.
OUT="$(e2e "3380" "3380=4058000:42:3:5:3600")"
E_KHS="$(printf '%s' "$OUT" | sed -n 1p)"
E_STATS="$(printf '%s' "$OUT" | sed -n 2p)"
eq "(a1) 1 GPU: \$stats is the subcommand object VERBATIM (byte-identical to legacy)" \
   "$(obj 4058000 42 3 5 3600)" "$E_STATS"
eq "(a2) 1 GPU: \$khs unchanged" "4058000" "$E_KHS"

# (b) 6 GPUs: six hs[] elements, khs = the true rig total (NOT 1/6 of it).
TBL="3380=4058000:10:0:0:100
3381=4058000:10:0:0:100
3382=4058000:10:1:0:100
3383=4058000:10:0:2:100
3384=4058000:10:0:0:100
3385=4058000:10:0:0:900"
OUT="$(e2e "3380 3381 3382 3383 3384 3385" "$TBL")"
E_KHS="$(printf '%s' "$OUT" | sed -n 1p)"
E_STATS="$(printf '%s' "$OUT" | sed -n 2p)"
eq "(b1) 6 GPUs: hs[] has one element PER CARD" \
   '[4058000,4058000,4058000,4058000,4058000,4058000]' \
   "$(printf '%s' "$E_STATS" | sed -n 's/.*"hs":\(\[[^]]*\]\).*/\1/p')"
eq "(b2) 6 GPUs: \$khs is the WHOLE rig (6x), not GPU 0 alone" "24348000" "$E_KHS"
eq "(b3) 6 GPUs: ar[] summed across every process" '[60,1,0,2]' \
   "$(printf '%s' "$E_STATS" | sed -n 's/.*"ar":\(\[[^]]*\]\).*/\1/p')"
eq "(b4) 6 GPUs: uptime is the max across processes" '900' \
   "$(printf '%s' "$E_STATS" | sed -n 's/.*"uptime":\([0-9]*\).*/\1/p')"

# (c) 6 GPUs with #3 (port 3383) dead: the gap must NOT truncate the scan, and
# the five surviving cards must all still report — EACH AT ITS OWN INDEX.
#
# CORRECTED 2026-07-19 (was '[4058000 x5]', a 5-element array).
# PROOF the old expectation was wrong, not the code: h-run.sh:257 emits
# `BG device=<i> port=$((PORT + i))`, so port 3384 IS device 4. A 5-element hs[]
# makes HiveOS — which indexes hs[] positionally per card, as this file and
# h-stats.sh:24 both state — paint device 4's rate onto GPU 3, device 5's onto
# GPU 4, and show nothing for GPU 5. The old assertion pinned that shift-left as
# correct. $khs is unchanged either way (c2 still expects 20290000 and passes),
# which is exactly why the defect shipped green: the total looks right.
TBL="3380=4058000:10:0:0:100
3381=4058000:10:0:0:100
3382=4058000:10:0:0:100
3384=4058000:10:0:0:100
3385=4058000:10:0:0:100"
OUT="$(e2e "3380 3381 3382 3384 3385" "$TBL")"
E_KHS="$(printf '%s' "$OUT" | sed -n 1p)"
E_STATS="$(printf '%s' "$OUT" | sed -n 2p)"
eq "(c1) 6 GPUs, #3 dead: 5 LIVE cards report AT THEIR OWN INDEX, slot 3 = 0" \
   '[4058000,4058000,4058000,0,4058000,4058000]' \
   "$(printf '%s' "$E_STATS" | sed -n 's/.*"hs":\(\[[^]]*\]\).*/\1/p')"
eq "(c2) 6 GPUs, #3 dead: \$khs = the 5 survivors, partial rig still reports" \
   "20290000" "$E_KHS"

# (d) All dead: a valid ZERO object — never empty, never a crash.
OUT="$(e2e "" "")"
E_KHS="$(printf '%s' "$OUT" | sed -n 1p)"
E_STATS="$(printf '%s' "$OUT" | sed -n 2p)"
eq "(d1) all dead: \$khs = 0" "0" "$E_KHS"
eq "(d2) all dead: \$stats is the valid zero object (legacy byte-identical)" \
   "$(obj 0 0 0 0 0)" "$E_STATS"

# (e) One port returns malformed JSON: drop THAT CARD, keep the rest of the rig —
# and keep every survivor on its own index.
#
# CORRECTED 2026-07-19 (was '[4058000,4058000]'). Port 3381 is device 1; a
# 2-element hs[] would show device 2's rate as GPU 1. "Dropped" must mean a ZERO
# at index 1, not a hole that shifts the array. $khs is unchanged (e2 passes).
TBL="3380=4058000:10:0:0:100
3381=RAWGARBAGE
3382=4058000:10:0:0:100"
OUT="$(e2e "3380 3381 3382" "$TBL")"
E_KHS="$(printf '%s' "$OUT" | sed -n 1p)"
E_STATS="$(printf '%s' "$OUT" | sed -n 2p)"
eq "(e1) malformed JSON from one port: that card zeroes, others keep their index" \
   '[4058000,0,4058000]' \
   "$(printf '%s' "$E_STATS" | sed -n 's/.*"hs":\(\[[^]]*\]\).*/\1/p')"
eq "(e2) malformed JSON: \$khs counts the two good cards" "8116000" "$E_KHS"

# (f) TIMEOUT path: a wedged process must not hang the hook. Port 3381 sleeps 30s;
# with a 1s scrape timeout the hook must finish fast AND still report the others.
T0=$SECONDS
TBL="3380=4058000:10:0:0:100
3381=HANG
3382=4058000:10:0:0:100"
OUT="$(e2e "3380 3381 3382" "$TBL" CSD_HSTATS_SCRAPE_TIMEOUT=1)"
ELAPSED=$((SECONDS - T0))
E_KHS="$(printf '%s' "$OUT" | sed -n 1p)"
E_STATS="$(printf '%s' "$OUT" | sed -n 2p)"
if [ "$ELAPSED" -le 10 ]; then
  ok "(f1) wedged port: hook returned in ${ELAPSED}s (bounded, << the 30s hang)"
else
  fail "(f1) wedged port bounded" "hook took ${ELAPSED}s — the scrape timeout did not bound it"
fi
# CORRECTED 2026-07-19 (was '[4058000,4058000]'): same shift-left as (e1). The
# wedged card is device 1, so it must occupy index 1 as a 0. $khs unchanged (f3).
eq "(f2) wedged port: healthy cards keep their index, the wedged slot is 0" \
   '[4058000,0,4058000]' \
   "$(printf '%s' "$E_STATS" | sed -n 's/.*"hs":\(\[[^]]*\]\).*/\1/p')"
eq "(f3) wedged port: \$khs counts the healthy cards" "8116000" "$E_KHS"

# ── 4b. POSITIONAL ALIGNMENT (HIGH-3) ────────────────────────────────────────
# h-run.sh:257 assigns `port = PORT + device` (hive_multi_gpu_plan emits
#   BG device=<i> port=$((PORT + i))).
# So the device index is RECOVERABLE as (port - PORT) and NOTHING ELSE may be
# used to place a card in hs[]. Collecting in scan order and appending
# (`n++; hs[n]=…`) discards that index: on `--gpu-id 0,2` the rig reports
# hs:[card0, card2] and HiveOS — which indexes hs[]/temp[] POSITIONALLY per card,
# exactly as this file's own header states — displays card 2's rate as GPU 1.
#
# WHY THIS IS WORSE THAN THE BUG IT REPLACED: the TOTAL khs stays correct, so the
# hashrate watchdog is satisfied and the rig looks healthy. Only the per-card
# diagnosis is silently wrong, and nothing anywhere signals it. The under-report
# it replaced at least announced itself.
#
# THE CONTRACT: index by (port - PORT); pad every gap with 0 in BOTH hs[] and
# temp[]; emit up to the highest index that actually produced a record.
echo
echo "-- positional alignment: hs[] index MUST equal the device index --"

# (g) --gpu-id 0,2 — a one-card hole. Card 2 belongs at index 2, index 1 is 0.
TBL="3380=1000:10:0:0:100
3382=3000:10:0:0:100"
OUT="$(e2e "3380 3382" "$TBL")"
E_KHS="$(printf '%s' "$OUT" | sed -n 1p)"
E_STATS="$(printf '%s' "$OUT" | sed -n 2p)"
eq "(g1) --gpu-id 0,2: card 2's rate sits at INDEX 2, index 1 padded 0" \
   '[1000,0,3000]' \
   "$(printf '%s' "$E_STATS" | sed -n 's/.*"hs":\(\[[^]]*\]\).*/\1/p')"
eq "(g2) --gpu-id 0,2: padding does not change the rig total" "4000" "$E_KHS"

# (g3) Two holes mid-rig (cards 1 and 3 of 6 die): every survivor keeps its own
# index; the dead slots are 0, never a shift-left.
TBL="3380=1000:10:0:0:100
3382=3000:10:0:0:100
3384=5000:10:0:0:100
3385=6000:10:0:0:100"
OUT="$(e2e "3380 3382 3384 3385" "$TBL")"
E_STATS="$(printf '%s' "$OUT" | sed -n 2p)"
eq "(g3) cards 1 and 3 dead mid-run: indices stay aligned, dead slots are 0" \
   '[1000,0,3000,0,5000,6000]' \
   "$(printf '%s' "$E_STATS" | sed -n 's/.*"hs":\(\[[^]]*\]\).*/\1/p')"

# (g4) Only card 5 survives. hs[] must still be SIX elements with the rate at
# index 5 — not a one-element array that HiveOS would paint onto GPU 0.
TBL="3385=6000:10:0:0:100"
OUT="$(e2e "3385" "$TBL")"
E_KHS="$(printf '%s' "$OUT" | sed -n 1p)"
E_STATS="$(printf '%s' "$OUT" | sed -n 2p)"
eq "(g4) only card 5 alive: 6 elements, the first five 0, rate at index 5" \
   '[0,0,0,0,0,6000]' \
   "$(printf '%s' "$E_STATS" | sed -n 's/.*"hs":\(\[[^]]*\]\).*/\1/p')"
eq "(g5) only card 5 alive: khs is that one card" "6000" "$E_KHS"

# (g6) temp[] must be padded on the SAME indices as hs[], or a gap silently
# reassigns every later card's temperature to the wrong GPU.
TBL="3380=1000:10:0:0:100:60
3382=3000:10:0:0:100:80"
OUT="$(e2e "3380 3382" "$TBL")"
E_STATS="$(printf '%s' "$OUT" | sed -n 2p)"
eq "(g6) temp[] padded on the same indices as hs[]" \
   '[60,0,80]' \
   "$(printf '%s' "$E_STATS" | sed -n 's/.*"temp":\(\[[^]]*\]\).*/\1/p')"

# (g7) MEDIUM-1: a SPARSE `--gpu-id 0,8` rig leaves ports 3381-3387 dead — SEVEN
# consecutive misses. With HS_MISS_TOL=4 the scan gives up at 3384 and card 8 is
# never scraped: precisely the under-reporting this file exists to fix, in the
# sparse case. The cap and the per-probe timeout already bound the scan, so the
# miss tolerance buys nothing and must not truncate a legal rig.
TBL="3380=1000:10:0:0:100
3388=9000:10:0:0:100"
OUT="$(e2e "3380 3388" "$TBL")"
E_KHS="$(printf '%s' "$OUT" | sed -n 1p)"
E_STATS="$(printf '%s' "$OUT" | sed -n 2p)"
eq "(g7) --gpu-id 0,8: 7-port gap does NOT truncate the scan; card 8 at index 8" \
   '[1000,0,0,0,0,0,0,0,9000]' \
   "$(printf '%s' "$E_STATS" | sed -n 's/.*"hs":\(\[[^]]*\]\).*/\1/p')"
eq "(g8) --gpu-id 0,8: both cards counted in khs" "10000" "$E_KHS"

# (g9) MEDIUM-2: HS_BUDGET is documented as bounding the WHOLE scan, but it is
# enforced only inside hs_live_ports. The scrape loop has just a per-call
# HS_SCRAPE_TIMEOUT, so the worst case is BUDGET + MAX_GPUS x SCRAPE_TIMEOUT
# (~8 + 32x3 ~= 104s) against a ~10s HiveOS poll — overlapping resident
# invocations and permanently stale stats, on exactly the wedged-but-listening
# rig this file targets.
# Eight wedged ports x 2s = 16s unguarded; with the budget honoured in the
# scrape loop it must break out around BUDGET + one in-flight scrape.
TBL="3380=HANG
3381=HANG
3382=HANG
3383=HANG
3384=HANG
3385=HANG
3386=HANG
3387=HANG"
T0=$SECONDS
OUT="$(e2e "3380 3381 3382 3383 3384 3385 3386 3387" "$TBL" \
        CSD_HSTATS_SCRAPE_TIMEOUT=2 CSD_HSTATS_BUDGET=3 CSD_HSTATS_MAX_GPUS=8)"
ELAPSED=$((SECONDS - T0))
# Ceiling 12s: measured 5-7s guarded, and 19s unguarded on the pre-fix code, so
# the gap is wide enough that this discriminates without flaking on fork latency.
if [ "$ELAPSED" -le 12 ]; then
  ok "(g9) HS_BUDGET bounds the SCRAPE loop too: ${ELAPSED}s (unguarded measured 19s)"
else
  fail "(g9) HS_BUDGET bounds the scrape loop" \
       "took ${ELAPSED}s — the budget is enforced only in hs_live_ports, so a wedged rig overruns the ~10s HiveOS poll"
fi

# (g10) LOW-3: the merge lifts `ver` straight into printf "%s" with no escaping,
# so ANY local process that binds an unused port in range can forge JSON keys in
# the object the rig reports. Sanitise with the same allowlist idiom h-config.sh
# already uses for WNAME.
R="$(printf '100\t\t10\t0\t0\t0\t0\t0.2.3","evil":"x\n' | merge)"
eq "(g10) a forged ver cannot inject JSON keys (allowlist-sanitised)" \
   '{"algo":"sha256d","ar":[0,0,0,0],"fan":[],"hs":[100],"hs_units":"khs","temp":[],"uptime":10,"ver":"0.2.3evilx"}' "$R"

# ── 4c. NEW-C: the scan bound, the probe budget, and the LOUD fallback ───────
# The 32-port blind scan was a REGRESSION INTO THE ORIGINAL BUG. Measured on
# WSL2 (closed loopback stalls the connect — the pessimal case, and the one a
# wedged rig produces): 32 dead ports cost 33154 ms timeout-bounded and 65093 ms
# with a bare /dev/tcp subshell. Against a ~10s HiveOS poll the probe ate the
# whole HS_BUDGET, the scrape loop broke on iteration 1, hs_merge got nothing,
# and the fallback scraped $PORT alone => the ~1/N under-report was BACK, with
# no log line anywhere. Three defences, each pinned here.
echo
echo "-- NEW-C: device-count scan bound, probe budget, LOUD fallback --"

# (h1) The bound is the REAL device count. h-run.sh:hive_multi_gpu_plan launches
# device d only when `d < gpu_count` (:256) and puts it on PORT+d, so every live
# stats port is inside [PORT, PORT+count-1]. Bounding by the count is therefore
# COMPLETE, not a heuristic — and on a 6-GPU rig it is 6 probes, not 32.
OUT="$(e2e "3380 3381 3382 3383 3384 3385" "3380=1000:0:0:0:100
3381=1000:0:0:0:100
3382=1000:0:0:0:100
3383=1000:0:0:0:100
3384=1000:0:0:0:100
3385=1000:0:0:0:100" NVSMI_GPUS=6)"
eq "(h1) 6-GPU rig: the scan probes 6 ports, not 32 (5.3x fewer connect stalls)" \
   "6" "$(e2e_probes)"
eq "(h2) 6-GPU rig: bounding by the device count still reports the WHOLE rig" \
   "6000" "$(printf '%s' "$OUT" | sed -n 1p)"

# (h3) Sparse `--gpu-id 0,8` is still complete: h-run.sh only honours id 8 on a
# rig with >=9 cards, so the count-derived bound reaches it.
OUT="$(e2e "3380 3388" "3380=1000:10:0:0:100
3388=9000:10:0:0:100" NVSMI_GPUS=9)"
eq "(h3) --gpu-id 0,8 on a 9-GPU box: card 8 still found under the count bound" \
   '[1000,0,0,0,0,0,0,0,9000]' \
   "$(printf '%s' "$OUT" | sed -n 2p | sed -n 's/.*"hs":\(\[[^]]*\]\).*/\1/p')"
eq "(h4) --gpu-id 0,8: exactly 9 probes (the count), never 32" "9" "$(e2e_probes)"

# (h5) Detection failure (cpu variant / container / no nvidia-smi) must DEGRADE
# to the old blind bound, never to something narrower — a narrower guess would
# drop real cards. Every earlier case in this file runs with NVSMI_GPUS unset
# and still reaches port 3388 (g7), which is that guarantee in action.
OUT="$(e2e "3380 3388" "3380=1000:10:0:0:100
3388=9000:10:0:0:100")"
eq "(h5) no GPU count available => legacy 32-port bound, card 8 NOT dropped" \
   "10000" "$(printf '%s' "$OUT" | sed -n 1p)"

# (h6) THE MANDATORY ONE. When the fallback fires it MUST say so. A fix that
# fails back into the bug it fixes, invisibly, is worse than no fix: the rig
# under-reports, HiveOS's hashrate watchdog may kill it, and nothing in any log
# points at h-stats.sh. All ports dead => fallback => a line on stderr.
OUT="$(e2e "" "")"
ERR="$(e2e_err)"
case "$ERR" in
  *"[h-stats] FALLBACK:"*) ok "(h6) fallback to the single-port path LOGS (no silent regression)" ;;
  *) fail "(h6) fallback logs" "no '[h-stats] FALLBACK:' on stderr; got: ${ERR:-<empty>}" ;;
esac
case "$ERR" in
  *"UNDER-REPORT"*) ok "(h7) the fallback log names the consequence (under-reporting)" ;;
  *) fail "(h7) fallback log names the consequence" "got: ${ERR:-<empty>}" ;;
esac
eq "(h8) the fallback still emits a VALID zero object, not an empty \$stats" \
   "$(obj 0 0 0 0 0)" "$(printf '%s' "$OUT" | sed -n 2p)"

# (h9) A healthy multi-GPU poll must NOT log the fallback — otherwise the signal
# is noise and the next operator learns to ignore it.
OUT="$(e2e "3380 3381" "3380=1000:0:0:0:100
3381=2000:0:0:0:100" NVSMI_GPUS=2)"
case "$(e2e_err)" in
  *FALLBACK*) fail "(h9) healthy rig is quiet" "the fallback logged on a healthy 2-GPU poll: $(e2e_err)" ;;
  *) ok "(h9) healthy multi-GPU poll logs no fallback (the signal stays meaningful)" ;;
esac

# (h10) The probe must not be able to consume the whole budget — that starvation
# is the exact mechanism that put the under-report back. HS_PROBE_BUDGET is a
# strict slice of HS_BUDGET, so the scrape loop always gets time.
if grep -q 'HS_PROBE_BUDGET' "$HSTATS"; then
  ok "(h10) a probe-only budget exists, so the probe cannot starve the scrape loop"
else
  fail "(h10) probe budget" "h-stats.sh has no HS_PROBE_BUDGET: a slow probe can still eat HS_BUDGET whole and silently re-enter the 1/N under-report"
fi

# ── 4d. NEW-D: what a non-reporting card's temperature must be ───────────────
# A lone card 5 emits temp:[0,0,0,0,0,70]. Omitting shifts 70C onto GPU 0
# (HiveOS indexes temp[] positionally); null changes a third-party contract for
# no gain; 0 reads as "card present, not reporting", which the NEW-C bound makes
# provable — the scan stops at the real device count and h-run.sh only launches
# device d when d < count, so every index below a reporting card IS a real card.
echo
echo "-- NEW-D: a dead card's temperature is 0, and only when some card reports --"

OUT="$(e2e "3385" "3385=6000:10:0:0:100:70" NVSMI_GPUS=6)"
E_STATS="$(printf '%s' "$OUT" | sed -n 2p)"
eq "(i1) lone card 5 WITH a sensor: temp[] stays positional, dead cards are 0" \
   '[0,0,0,0,0,70]' \
   "$(printf '%s' "$E_STATS" | sed -n 's/.*"temp":\(\[[^]]*\]\).*/\1/p')"
eq "(i2) lone card 5: hs[] agrees index-for-index with temp[]" \
   '[0,0,0,0,0,6000]' \
   "$(printf '%s' "$E_STATS" | sed -n 's/.*"hs":\(\[[^]]*\]\).*/\1/p')"
# The non-nvml fleet build reports no temperature at all. It must stay [] rather
# than becoming a row of fabricated 0C readings.
OUT="$(e2e "3385" "3385=6000:10:0:0:100" NVSMI_GPUS=6)"
eq "(i3) no card reports a temperature => temp[] stays EMPTY, no fake 0C row" \
   '[]' \
   "$(printf '%s' "$OUT" | sed -n 2p | sed -n 's/.*"temp":\(\[[^]]*\]\).*/\1/p')"

# ── 4e. NEW-E: the ver allowlist is STRUCTURAL, not per-path ─────────────────
# The old single-port shortcut echoed the miner's JSON VERBATIM, bypassing the
# allowlist in hs_merge entirely. Any local process that bound $PORT could forge
# keys into what the rig reports. The fix is not a second sanitiser to remember:
# the shortcut is GONE, so every path — 1 card, 6 cards, fallback — funnels
# through hs_extract | hs_merge and there is no unsanitised path left to forget.
echo
echo "-- NEW-E: no verbatim passthrough; one sanitised output path --"

# A hostile object on the SINGLE live port. Pre-fix this was echoed verbatim and
# "evil" reached HiveOS; now it is normalised out of existence.
d="$(mktemp -d)"; mkdir -p "$d/bin"
cat > "$d/csd-gpu-miner" <<'EVIL'
#!/usr/bin/env bash
echo '{"algo":"sha256d","ar":[1,0,0,0],"fan":[],"hs":[1000],"hs_units":"khs","temp":[],"uptime":7,"ver":"0.2.4","evil":"pwned"}'
exit 0
EVIL
chmod +x "$d/csd-gpu-miner"; : > "$d/h-manifest.conf"; cp "$HSTATS" "$d/h-stats.sh"
cat > "$d/bin/timeout" <<'TS2'
#!/usr/bin/env bash
secs="$1"; shift
if [ "${1:-}" = "bash" ]; then
  p="${*: -1}"; p="${p##*/}"
  case " ${LIVE_SET:-} " in *" $p "*) exit 0 ;; *) exit 1 ;; esac
fi
exec /usr/bin/timeout "$secs" "$@"
TS2
chmod +x "$d/bin/timeout"
R="$(LIVE_SET="3380" PATH="$d/bin:/usr/bin:/bin" bash -c '
  set -uo pipefail; cd "$1"; source ./h-stats.sh; printf "%s\n" "$stats"' _ "$d" 2>/dev/null)"
rm -rf "$d"
case "$R" in
  *evil*|*pwned*) fail "(j1) single live port cannot inject keys" "forged key survived into \$stats: $R" ;;
  *) ok "(j1) a forged key on the SINGLE live port is stripped (no verbatim passthrough)" ;;
esac
eq "(j2) the legitimate fields survive the normalisation intact" \
   '{"algo":"sha256d","ar":[1,0,0,0],"fan":[],"hs":[1000],"hs_units":"khs","temp":[],"uptime":7,"ver":"0.2.4"}' "$R"

# (j3) STRUCTURAL, not incidental: there must be no path that assigns the raw
# scrape straight to $stats. Every assignment has to pass through hs_merge.
RAW_ASSIGN="$(grep -nE '^[[:space:]]*stats="\$\(hs_scrape[^|]*\)"[[:space:]]*$' "$HSTATS" || true)"
if [ -z "$RAW_ASSIGN" ]; then
  ok "(j3) no raw hs_scrape output is ever assigned to \$stats unsanitised"
else
  fail "(j3) no verbatim passthrough" "an unsanitised scrape reaches \$stats, bypassing the ver allowlist:
$RAW_ASSIGN"
fi

# ── 5. Structural guards ─────────────────────────────────────────────────────
echo
echo "-- structural guards on h-stats.sh --"

# The script is SOURCED by the HiveOS agent: a SHELL-level `exit` on a normal
# path would kill the agent's shell. Only the pre-existing `cd || exit 1` guard
# is allowed. (An `exit` inside an embedded awk program is awk's own and is fine,
# so match only shell-level positions: start of line, or after `||`/`&&`.)
BAD_EXIT="$(grep -nE '^[[:space:]]{0,2}exit[[:space:]]|(\|\||&&)[[:space:]]*exit[[:space:]]' "$HSTATS" \
  | grep -v 'cd "$(dirname' || true)"
if [ -z "$BAD_EXIT" ]; then
  ok "no stray 'exit' on a normal path (the agent SOURCES this file)"
else
  fail "stray exit" "$BAD_EXIT"
fi

# No `set -e`: one dead GPU must never abort the whole report.
if grep -qE '^[[:space:]]*set -e' "$HSTATS"; then
  fail "no set -e" "h-stats.sh introduced 'set -e' — one dead port would abort the whole rig report"
else
  ok "no 'set -e' (a dead/garbage port is skipped, never fatal)"
fi

# Every external call in the scan/scrape path must be timeout-bounded.
if grep -q 'HS_TIMEOUT_BIN' "$HSTATS" && grep -q 'HS_SCRAPE_TIMEOUT' "$HSTATS" && grep -q 'HS_PROBE_TIMEOUT' "$HSTATS"; then
  ok "probe AND scrape are both timeout-bounded"
else
  fail "timeout bounds" "h-stats.sh is missing a probe or scrape timeout"
fi

# The kH/s unit conversion must NOT reappear in shell (the §G7 clamp bug class).
if grep -qE '/[[:space:]]*1000(\.0)?|1e6|1e9|/1000' "$HSTATS"; then
  fail "no unit maths in shell" "h-stats.sh appears to divide by 1000/1e6/1e9 — the §G7 kH/s transform must stay in Rust"
else
  ok "no ÷1000 / ÷1e6 / ÷1e9 in shell (the kH/s transform stays in Rust)"
fi

# The scan must be bounded by a cap AND a wall-clock budget, not just by misses.
if grep -q 'HS_MAX_GPUS' "$HSTATS" && grep -q 'HS_BUDGET' "$HSTATS" && grep -q 'HS_MISS_TOL' "$HSTATS"; then
  ok "scan bounded three ways (cap, consecutive-miss cut-off, wall-clock budget)"
else
  fail "scan bounds" "h-stats.sh is missing one of HS_MAX_GPUS / HS_MISS_TOL / HS_BUDGET"
fi

rm -rf "$SB" "$FNS"

echo
echo "========================================"
echo "  Passed: $PASS  Failed: $FAIL"
echo "========================================"
echo
[ "$FAIL" -gt 0 ] && exit 1
exit 0
