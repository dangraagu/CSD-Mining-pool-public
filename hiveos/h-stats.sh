#!/usr/bin/env bash
# HiveOS custom-miner stats reporter for csd-pool-miner (P4 §1).
#
# HiveOS sources this every ~10s and reads two shell variables afterwards:
#   $khs    total hashrate in kH/s (a number)
#   $stats  a JSON object: { hs:[...], hs_units, ar:[...], uptime, algo, ... }
#
# The UNIT transform (scrape /1/summary, convert H/s -> kH/s with the correct
# ÷1000 divisor, shape the JSON) lives in the miner's OWN `hiveos-stats`
# subcommand — pure, unit-tested Rust (src/hiveos.rs). That is deliberate: the
# §G7 kH/s-clamp bug ("every card shows 1000 GH") is exactly the kind of error
# that silently reappears in shell jq/awk, so this script does NO unit maths.
#
# ── MULTI-GPU (the bug this file fixes) ───────────────────────────────────────
# The binary is one-process-one-device, so h-run.sh launches device 0 on $PORT
# and each extra card on PORT+1, PORT+2, … (hive_launch_extra_gpus). This script
# used to scrape ONLY $PORT — i.e. device 0 — so a 6-GPU rig reported ~1/6 of its
# real hashrate. That is NOT merely cosmetic: HiveOS's own hashrate watchdog
# compares the reported rate against a threshold, so a healthy multi-GPU rig
# could be killed and restarted in a loop. It is an UPTIME bug.
#
# The fix: probe PORT, PORT+1, … , scrape every LIVE one, and merge the results
# into a single h-stats object with one hs[] element PER GPU (HiveOS indexes
# hs[]/temp[] positionally per card). The merge is pure AGGREGATION of values the
# Rust subcommand already converted — concatenating kH/s integers and summing
# share counters. It never re-derives a unit, so the §G7 hazard the "no maths in
# shell" rule protects against is untouched.
#
# ── Liveness must be probed separately ───────────────────────────────────────
# `hiveos-stats` ALWAYS exits 0 and ALWAYS prints a valid object — on a dead port
# it prints the zero object (src/main.rs:1099-1112). So its output can NOT tell a
# dead port from a live-but-idle card. We therefore TCP-probe each port first
# (bash /dev/tcp, no external dependency) and only scrape the ones that answer.
#
# ── Fail-soft, always ────────────────────────────────────────────────────────
# Every step tolerates failure: a dead / unreachable / garbage-emitting port is
# skipped and the scan continues, so a partial rig still reports. The scan is
# bounded four ways (per-probe timeout, a probe-only wall-clock budget, the
# device-count cap, and the total wall-clock budget) so a wedged process can
# never hang the hook.
#
# ── The scan bound is the DEVICE COUNT, not a blind 32 (NEW-C) ───────────────
# Scanning a fixed 32 ports was a REGRESSION INTO THE ORIGINAL BUG.
#
# Two probe designs were measured, in BOTH regimes that matter (WSL2, real
# sockets, real /dev/tcp — see the timings recorded in the v0.2.4 notes):
#
#   A. CLOSED port (nothing ever bound). The kernel answers RST immediately, so
#      only process cost remains:
#        32 closed, `timeout`+`bash -c` probe .... 111 ms  (~3.5 ms per probe)
#        32 closed, bare /dev/tcp subshell ......  12 ms  (~0.4 ms per probe)
#      Here the fork IS the cost, and dropping it looks ~9x cheaper.
#
#   B. WEDGED port — LISTENING but never accept()ing, so the backlog fills and
#      further SYNs are dropped. This is what a hung miner process actually
#      looks like, and it is the regime that produced the operator's 7449 ms:
#        one wedged port, `timeout`-bounded probe .... 1002 ms
#        one wedged port, bare /dev/tcp subshell ..... NEVER RETURNED
#                                                     (killed at 120 s)
#
# So the "cheaper probe" (option a) is REJECTED. It wins ~3 ms per poll in the
# regime where nothing is wrong, and hangs the HiveOS hook FOREVER in the exact
# regime this file exists to survive. The `timeout` fork STAYS; it is the only
# thing that turns an unbounded stall into a bounded 1 s.
#
# What actually fixes the cost is not probing faster, it is not probing ports
# that CANNOT be live (option b). In regime B the old scan was 32 x 1 s = 32 s
# against a ~10 s HiveOS poll; on a 6-GPU rig it is now 6 probes, and
# HS_PROBE_BUDGET caps even that. Measured end-to-end (h-stats.sh sourced, real
# listeners, 32-port bound vs device-count bound):
#
#        all dead   1 live   6 live   6 live+gap   8 live
#   old:   279 ms   111 ms   132 ms     168 ms     138 ms
#   new:    25 ms    22 ms    45 ms      46 ms      55 ms
#
# And the bound is COMPLETE, not a guess: h-run.sh (hive_multi_gpu_plan, :256)
# launches device d only when `d < gpu_count`, and puts it on PORT+d. So every
# live stats port is in [PORT, PORT + gpu_count - 1] — ALWAYS, including the
# sparse `--gpu-id 0,8` case, which h-run.sh only honours on a >=9-GPU box.
# Bounding the scan by the real device count therefore cannot miss a card.
# If the count is unavailable (cpu variant, container, no nvidia-smi/clinfo) we
# keep the old blind 32 — degraded, but never narrower than before.
#
# Why that matters: at 32 ports the probe consumed ~93% of HS_BUDGET against a
# ~10s HiveOS poll. When the probe eats the budget, the scrape loop breaks on
# iteration 1, hs_merge gets nothing, and we fall back to scraping $PORT alone —
# silently re-entering the ~1/N under-report this file exists to fix. HS_BUDGET
# is now split so the probe can never starve the scrape (HS_PROBE_BUDGET), and
# the fallback LOGS (hs_log) so the regression can never again be silent.
#
# ── ONE output path (NEW-E) ──────────────────────────────────────────────────
# There is no longer a "single port => echo the miner's JSON verbatim" shortcut.
# EVERY path — one card, six cards, or the all-dead fallback — goes through
# hs_extract | hs_merge, so the `ver` allowlist in hs_merge is STRUCTURAL: there
# is no second path for a future editor to forget to sanitise. A legitimate
# object round-trips byte-identically (hs_merge emits the same key order and
# spelling serde_json does), so a 1-GPU rig's output is unchanged.

cd "$(dirname "$0")" || exit 1
# shellcheck source=/dev/null
[ -e h-manifest.conf ] && . ./h-manifest.conf

PORT="${CUSTOM_API_PORT:-3380}"
BIN="${CUSTOM_BIN:-$(dirname "$0")/csd-gpu-miner}"   # install-path-agnostic fallback (we already cd'd here)

HS_PROBE_TIMEOUT="${CSD_HSTATS_PROBE_TIMEOUT:-1}"   # seconds, per TCP probe
HS_SCRAPE_TIMEOUT="${CSD_HSTATS_SCRAPE_TIMEOUT:-3}" # seconds, per hiveos-stats call
HS_BUDGET="${CSD_HSTATS_BUDGET:-8}"             # seconds, PROBE + SCRAPE (HiveOS polls every ~10s)

# `timeout` is present on HiveOS (h-run.sh relies on it), but degrade gracefully.
HS_TIMEOUT_BIN="$(command -v timeout 2>/dev/null || true)"

# ── Logging ───────────────────────────────────────────────────────────────────
# This file is SOURCED by the HiveOS agent, so stdout belongs to the agent — we
# log to stderr (which lands in the agent's log) and best-effort append to $LOG
# when h-run.sh's log path happens to be exported. Always returns 0: a logging
# failure must never change what the rig reports.
hs_log() {
  printf '[h-stats] %s\n' "$*" >&2 2>/dev/null
  if [ -n "${LOG:-}" ]; then printf '[h-stats] %s\n' "$*" >> "$LOG" 2>/dev/null; fi
  return 0
}

# ── Scan bound = the real device count (see the NEW-C note in the header) ────
# Mirrors h-run.sh:hive_gpu_count so the two agree on how many cards exist, and
# therefore on the port range h-run.sh can possibly have used. Timeout-guarded
# and `|| true` throughout: a wedged or absent tool yields 0, never a hang.
# Echoes a non-negative integer; 0 means "unknown".
hs_gpu_count() {
  _hs_n=0
  if command -v nvidia-smi >/dev/null 2>&1; then
    if [ -n "$HS_TIMEOUT_BIN" ]; then
      _hs_n="$("$HS_TIMEOUT_BIN" 5 nvidia-smi -L 2>/dev/null | grep -c '^GPU ' || true)"
    else
      _hs_n="$(nvidia-smi -L 2>/dev/null | grep -c '^GPU ' || true)"
    fi
  fi
  if [ "${_hs_n:-0}" -lt 1 ] 2>/dev/null && command -v clinfo >/dev/null 2>&1; then
    if [ -n "$HS_TIMEOUT_BIN" ]; then
      _hs_n="$("$HS_TIMEOUT_BIN" 5 clinfo 2>/dev/null | grep -c 'Device Type.*GPU' || true)"
    else
      _hs_n="$(clinfo 2>/dev/null | grep -c 'Device Type.*GPU' || true)"
    fi
  fi
  case "$_hs_n" in ''|*[!0-9]*) _hs_n=0 ;; esac
  printf '%s' "$_hs_n"
}

# An explicit CSD_HSTATS_MAX_GPUS always wins (tests, and the escape hatch for a
# rig whose card count we cannot detect). Otherwise use the detected count; if
# detection fails (cpu variant, no nvidia-smi/clinfo, container) keep the old
# blind 32 — degraded but never WORSE than before, and the probe budget below
# stops even that from starving the scrape.
if [ -n "${CSD_HSTATS_MAX_GPUS:-}" ]; then
  HS_MAX_GPUS="$CSD_HSTATS_MAX_GPUS"
else
  HS_DETECTED_GPUS="$(hs_gpu_count 2>/dev/null)"
  if [ "${HS_DETECTED_GPUS:-0}" -ge 1 ] 2>/dev/null; then
    HS_MAX_GPUS="$HS_DETECTED_GPUS"
  else
    HS_MAX_GPUS=32
  fi
fi

# Consecutive-dead-port cut-off. Defaults to HS_MAX_GPUS = effectively OFF: a
# sparse `--gpu-id 0,8` include-list leaves SEVEN dead ports between the two live
# cards, so any small tolerance silently drops card 8 — the exact under-reporting
# this file exists to fix. Now that HS_MAX_GPUS is the DEVICE COUNT rather than a
# blind 32, the cap alone bounds the scan tightly; this is kept only as an escape
# hatch for a rig that wants the scan cut short.
HS_MISS_TOL="${CSD_HSTATS_MISS_TOL:-$HS_MAX_GPUS}"

# Probe-only slice of HS_BUDGET. The probe MUST NOT be able to consume the whole
# budget: if it does, the scrape loop breaks on its first iteration, hs_merge
# gets no records, and we drop into the single-port fallback = the ~1/N
# under-report. Half the budget, floor 1s.
HS_PROBE_BUDGET="${CSD_HSTATS_PROBE_BUDGET:-$(( HS_BUDGET / 2 ))}"
[ "$HS_PROBE_BUDGET" -ge 1 ] 2>/dev/null || HS_PROBE_BUDGET=1

# ── I/O helpers ───────────────────────────────────────────────────────────────

# Is 127.0.0.1:$1 accepting connections? 0 = yes. Uses the bash /dev/tcp
# redirection (a builtin — no nc/ss/lsof dependency), wrapped in `timeout` so a
# blackholed port cannot stall the hook. Tests override this function.
hs_port_live() {
  if [ -n "$HS_TIMEOUT_BIN" ]; then
    # The port is passed as an ARGUMENT, not concatenated into the command
    # string. Arithmetic expansion already launders it today, but a
    # string-concatenated `bash -c` is one refactor away from being an eval sink
    # in a hook that runs as root.
    # shellcheck disable=SC2016  # deliberate: $1 is the INNER bash's argument,
    # expanded by the child, not interpolated into the command string by us.
    "$HS_TIMEOUT_BIN" "$HS_PROBE_TIMEOUT" \
      bash -c 'exec 3<>/dev/tcp/127.0.0.1/$1' _ "$1" >/dev/null 2>&1
  else
    ( exec 3<>"/dev/tcp/127.0.0.1/$1" ) >/dev/null 2>&1
  fi
}

# Scrape one port's h-stats object. Echoes the JSON, or nothing on failure.
hs_scrape() {
  if [ -n "$HS_TIMEOUT_BIN" ]; then
    "$HS_TIMEOUT_BIN" "$HS_SCRAPE_TIMEOUT" "$BIN" hiveos-stats --stats-port "$1" 2>/dev/null
  else
    "$BIN" hiveos-stats --stats-port "$1" 2>/dev/null
  fi
}

# Echo the live stats ports, one per line, starting at $PORT. Bounded by
# HS_MAX_GPUS, by HS_MISS_TOL consecutive misses (so a `--gpu-id 0,2` gap or one
# dead card does not truncate the scan), and by the HS_BUDGET wall clock.
hs_live_ports() {
  # The budget clock is SHARED with the scrape loop below (the caller starts it),
  # so HS_BUDGET bounds probe+scrape together as documented, not the probe alone.
  # Falls back to starting it here when called standalone (unit tests).
  _hs_t0="${_hs_t0:-$SECONDS}"
  # The probe gets its OWN slice so it cannot starve the scrape loop (NEW-C).
  # Standalone (unit tests) it degrades to the full budget = the old behaviour.
  _hs_pbud="${HS_PROBE_BUDGET:-$HS_BUDGET}"
  _hs_i=0
  _hs_miss=0
  while [ "$_hs_i" -lt "$HS_MAX_GPUS" ]; do
    if [ $((SECONDS - _hs_t0)) -ge "$_hs_pbud" ]; then
      hs_log "probe budget (${_hs_pbud}s) exhausted after ${_hs_i} port(s); scan truncated — cards above index $((_hs_i - 1)) will not be reported this poll."
      break
    fi
    [ $((SECONDS - _hs_t0)) -lt "$HS_BUDGET" ] || break
    _hs_p=$((PORT + _hs_i))
    if hs_port_live "$_hs_p"; then
      printf '%s\n' "$_hs_p"
      _hs_miss=0
    else
      _hs_miss=$((_hs_miss + 1))
      [ "$_hs_miss" -lt "$HS_MISS_TOL" ] || break
    fi
    _hs_i=$((_hs_i + 1))
  done
}

# ── PURE transforms (no I/O — unit-tested directly by tests/hiveos_stats_multigpu.sh) ──

# stdin: ONE h-stats JSON object. stdout: a flat tab-separated record
#   khs <TAB> temp <TAB> uptime <TAB> good <TAB> rejected <TAB> invalid <TAB> stale <TAB> ver
# `temp` is EMPTY when the object carries no temperature (the non-nvml fleet
# build). Malformed input yields an empty record, which hs_merge then ignores.
# Prefers jq; falls back to a hand-rolled awk extractor when jq is absent (it is
# not installed on a stock rig). Both paths emit the SAME record format, so the
# merge below has exactly one implementation regardless of which ran.
hs_extract() {
  if command -v jq >/dev/null 2>&1; then
    # The `select` is NOT decoration: without it `.hs[0]//0` yields 0 for ANY
    # valid JSON object, so a foreign service that happened to bind a stats port
    # would be extracted as a real card reporting 0 kH/s. The awk fallback below
    # already rejects an object with no (or empty) hs[] — this keeps the two
    # implementations byte-identical on MALFORMED input too, not just on good
    # input. A no-hs[] object means "not our object", on both paths.
    jq -r 'select((.hs|type) == "array" and (.hs|length) > 0)
           | [ (.hs[0]//0), (.temp[0]//""), (.uptime//0),
               (.ar[0]//0), (.ar[1]//0), (.ar[2]//0), (.ar[3]//0), (.ver//"") ] | @tsv' \
      2>/dev/null
  else
    awk '
      # Everything after the literal "key": in the object, or "" if absent.
      function afterkey(s, key,   re, i) {
        re = "\"" key "\":"
        i = index(s, re)
        if (i == 0) return ""
        return substr(s, i + length(re))
      }
      # The text BETWEEN the brackets of an array field ("" if absent/empty).
      function arrbody(s, key,   r, e) {
        r = afterkey(s, key)
        if (r == "" || substr(r, 1, 1) != "[") return ""
        r = substr(r, 2)
        e = index(r, "]")
        if (e == 0) return ""
        return substr(r, 1, e - 1)
      }
      # A scalar number field ("" if absent).
      function num(s, key,   r, e) {
        r = afterkey(s, key)
        if (r == "") return ""
        e = match(r, /[,}]/)
        if (e > 0) r = substr(r, 1, e - 1)
        gsub(/[ \t"]/, "", r)
        return r
      }
      # A quoted string field ("" if absent).
      function str(s, key,   r, e) {
        r = afterkey(s, key)
        if (r == "" || substr(r, 1, 1) != "\"") return ""
        r = substr(r, 2)
        e = index(r, "\"")
        if (e == 0) return ""
        return substr(r, 1, e - 1)
      }
      {
        hb = arrbody($0, "hs")
        if (hb == "") next                  # no hs[] at all => not our object
        split(hb, H, ",")
        tb = arrbody($0, "temp")
        if (tb == "") { t = "" } else { split(tb, T, ","); t = T[1] + 0 }
        split(arrbody($0, "ar"), A, ",")
        printf "%d\t%s\t%d\t%d\t%d\t%d\t%d\t%s\n", \
          H[1] + 0, t, num($0, "uptime") + 0, \
          A[1] + 0, A[2] + 0, A[3] + 0, A[4] + 0, str($0, "ver")
      }
    ' 2>/dev/null
  fi
}

# stdin: one flat record per live GPU (from hs_extract). stdout: ONE merged
# h-stats object, keys in the same alphabetical order serde_json emits.
#   hs[]    concatenated — one element PER GPU, so HiveOS shows per-card rates.
#           POSITIONAL: the caller feeds one record per DEVICE INDEX (padding
#           gaps with an all-zero record), so element i is always device i.
#   temp[]  concatenated and padded with 0 for cards that report none; stays
#           EMPTY when no card reports any (the non-nvml fleet build's shape)
#   fan[]   always empty — the miner has no fan telemetry; we never fabricate it
#   ar[]    element-wise SUM (share counters are per-process; the rig wants totals)
#   uptime  MAX across processes (device 0 may have restarted under the others)
#   ver     the first non-empty (every process is the same binary)
# Exits 1 without printing when there are no usable records, so the caller can
# fall back rather than emit nonsense.
hs_merge() {
  awk -F'\t' '
    NF >= 8 {
      n++
      hs[n] = $1 + 0
      if ($2 == "") { tv[n] = 0 } else { tv[n] = $2 + 0; anytemp = 1 }
      if ($3 + 0 > maxup) maxup = $3 + 0
      g += $4; r += $5; iv += $6; st += $7
      # ver is the ONE free-text field that reaches printf "%s" below, making it
      # the only injection sink in this object: any local process that binds an
      # unused port in range could otherwise forge JSON keys into what the rig
      # reports. Allowlist it, the same idiom h-config.sh uses for WNAME. Applied
      # here so it covers BOTH extractors (jq and the awk fallback).
      # NB: no apostrophes in this awk program, it is single-quoted shell.
      if (ver == "" && $8 != "") { _v = $8; gsub(/[^A-Za-z0-9._+-]/, "", _v); ver = _v }
    }
    END {
      if (n == 0) exit 1
      for (i = 1; i <= n; i++) hsl = hsl (i > 1 ? "," : "") hs[i]
      if (anytemp) for (i = 1; i <= n; i++) tpl = tpl (i > 1 ? "," : "") tv[i]
      printf "{\"algo\":\"sha256d\",\"ar\":[%d,%d,%d,%d],\"fan\":[],\"hs\":[%s],\"hs_units\":\"khs\",\"temp\":[%s],\"uptime\":%d,\"ver\":\"%s\"}\n", \
        g, r, iv, st, hsl, tpl, maxup, ver
    }
  '
}

# ── Collect ───────────────────────────────────────────────────────────────────
# Probe the ports, scrape the live ones, merge. Any failure anywhere leaves
# $stats empty and drops us into the single-port fallback below.
#
# ── POSITIONAL ALIGNMENT (the trap this block exists to avoid) ────────────────
# h-run.sh (hive_multi_gpu_plan, :257) assigns `port = PORT + device`. The device
# index is therefore RECOVERABLE as (port - PORT), and it is the ONLY legitimate
# way to place a card in hs[]. Appending in scan order instead would put card 2's
# rate at index 1 on a `--gpu-id 0,2` rig, and shift every later card left the
# moment one dies mid-run. HiveOS indexes hs[]/temp[] POSITIONALLY per card, so
# that misassigns the rates on screen while the rig TOTAL stays correct — it
# looks healthy and reads wrong, which is worse than an obvious under-report.
#
# So: emit one record per device index from 0 up to the highest index that
# actually produced a record, padding every gap (dead card, garbage, wedged
# scrape) with an all-zero record. A zero pad contributes 0 to hs[], 0 to temp[]
# (without itself claiming a temperature), and nothing to the share totals.
#
# ── Why a pad's temperature is 0 and not omitted/null (NEW-D) ────────────────
# A lone surviving card 5 emits temp:[0,0,0,0,0,70]. Three options were on the
# table; 0 is the only correct one:
#   omit  — shortening temp[] shifts card 5's 70C onto GPU 0. HiveOS indexes
#           temp[] POSITIONALLY, so this is the misalignment the whole indexed
#           path exists to prevent. Rejected outright.
#   null  — HiveOS's stats consumer expects numbers in temp[]; a null is a shape
#           change to a third-party contract we do not control, for zero gain.
#   0     — CHOSEN. It reads as "card present, reporting nothing", which is
#           exactly true. The NEW-C bound makes that a provable claim rather
#           than a hopeful one: the scan now stops at the real device count, and
#           h-run.sh only launches device d when d < gpu_count, so every index
#           below a reporting card is a card that EXISTS and is simply dead or
#           wedged. hs[]=0 on the same index already says "not hashing", so 0C
#           adds no new false claim — and a dead card genuinely has no reading.
# The 0s are only emitted at all when SOME card reports a temperature; on the
# non-nvml fleet build temp[] stays [] rather than becoming a row of fake 0s.
HS_PAD_REC="$(printf '0\t\t0\t0\t0\t0\t0\t')"

_hs_t0=$SECONDS          # budget clock: spans the probe scan AND the scrape loop
stats=""
HS_PORTS="$(hs_live_ports 2>/dev/null)"
HS_NLIVE="$(printf '%s' "$HS_PORTS" | grep -c '[0-9]' 2>/dev/null)"
[ -z "$HS_NLIVE" ] && HS_NLIVE=0

if [ "$HS_NLIVE" -ge 1 ] 2>/dev/null; then
  # One record per DEVICE INDEX, gaps padded. This is the ONLY collect path —
  # a 1-GPU rig included (NEW-E). hs_merge re-emits the object in serde_json's
  # own key order/spelling, so a legitimate single-card object round-trips
  # BYTE-IDENTICALLY while an object forged by a hostile local process that
  # bound a stats port is normalised out of existence.
  # A port that dies between the probe and the scrape, or answers with garbage,
  # yields no record — it becomes a zero pad if a later card reports, and is
  # simply not emitted if it is the last one. Either way no card shifts.
  HS_RECORDS=""
  _hs_next=0
  for _hs_p in $HS_PORTS; do
    # Same wall-clock budget as the probe scan: without this the worst case is
    # HS_BUDGET + HS_MAX_GPUS x HS_SCRAPE_TIMEOUT (~104s) against a ~10s HiveOS
    # poll, which stacks resident invocations and serves permanently stale stats
    # on exactly the wedged-but-listening rig this file targets.
    [ $((SECONDS - _hs_t0)) -lt "$HS_BUDGET" ] || break
    _hs_d=$((_hs_p - PORT))
    _hs_rec="$(hs_scrape "$_hs_p" | hs_extract)"
    [ -z "$_hs_rec" ] && continue
    while [ "$_hs_next" -lt "$_hs_d" ]; do
      HS_RECORDS="$HS_RECORDS$HS_PAD_REC
"
      _hs_next=$((_hs_next + 1))
    done
    HS_RECORDS="$HS_RECORDS$_hs_rec
"
    _hs_next=$((_hs_d + 1))
  done
  stats="$(printf '%s' "$HS_RECORDS" | hs_merge 2>/dev/null)" || stats=""
fi

# Nothing live, or the merge produced nothing: fall back to a single-port scrape
# of $PORT. The subcommand prints a zero-but-valid object when that port is dead,
# so this is exactly the legacy "alive but zero" output.
#
# THIS PATH MUST BE LOUD (NEW-C). On a multi-GPU rig it re-enters the ~1/N
# under-report that this whole file exists to fix — the hashrate HiveOS's own
# watchdog compares against a threshold. A fix that fails back INTO the bug with
# no log line is the single worst outcome here, so every entry is announced.
# It still goes through hs_extract | hs_merge: one sanitised output path, no
# verbatim passthrough (NEW-E).
if [ -z "$stats" ]; then
  hs_log "FALLBACK: multi-port collect produced no records (live_ports=$HS_NLIVE, scan_limit=$HS_MAX_GPUS, budget=${HS_BUDGET}s) — reporting stats port $PORT ALONE. A multi-GPU rig WILL UNDER-REPORT its hashrate until this clears."
  stats="$(hs_scrape "$PORT" | hs_extract | hs_merge 2>/dev/null)" || stats=""
fi

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
