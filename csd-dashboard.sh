#!/usr/bin/env bash
# csd-dashboard.sh — live terminal dashboard for the CSD pool miner.
#
# Licensed under PolyForm Perimeter 1.0.0 (see LICENSE). Part of csd-pool-miner.
#
# A READ-ONLY local viewer: it does nothing but GET the miner's own stats
# endpoint (http://127.0.0.1:<port>/1/summary) once per refresh and draw it.
# It never writes config, never touches the miner binary or the share/submit
# path, and never opens a non-loopback socket. Worst case it prints
# "endpoint unreachable" — it cannot stop, slow, or corrupt mining.
#
# The endpoint exists only when the miner runs with `--stats-port` (HiveOS
# forces 3380). Standalone rigs: add `--stats-port 3380` to your mine command.
#
# Usage:
#   csd-dashboard.sh [--port N] [--refresh N] [--once] [--no-color] [--update] [-h]
# Endpoint precedence: $CSD_STATS_URL > --port > $CSD_STATS_PORT > 3380.
set -u

SELF_NAME="csd-dashboard.sh"
REPO_DL="https://github.com/dangraagu/CSD-Mining-pool-public/releases/latest/download"

PORT=""
REFRESH="${CSD_REFRESH:-2}"
ONCE=0
NO_COLOR_FLAG=0
DO_UPDATE=0

usage() {
  cat <<EOF
csd-dashboard.sh — live CSD pool miner dashboard (read-only viewer)

  --port N        stats port (default: \$CSD_STATS_PORT or 3380)
  --refresh N     seconds between refreshes (default: 2)
  --once          print one frame and exit (good for pipes / cron / HiveOS)
  --no-color      disable ANSI color
  --update        self-update this script from the latest release (fail-closed)
  -h, --help      this help

Endpoint: \$CSD_STATS_URL overrides everything, else http://127.0.0.1:<port>/1/summary
The miner must be running with --stats-port <port> for the endpoint to exist.
EOF
}

while [ $# -gt 0 ]; do
  case "$1" in
    --port) PORT="${2:-}"; shift 2 || { echo "missing value for --port" >&2; exit 2; } ;;
    --refresh) REFRESH="${2:-}"; shift 2 || { echo "missing value for --refresh" >&2; exit 2; } ;;
    --once) ONCE=1; shift ;;
    --no-color) NO_COLOR_FLAG=1; shift ;;
    --update) DO_UPDATE=1; shift ;;
    -h|--help) usage; exit 0 ;;
    *) echo "unknown argument: $1" >&2; usage >&2; exit 2 ;;
  esac
done

# integer-only refresh (busybox sleep takes no fractions)
case "$REFRESH" in (*[!0-9]*|"") REFRESH=2 ;; esac
[ "$REFRESH" -lt 1 ] 2>/dev/null && REFRESH=1

# ---- resolve endpoint URL ---------------------------------------------------
if [ -n "${CSD_STATS_URL:-}" ]; then
  URL="$CSD_STATS_URL"
else
  p="${PORT:-${CSD_STATS_PORT:-3380}}"
  case "$p" in (*[!0-9]*|"") p=3380 ;; esac
  URL="http://127.0.0.1:${p}/1/summary"
fi

# ---- self-update (fail-closed, best-effort) ---------------------------------
self_update() {
  local self dir tmp want got verifier
  self="$0"; case "$self" in (/*) : ;; (*) self="$(pwd)/$self" ;; esac
  dir="$(dirname "$self")"; tmp="$dir/.$SELF_NAME.new.$$"
  for verifier in sha256sum "shasum -a 256"; do
    command -v "${verifier%% *}" >/dev/null 2>&1 && break || verifier=""
  done
  [ -z "$verifier" ] && { echo "no sha256 verifier; refusing to self-update" >&2; return 1; }
  fetch "$REPO_DL/SHA256SUMS" > "$tmp.sums" 2>/dev/null || { echo "SHA256SUMS fetch failed" >&2; rm -f "$tmp.sums"; return 1; }
  want="$(awk -v f="$SELF_NAME" '$2==f || $2=="*"f {print $1}' "$tmp.sums" | head -n1)"
  rm -f "$tmp.sums"
  [ -z "$want" ] && { echo "$SELF_NAME not in SHA256SUMS; refusing" >&2; return 1; }
  fetch "$REPO_DL/$SELF_NAME" > "$tmp" 2>/dev/null || { echo "download failed" >&2; rm -f "$tmp"; return 1; }
  got="$($verifier "$tmp" | awk '{print $1}')"
  if [ "$got" != "$want" ]; then echo "checksum mismatch; refusing (kept current)" >&2; rm -f "$tmp"; return 1; fi
  chmod +x "$tmp" 2>/dev/null
  cp "$self" "$self.bak" 2>/dev/null
  mv "$tmp" "$self" && { echo "updated $SELF_NAME (prior at $SELF_NAME.bak)"; return 0; }
  rm -f "$tmp"; echo "install failed; kept current" >&2; return 1
}

# ---- one HTTP GET (curl, fall back to wget) ---------------------------------
fetch() {
  if command -v curl >/dev/null 2>&1; then
    curl -fsS --max-time 4 "$1"
  elif command -v wget >/dev/null 2>&1; then
    wget -qO- --timeout=4 "$1"
  else
    return 99
  fi
}

if [ "$DO_UPDATE" = 1 ]; then self_update; exit $?; fi

# ---- color resolution (once) ------------------------------------------------
C_R=""; C_G=""; C_Y=""; C_C=""; C_D=""; C_B=""; C_0=""
color_off=0
[ "$NO_COLOR_FLAG" = 1 ] && color_off=1
[ -n "${NO_COLOR:-}" ] && color_off=1
[ ! -t 1 ] && [ "$ONCE" = 1 ] && color_off=1
[ "${TERM:-dumb}" = dumb ] && color_off=1
if [ "$color_off" = 0 ]; then
  C_R='\033[31m'; C_G='\033[32m'; C_Y='\033[33m'; C_C='\033[36m'; C_D='\033[90m'; C_B='\033[1m'; C_0='\033[0m'
fi

# ---- helpers ----------------------------------------------------------------
# humanize H/s -> "1.21 GH/s" (busybox-awk safe; handles sci notation)
hr() {
  awk -v v="${1:-0}" 'BEGIN{
    v=v+0; u="H/s";
    if (v>=1e12){v/=1e12;u="TH/s"} else if (v>=1e9){v/=1e9;u="GH/s"}
    else if (v>=1e6){v/=1e6;u="MH/s"} else if (v>=1e3){v/=1e3;u="kH/s"}
    printf "%.2f %s", v, u
  }'
}
# seconds -> "3h 12m 40s"
uptime_fmt() {
  awk -v s="${1:-0}" 'BEGIN{ s=int(s+0); h=int(s/3600); m=int((s%3600)/60); x=s%60;
    if(h>0) printf "%dh %dm %ds",h,m,x; else if(m>0) printf "%dm %ds",m,x; else printf "%ds",x }'
}
# mid-truncate a long address -> "0xA1b2…9f3c"
trunc_addr() {
  awk -v a="${1:-}" 'BEGIN{ if(length(a)>14) printf "%s…%s", substr(a,1,6), substr(a,length(a)-3); else printf "%s", a }'
}
# pure-sed scalar extractors (used when jq is absent). Tolerant of optional
# whitespace after the colon (serde pretty uses ": ", compact uses ":") and
# assume the input has already had newlines stripped (see no-jq branch).
js_str()  { printf '%s' "$1" | sed -n "s/.*\"$2\"[ ]*:[ ]*\"\([^\"]*\)\".*/\1/p" | head -n1; }
js_num()  { printf '%s' "$1" | sed -n "s/.*\"$2\"[ ]*:[ ]*\([0-9][0-9.eE+-]*\).*/\1/p" | head -n1; }
# health is a nested object; pull a number only from inside "health":{...}
js_health() { printf '%s' "$1" | sed -n 's/.*"health"[ ]*:[ ]*{\([^}]*\)}.*/\1/p' | sed -n "s/.*\"$2\"[ ]*:[ ]*\([0-9][0-9.eE+-]*\).*/\1/p" | head -n1; }

HAS_JQ=0; command -v jq >/dev/null 2>&1 && HAS_JQ=1

# previous-sample state for shares/min rate
PREV_GOOD=""; PREV_TS=""

# ---- terminal setup / teardown ----------------------------------------------
restore() { [ "$ONCE" = 1 ] || { [ -t 1 ] && printf '\033[?25h\033[0m\n'; }; }
trap 'restore; exit 0' INT TERM
trap 'restore' EXIT
if [ "$ONCE" = 0 ] && [ -t 1 ]; then printf '\033[2J\033[H\033[?25l'; fi

# ---- render one frame -------------------------------------------------------
render() {
  local body rc ver worker up h0 h1 h2 good total rej stale pool recon fail temp pwr _tty
  body="$(fetch "$URL")"; rc=$?
  _tty=0; [ "$ONCE" = 0 ] && [ -t 1 ] && _tty=1
  [ "$_tty" = 1 ] && printf '\033[H'

  if [ $rc -ne 0 ] || [ -z "$body" ]; then
    # endpoint down / unreachable — remediation banner, never blank
    printf "${C_B}${C_C}  CSD Pool Miner${C_0}\n\n"
    printf "  ${C_R}● stats endpoint unreachable${C_0}\n"
    printf "  ${C_D}%s${C_0}\n\n" "$URL"
    printf "  Is the miner running with ${C_B}--stats-port${C_0} ?\n"
    printf "  HiveOS sets 3380 automatically. Standalone: add ${C_B}--stats-port 3380${C_0}\n"
    printf "  to your mine command, or set ${C_B}CSD_STATS_PORT${C_0} / pass ${C_B}--port N${C_0}.\n"
    [ "$ONCE" = 0 ] && printf "\n  ${C_D}retrying every %ss · q / Ctrl-C to quit${C_0}\n" "$REFRESH"
    [ "$_tty" = 1 ] && printf '\033[J'
    return
  fi

  if [ "$HAS_JQ" = 1 ]; then
    # one jq pass, fields joined on '|'. A non-whitespace separator is REQUIRED:
    # with a tab/whitespace IFS, `read` collapses empty fields, so a missing
    # worker_id/health would shift every later field left (pool lands in stale).
    # '|' is safe: no field value (host:port, hex addr, ints, floats) contains it.
    IFS='|' read -r ver worker up h0 h1 h2 good total rej stale pool recon fail temp pwr <<EOF
$(printf '%s' "$body" | jq -r '[(.version//"?"),(.worker_id//""),((.uptime//0)|tostring),((.hashrate.total[0]//0)|tostring),((.hashrate.total[1]//0)|tostring),((.hashrate.total[2]//0)|tostring),((.results.shares_good//0)|tostring),((.results.shares_total//0)|tostring),((.results.shares_rejected//0)|tostring),((.results.shares_stale//0)|tostring),(.connection.pool//"n/a"),((.connection.reconnects//0)|tostring),((.connection.failovers//0)|tostring),(.health.gpu_temp_c//""|tostring),(.health.gpu_power_w//""|tostring)]|join("|")' 2>/dev/null)
EOF
  else
    # collapse to one line so the sed extractors work on pretty OR compact JSON
    local b1; b1="$(printf '%s' "$body" | tr -d '\n\r')"
    ver="$(js_str "$b1" version)"
    worker="$(js_str "$b1" worker_id)"
    # "uptime" appears twice (top-level miner uptime + connection.uptime). The
    # generic greedy extractor would grab the LAST one (connection); strip the
    # connection object first so we read the miner uptime.
    up="$(printf '%s' "$b1" | sed 's/"connection"[ ]*:[ ]*{[^}]*}//' | sed -n 's/.*"uptime"[ ]*:[ ]*\([0-9][0-9.eE+-]*\).*/\1/p' | head -n1)"
    # hashrate.total array -> three values
    local arr; arr="$(printf '%s' "$b1" | sed -n 's/.*"total"[ ]*:[ ]*\[\([^]]*\)\].*/\1/p')"
    h0="$(printf '%s' "$arr" | awk -F, '{print $1+0}')"
    h1="$(printf '%s' "$arr" | awk -F, '{print $2+0}')"
    h2="$(printf '%s' "$arr" | awk -F, '{print $3+0}')"
    good="$(js_num "$b1" shares_good)"; total="$(js_num "$b1" shares_total)"
    rej="$(js_num "$b1" shares_rejected)"; stale="$(js_num "$b1" shares_stale)"
    pool="$(js_str "$b1" pool)"
    recon="$(js_num "$b1" reconnects)"; fail="$(js_num "$b1" failovers)"
    temp="$(js_health "$b1" gpu_temp_c)"; pwr="$(js_health "$b1" gpu_power_w)"
  fi

  : "${ver:=?}"; : "${worker:=}"; : "${up:=0}"
  : "${h0:=0}"; : "${h1:=0}"; : "${h2:=0}"
  : "${good:=0}"; : "${total:=0}"; : "${rej:=0}"; : "${stale:=0}"
  : "${pool:=n/a}"; : "${recon:=0}"; : "${fail:=0}"

  # computed: reject% and a per-minute accepted rate (needs a 2nd sample)
  local rejpct rate now
  rejpct="$(awk -v r="$rej" -v s="$stale" -v t="$total" 'BEGIN{ t=t+0; if(t<=0){print "0.00"} else printf "%.2f",(r+s)*100/t }')"
  now="$(date +%s 2>/dev/null || echo 0)"
  if [ -n "$PREV_GOOD" ] && [ -n "$PREV_TS" ] && [ "$now" -gt "$PREV_TS" ] 2>/dev/null; then
    rate="$(awk -v g="$good" -v pg="$PREV_GOOD" -v dt="$((now-PREV_TS))" 'BEGIN{ d=g-pg; if(d<0)d=0; printf "%.1f", d*60/dt }')/min"
  else
    rate="—"
  fi
  PREV_GOOD="$good"; PREV_TS="$now"

  # colors by health. Explicit if/elif — a chained `awk && red || awk && yellow`
  # mis-fires (the trailing && also runs after the red branch), painting a
  # critical >5% reject rate yellow instead of red.
  local rej_col="$C_G"
  if awk "BEGIN{exit !($rejpct>5)}"; then rej_col="$C_R"
  elif awk "BEGIN{exit !($rejpct>1)}"; then rej_col="$C_Y"
  fi
  [ "${stale:-0}" != "0" ] && [ "$rej_col" = "$C_G" ] && rej_col="$C_Y"

  local temps powers
  if [ -n "${temp:-}" ]; then temps="$(awk -v t="$temp" 'BEGIN{printf "%.0f °C",t+0}')"; else temps="n/a"; fi
  if [ -n "${pwr:-}" ];  then powers="$(awk -v p="$pwr" 'BEGIN{printf "%.0f W",p+0}')"; else powers="n/a"; fi

  printf "${C_B}${C_C}  CSD Pool Miner${C_0}${C_D}                                  v%s${C_0}\n" "$ver"
  printf "  ${C_D}Worker${C_0}  %-18s ${C_D}Uptime${C_0}  %s\n" "$(trunc_addr "$worker")" "$(uptime_fmt "$up")"
  printf "  ${C_D}Pool${C_0}    %-26s ${C_G}● UP${C_0}\n" "$pool"
  printf "  ${C_D}----------------------------------------------------${C_0}\n"
  printf "  ${C_B}HASHRATE${C_0}   ${C_D}10s${C_0} %-12s ${C_D}1m${C_0} %-12s ${C_D}15m${C_0} %s\n" "$(hr "$h0")" "$(hr "$h1")" "$(hr "$h2")"
  printf "  ${C_D}----------------------------------------------------${C_0}\n"
  printf "  ${C_B}SHARES${C_0}     ${C_G}acc${C_0} %-8s ${rej_col}rej${C_0} %-6s ${C_Y}stale${C_0} %-6s\n" "$good" "$rej" "$stale"
  printf "             ${C_D}total${C_0} %-7s ${C_D}reject%%${C_0} %-7s ${C_D}rate${C_0} %s\n" "$total" "${rejpct}%" "$rate"
  printf "  ${C_D}----------------------------------------------------${C_0}\n"
  printf "  ${C_B}GPU${C_0}        ${C_D}temp${C_0} %-9s ${C_D}power${C_0} %s\n" "$temps" "$powers"
  printf "  ${C_B}LINK${C_0}       ${C_D}reconnects${C_0} %-5s ${C_D}failovers${C_0} %s\n" "$recon" "$fail"
  printf "  ${C_D}----------------------------------------------------${C_0}\n"
  printf "  ${C_B}EARNINGS${C_0}   ${C_D}——  (set CSD_POOL_API to enable)${C_0}\n"
  [ "$ONCE" = 0 ] && printf "  ${C_D}refresh %ss · q / Ctrl-C quit${C_0}\n" "$REFRESH"
  [ "$_tty" = 1 ] && printf '\033[J'
}

# ---- main loop --------------------------------------------------------------
if [ "$ONCE" = 1 ]; then
  render
  exit 0
fi

while :; do
  render
  # read doubles as the delay AND a 'q' quit listener on a TTY; else plain sleep
  if [ -t 0 ]; then
    if read -t "$REFRESH" -n1 key 2>/dev/null; then
      case "$key" in (q|Q) break ;; esac
    fi
  else
    sleep "$REFRESH"
  fi
done
