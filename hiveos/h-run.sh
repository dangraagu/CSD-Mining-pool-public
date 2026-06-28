#!/usr/bin/env bash
# HiveOS custom-miner launcher for csd-pool-miner (P4 §1 / SP2).
#
# HiveOS runs this to start mining. It MUST `exec` the real binary so the
# process argv is `csd-gpu-miner …` and NOT `h-run.sh` — HiveOS refuses to start
# a custom miner whose argv contains h-run.sh. `exec` replaces this shell; we
# never background the MINER with `&`.
#
# AUTO-UPDATE (fleet, no clawback — FAIL-SAFE IS RULE #1)
# ────────────────────────────────────────────────────────────────────────────────
# A bricked rig is catastrophic, so EVERY update path falls through to running
# the EXISTING binary on ANY failure (no network, GitHub rate-limit, SHA
# mismatch, partial download, disk full, missing verifier). The update gate is
# the SAME proven three-check logic mine-auto.sh uses:
#   1. numeric semver compare via the binary's own `check-update` (so 0.1.10 >
#      0.1.9 — a string compare gets that wrong), string-`!=` fallback only when
#      no usable binary exists yet. "Latest" is resolved from the
#      releases/latest/download/ CDN asset latest-version.txt (FIX #8), NOT the
#      60-req/hr api.github.com endpoint that silently froze whole farms,
#   2. download to a TEMP path ("$CUSTOM_BIN.new") — NEVER onto the live binary,
#   3. SHA-256 verify the temp against the release SHA256SUMS with a TRUSTED
#      verifier (the already-installed $CUSTOM_BIN's `verify-file`, else OS
#      sha256sum) BEFORE the atomic swap. FAIL CLOSED: a missing SHA256SUMS, an
#      asset not listed in it, a SHA mismatch, or no trusted verifier all REFUSE
#      the update (FIX #9) — the download is discarded and the running binary is
#      kept. The rig never executes an unverified binary, and never bricks.
#
# Because HiveOS forbids a foreground loop in h-run.sh (the exec-rename hazard),
# auto-update is split into TWO layers:
#   (a) a one-shot STARTUP check, run BEFORE the exec, that swaps in a newer
#       verified binary so this launch starts current; and
#   (b) a BACKGROUND poll sidecar, started with `&` BEFORE the exec (so it
#       survives the exec as an init-owned orphan). It polls every CHECK_MIN
#       minutes; on a newer+verified version it does the swap, then signals a
#       restart by killing the miner so HiveOS relaunches THIS script and picks
#       up the new binary on the next start. The sidecar self-exits if the miner
#       process is gone (slot stopped), so it never spins on a dead slot.
# Both layers verify before swapping; neither EVER leaves the rig unable to mine.
#
# SP2 — csd-relay-node canonical-anchor relay
# ────────────────────────────────────────────────────────────────────────────────
# Before exec-ing the miner this script launches csd-relay-node in the background
# with strict resource caps so it NEVER starves the GPU miner:
#
#   nice -n 19     lowest user-space CPU scheduling priority
#   ionice -c 3    idle I/O class — disk only when nothing else needs it
# NOTE: taskset -c 0 is intentionally ABSENT — pinning to core 0 shares the
# system/IRQ core and degrades both the relay and system interrupts. Let the
# scheduler place it; nice+ionice provide sufficient yield.
#
# The relay is launched with `&` (background) before `exec`, which replaces this
# shell process. The relay process is an orphan owned by HiveOS's init; HiveOS
# restarts the whole slot (and therefore this script) when the miner exits, which
# naturally re-launches the relay too. No separate PID tracking is needed here.
#
# Launch args (REAL flag names, confirmed against binary):
#   --rpc             127.0.0.1:18645
#   --peer-seeds      <multiaddrs>
#   CSD_RELAY_BLACKLIST_ADDR20 env (blacklist delivered via environment, not a CLI flag)
#   --p2p-listen      /ip4/0.0.0.0/tcp/18644  (multiaddr)
# Fill in real peer-seeds in the constants block below.
#
# Forced miner flags (cannot be overridden from the flightsheet, by design):
#   --stats-port $CUSTOM_API_PORT --stats-bind 127.0.0.1
#     so h-stats.sh can scrape /1/summary on localhost only (never exposed).
# Everything else (address via --config, --backend, --gpu-id, …) comes from
# h-config.sh's output. The pool endpoint is compiled in and not configurable.

cd "$(dirname "$0")" || exit 1
# shellcheck source=/dev/null
[ -e h-manifest.conf ] && . ./h-manifest.conf

CONF="${CUSTOM_CONFIG_FILENAME:-config.toml}"
PORT="${CUSTOM_API_PORT:-3380}"
LOG="${CUSTOM_LOG_BASENAME:-/var/log/miner/csdpool/csd-pool-miner}.log"
mkdir -p "$(dirname "$LOG")"

# Re-render the config/flags ONLY if config.toml has no valid address yet. In the
# normal HiveOS flow, HiveOS runs h-config.sh (WITH the flight-sheet env) BEFORE us,
# so config.toml + extra-flags are already correct. HiveOS does NOT export
# $CUSTOM_USER_CONFIG / $CUSTOM_TEMPLATE into h-run.sh's environment, so re-running
# h-config here would regenerate them env-less and BLANK the address + --backend
# (the v0.1.17 P0: miner exits "got 0 chars", and the dropped backend forced the CPU
# build). Only (re)generate when the config is genuinely missing/addressless (e.g. a
# manual h-run.sh invocation); h-config is idempotent as a second-line backstop.
if [ -x ./h-config.sh ] && ! grep -qE '^address = "[0-9a-fA-F]{40}"' "$CONF" 2>/dev/null; then
  ./h-config.sh >/dev/null 2>&1
fi

# Load the extra flags h-config.sh wrote (word-split intentionally so each
# token becomes its own argv entry).
EXTRA_FLAGS=""
EXTRA_FLAGS_FILE="$(dirname "$CONF")/extra-flags"
[ -f "$EXTRA_FLAGS_FILE" ] && EXTRA_FLAGS="$(cat "$EXTRA_FLAGS_FILE")"

# ── Auto-update constants + helpers (mirror of mine-auto.sh, fail-safe) ───────
# The release publishes per-variant assets named csd-pool-miner-linux-<variant>
# (nvidia|amd|cpu). The HiveOS binary is renamed to csd-gpu-miner ($CUSTOM_BIN)
# to dodge the exec-rename hazard, so we download the right asset for the
# flightsheet backend and atomically swap it ONTO $CUSTOM_BIN.
REPO="dangraagu/CSD-Mining-pool-public"
UPDATE_BIN="${CUSTOM_BIN:-$(dirname "$0")/csd-gpu-miner}"
CHECK_MIN="${CHECK_MIN:-15}"
MINER_PROC="csd-gpu-miner"   # argv name HiveOS sees; used to detect/kill the miner
# Marker file recording which variant ($UPDATE_BIN actually IS (nvidia|amd|cpu).
# Written next to the binary after every successful swap, and (best-effort) seeded
# at install/first-run from the binary's own self-report. Lets the startup check
# notice a variant MISMATCH (bundled opencl binary on a --backend cuda rig) and
# correct it even when the VERSION already matches — the 15-day fleet bug.
VARIANT_MARKER="$(dirname "$UPDATE_BIN")/.installed-variant"

# Map the flightsheet --backend to the release asset variant. Default to cpu
# (always published) when no/unknown backend is given, so the asset name is
# never empty and the cpu build is the safe fallback.
update_variant() {
  case "$EXTRA_FLAGS" in
    *"--backend cuda"*)   echo "nvidia" ;;
    *"--backend opencl"*) echo "amd" ;;
    *"--backend cpu"*)    echo "cpu" ;;
    *)                    echo "cpu" ;;
  esac
}

# Which variant the binary self-reports it was COMPILED as, mapped to the asset
# names (nvidia|amd|cpu). Source of truth = the binary's `devices` subcommand,
# which prints exactly "build features: cuda=<bool> opencl=<bool>" to stdout and
# exits (network-free, no pool, no GPU required to read the line). cuda=true ->
# nvidia; opencl=true -> amd; both false -> cpu. Empty string if the binary can't
# report (missing / too old / unreadable), so the caller falls back to the marker.
ua_binary_variant() {
  [ -x "$UPDATE_BIN" ] || { echo ""; return; }
  feats="$("$UPDATE_BIN" devices 2>/dev/null | grep -m1 'build features:')" || feats=""
  case "$feats" in
    *"cuda=true"*)   echo "nvidia" ;;
    *"opencl=true"*) echo "amd" ;;
    *"cuda=false"*"opencl=false"*) echo "cpu" ;;
    *) echo "" ;;   # unrecognised / no line → unknown
  esac
}

# The variant currently INSTALLED on disk. Prefer the binary's own self-report
# (authoritative); fall back to the marker file the package/last-swap wrote; empty
# if neither is available. The marker is a cheap, no-exec hint for the case where
# the binary predates the `devices` self-report or can't run on this host.
ua_installed_variant() {
  v="$(ua_binary_variant)"
  if [ -n "$v" ]; then echo "$v"; return; fi
  if [ -f "$VARIANT_MARKER" ]; then
    m="$(tr -d '[:space:]' < "$VARIANT_MARKER" 2>/dev/null)"
    case "$m" in nvidia|amd|cpu) echo "$m"; return ;; esac
  fi
  echo ""
}

# Record the installed variant in the marker (best-effort; a failed write never
# blocks mining — the binary self-report is still the source of truth on the next
# start). Called after a successful swap.
ua_write_variant_marker() {
  printf '%s\n' "$1" > "$VARIANT_MARKER" 2>/dev/null || true
}

# Download $1 -> $2 atomically: fetch to a temp file, move into place only on
# success, so a partial/failed download never leaves a 0-byte file that later
# gets chmod+x'd and exec'd. Returns non-zero on failure.
ua_download() {
  url="$1"; out="$2"; tmp="$2.dl"
  if command -v curl >/dev/null 2>&1; then
    curl -fL --retry 3 -o "$tmp" "$url" && mv "$tmp" "$out"
  elif command -v wget >/dev/null 2>&1; then
    wget -O "$tmp" "$url" && mv "$tmp" "$out"
  else
    return 1
  fi
}

# Latest published version, or empty string on any failure.
#
# FIX #8: resolve via the releases/latest/download/ CDN asset latest-version.txt,
# NOT api.github.com. The unauthenticated API caps at 60 req/hr/IP, so a farm of
# HiveOS rigs behind one public IP gets 403 + an empty tag + a fleet that
# silently stops updating. The CDN download path has no per-IP limit. Bare
# version (no leading 'v'); empty on offline/404 → caller keeps mining on the
# installed binary. We never fall back to the rate-limited API.
ua_latest_tag() {
  url="https://github.com/$REPO/releases/latest/download/latest-version.txt"
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

# Decide whether $2 (latest) is newer than $1 (installed). Prefer the binary's
# OWN `check-update` (one tested numeric semver compare). If no usable binary
# exists yet, fall back to a plain string inequality. Returns 0 (update) / 1.
ua_should_update() {
  _installed="$1"; _latest="$2"
  if [ -x "$UPDATE_BIN" ] && "$UPDATE_BIN" check-update --current "$_installed" --latest "$_latest" >/dev/null 2>&1; then
    return 0
  fi
  # Subcommand present but exited non-zero == up-to-date/older: do NOT update.
  if [ -x "$UPDATE_BIN" ] && "$UPDATE_BIN" check-update --help >/dev/null 2>&1; then
    return 1
  fi
  # No usable binary yet (or it predates check-update): string fallback. Strip a
  # leading 'v' from BOTH sides first, so a bare installed version ("0.1.10",
  # read from --version) is not seen as different from a 'v'-prefixed release tag
  # ("v0.1.10") — which would otherwise trigger a needless re-download every poll.
  [ "${_installed#v}" != "${_latest#v}" ]
}

# Expected SHA-256 hex for asset $1 from the release SHA256SUMS (empty if no
# checksums published or the asset isn't listed → caller treats empty as
# "cannot verify"). Line format: "<hex>  <filename>" (sha256sum style).
ua_expected_sha() {
  asset="$1"; sums="$(dirname "$UPDATE_BIN")/SHA256SUMS.htmp"
  if ua_download "https://github.com/$REPO/releases/latest/download/SHA256SUMS" "$sums" 2>/dev/null; then
    awk -v a="$asset" '$2==a || $2=="*"a {print $1; exit}' "$sums"
    rm -f "$sums"
  fi
}

# Download the matching variant build, VERIFY it, and only then atomically swap
# it onto $UPDATE_BIN (= $CUSTOM_BIN). NEVER writes an unverified binary onto the
# live path. On a non-cpu 404, falls back to the cpu asset. Returns non-zero
# (and leaves the running binary untouched) if no verified binary could be
# staged — so the caller can always keep mining on the current binary.
ua_download_verify_swap() {
  variant="$(update_variant)"
  asset="csd-pool-miner-linux-$variant"
  # FIX C-4: per-process staging temp ("$UPDATE_BIN.new.$$") so the startup
  # check and the background sidecar (two PIDs) can never collide on a single
  # fixed ".new" file — a concurrent writer would otherwise corrupt the other's
  # download. Every return path below rm's "$staged"; the success path mv's it
  # away. Any stale ".new.*" from a hard-killed updater is swept at startup.
  staged="$UPDATE_BIN.new.$$"
  if ! ua_download "https://github.com/$REPO/releases/latest/download/$asset" "$staged"; then
    if [ "$variant" != "cpu" ]; then
      echo "[h-run] auto-update: '$variant' build unavailable (404/failed); falling back to cpu build." | tee -a "$LOG"
      variant="cpu"; asset="csd-pool-miner-linux-cpu"
      ua_download "https://github.com/$REPO/releases/latest/download/$asset" "$staged" || { rm -f "$staged"; return 1; }
    else
      rm -f "$staged"; return 1
    fi
  fi

  want="$(ua_expected_sha "$asset")"
  if [ -z "$want" ]; then
    # FIX #9: FAIL CLOSED. Live releases (v0.1.7+) always publish SHA256SUMS, so a
    # missing SHA256SUMS (or '$asset' not listed) is anomalous — refuse the update
    # and keep the EXISTING $CUSTOM_BIN rather than swapping in an unverified
    # download. (Previously this accepted the download: a fail-OPEN hole.) The rig
    # keeps mining on the known-good binary; a bricked rig is never the outcome.
    echo "[h-run] auto-update: refusing unverified update — no SHA256SUMS for this release (or '$asset' not listed). Keeping the running binary." | tee -a "$LOG"
    rm -f "$staged"; return 1
  else
    # Verify with a TRUSTED tool ONLY: the already-installed $UPDATE_BIN's
    # verify-file, else the OS sha256sum. NEVER let the just-downloaded file
    # verify itself. With a digest but no trusted verifier → FAIL CLOSED.
    if [ -x "$UPDATE_BIN" ] && "$UPDATE_BIN" verify-file --help >/dev/null 2>&1; then
      if ! "$UPDATE_BIN" verify-file "$staged" "$want" >/dev/null 2>&1; then
        echo "[h-run] auto-update: SHA-256 verify FAILED for $asset — discarding it, keeping the running binary." | tee -a "$LOG"
        rm -f "$staged"; return 1
      fi
    elif command -v sha256sum >/dev/null 2>&1; then
      got="$(sha256sum "$staged" | awk '{print $1}')"
      if [ "$got" != "$want" ]; then
        echo "[h-run] auto-update: SHA-256 verify FAILED for $asset (got $got, want $want) — discarding it." | tee -a "$LOG"
        rm -f "$staged"; return 1
      fi
    else
      echo "[h-run] auto-update: have a SHA256SUMS digest but no trusted verifier — refusing the update." | tee -a "$LOG"
      rm -f "$staged"; return 1
    fi
  fi

  # SAME-BINARY GUARD (launcher-only releases): if the on-disk binary ALREADY
  # hashes to $want (the just-verified target digest), the download is byte-
  # identical — a launcher-only version bump (e.g. 0.1.10 binary republished as
  # 0.1.11) where the miner reports a stale --version forever. Swapping it in and
  # (in the sidecar) killing the miner to "apply" it would re-fire every poll
  # → fleet-wide flapping (a miner restart every CHECK_MIN), because
  # ua_installed_version re-reads the unchanged --version each time and never
  # converges. So if the on-disk binary matches $want, DISCARD the staged copy
  # and return 2 (= "verified, but no change") so the caller can skip the swap +
  # the miner-kill. We only need to READ the on-disk binary to hash it (exec bit
  # not required), so gate on -e; verify with the binary's own verify-file when it
  # is executable and supports it, else the OS sha256sum.
  if [ -e "$UPDATE_BIN" ]; then
    if [ -x "$UPDATE_BIN" ] && "$UPDATE_BIN" verify-file --help >/dev/null 2>&1; then
      if "$UPDATE_BIN" verify-file "$UPDATE_BIN" "$want" >/dev/null 2>&1; then
        rm -f "$staged"; return 2
      fi
    elif command -v sha256sum >/dev/null 2>&1; then
      cur="$(sha256sum "$UPDATE_BIN" | awk '{print $1}')"
      if [ "$cur" = "$want" ]; then rm -f "$staged"; return 2; fi
    else
      # NO-FLAP fail-safe: the binary EXISTS but we have NO usable verifier for the
      # same-binary check (no verify-file, no sha256sum), so we cannot prove the
      # staged bytes differ from the on-disk binary. Swapping anyway would let the
      # sidecar treat it as a real update (rc=0 → kill+restart) and, on a
      # launcher-only bump (identical bytes), re-fire EVERY poll = fleet-wide flap.
      # So discard the staged copy and report rc=2 ("verified, no change") — the
      # safe no-op. A genuinely newer binary is still caught by the version gate
      # (ua_should_update) on the next startup check; we only forgo the swap when we
      # cannot tell, which is the fail-safe direction for a no-clawback fleet.
      rm -f "$staged"; return 2
    fi
  fi

  chmod +x "$staged" 2>/dev/null || true
  mv "$staged" "$UPDATE_BIN"   # atomic swap onto the live path, only after verify
  # Record WHICH variant now lives on disk (the FINAL $variant — may be "cpu" if a
  # non-cpu asset 404'd and we fell back above). Best-effort; the binary's own
  # `devices` self-report remains the source of truth on the next start.
  ua_write_variant_marker "$variant"
}

# Current installed version string (for the semver compare). "none" if the
# binary can't report it, which makes ua_should_update take the string path.
ua_installed_version() {
  if [ -x "$UPDATE_BIN" ]; then
    v="$("$UPDATE_BIN" --version 2>/dev/null | grep -Eo '[0-9]+\.[0-9]+\.[0-9]+' | head -n1)"
    [ -n "$v" ] && { echo "$v"; return; }
  fi
  echo "none"
}

# (a) STARTUP update check — one-shot, BEFORE the exec. Best-effort and fully
# fail-safe: any failure logs and returns, leaving the existing $CUSTOM_BIN in
# place so the rig always starts mining on a known-good binary.
#
# VARIANT-AWARE (FIX): the swap fires on a newer VERSION *or* a variant MISMATCH.
# A fresh HiveOS install bundles ONE prebuilt binary (the opencl/amd build, due to
# the release-packaging bug) whose --version already equals "latest", so the
# version gate alone NEVER corrects it and an NVIDIA rig stays stuck on opencl
# ("cuda=false opencl=true" under --backend cuda). Detecting the installed variant
# from the binary's own `devices` self-report (with the marker file as fallback)
# lets us fetch+verify+swap the CORRECT variant even when the version matches.
hive_update_check_startup() {
  latest="$(ua_latest_tag || true)"
  installed="$(ua_installed_version)"
  want_var="$(update_variant)"
  have_var="$(ua_installed_variant)"

  # VARIANT correction is possible even when we are OFFLINE for the version check
  # only insofar as we can reach the CDN for the asset — ua_download_verify_swap
  # needs the network regardless. But the DECISION to try must not require a tag:
  # a known variant mismatch is a reason to attempt the swap even if latest-version
  # .txt is momentarily unreachable (the download/verify below is still fail-safe).
  reason=""
  if [ -n "$latest" ] && ua_should_update "$installed" "$latest"; then
    reason="version"
  elif [ -n "$have_var" ] && [ "$have_var" != "$want_var" ]; then
    reason="variant"
  fi

  if [ -z "$reason" ]; then
    [ -z "$latest" ] && echo "[h-run] auto-update: no release tag (offline / rate-limited) and installed variant ($have_var) matches requested ($want_var) — starting on the installed binary." | tee -a "$LOG"
    return 0
  fi

  if [ "$reason" = "variant" ]; then
    echo "[h-run] auto-update: WRONG-VARIANT binary detected — installed=$have_var but flightsheet requests $want_var; fetching the $want_var build (verify, then swap before launch)." | tee -a "$LOG"
  else
    echo "[h-run] auto-update: $installed -> $latest (verify, then swap before launch)" | tee -a "$LOG"
  fi

  ua_download_verify_swap; rc=$?
  if [ "$rc" -eq 0 ]; then
    echo "[h-run] auto-update: swapped in the $want_var build${latest:+ ($latest)}; launching it." | tee -a "$LOG"
  elif [ "$rc" -eq 2 ]; then
    # Verified, but the on-disk binary is already byte-identical to the requested
    # asset (a launcher-only version bump where --version stays stale). Nothing to
    # swap. If we got here via the VARIANT trigger, the installed binary actually
    # IS the requested variant (a stale/wrong marker, or a self-report blip, made us
    # think otherwise) — reconcile the marker to $want_var so the variant trigger
    # stops re-firing every start.
    echo "[h-run] auto-update: on-disk binary already matches the requested $want_var build — no swap needed." | tee -a "$LOG"
    [ "$reason" = "variant" ] && ua_write_variant_marker "$want_var"
  else
    echo "[h-run] auto-update: update not applied (download/verify failed) — keeping current binary, still mining." | tee -a "$LOG"
  fi
  return 0
}

# (b) BACKGROUND poll sidecar — started with `&` BEFORE exec, so it survives the
# exec as an init-owned orphan. Polls every CHECK_MIN; on a newer+verified
# version it swaps then signals a restart by killing the miner (HiveOS relaunches
# this script, which picks up the new binary via the startup check). Self-exits
# when the miner is gone (slot stopped) so it never spins on a dead slot.
hive_update_sidecar() {
  # Give the miner a moment to come up before the first liveness probe.
  sleep "$((CHECK_MIN * 60))"
  # Version we have already CONFIRMED the on-disk binary satisfies byte-for-byte
  # (set when ua_download_verify_swap reports "no change", rc=2). While latest
  # equals this, we skip the whole download attempt — so a launcher-only bump
  # (binary unchanged, --version stale) does NOT re-download every poll.
  # CAVEAT: this keys on the VERSION string, so re-cutting the SAME version with
  # NEW bytes is NOT picked up by an already-running sidecar (it stays pinned to
  # confirmed_current_for and skips the fetch). To ship changed bytes to a live
  # fleet, BUMP THE VERSION (don't republish an existing tag) — a higher version
  # makes latest != confirmed_current_for and the sidecar fetches + verifies again.
  confirmed_current_for=""
  while true; do
    # If the miner isn't running, the slot was stopped (not an update restart):
    # exit so we don't poll forever after HiveOS tears the slot down.
    if ! pgrep -x "$MINER_PROC" >/dev/null 2>&1; then
      echo "[h-run] auto-update sidecar: miner no longer running — exiting sidecar." >> "$LOG" 2>&1
      exit 0
    fi
    latest="$(ua_latest_tag || true)"
    if [ -n "$latest" ] && [ "$latest" != "$confirmed_current_for" ]; then
      installed="$(ua_installed_version)"
      if ua_should_update "$installed" "$latest"; then
        echo "[h-run] auto-update sidecar: $installed -> $latest (verify, then swap + restart)" >> "$LOG" 2>&1
        ua_download_verify_swap; rc=$?
        if [ "$rc" -eq 0 ]; then
          echo "[h-run] auto-update sidecar: swapped in $latest — restarting miner so HiveOS relaunches on the new binary." >> "$LOG" 2>&1
          # Stop the relay too so the relaunch starts it cleanly.
          pkill -f csd-relay-node 2>/dev/null || true
          pkill -f "$MINER_PROC" 2>/dev/null || true
          exit 0
        elif [ "$rc" -eq 2 ]; then
          # NO-BRICK / NO-FLAP: the on-disk binary already matches $latest (a
          # launcher-only bump). DO NOT kill the miner — that would re-fire every
          # poll forever. Mark this version confirmed so we stop re-downloading it,
          # and keep polling so a FUTURE real binary bump is still caught.
          echo "[h-run] auto-update sidecar: on-disk binary already matches $latest (launcher-only bump) — not restarting; will watch for a newer release." >> "$LOG" 2>&1
          confirmed_current_for="$latest"
        else
          echo "[h-run] auto-update sidecar: update not applied (download/verify failed) — keeping current, will retry." >> "$LOG" 2>&1
        fi
      fi
    fi
    sleep "$((CHECK_MIN * 60))"
  done
}
# ── end auto-update helpers ───────────────────────────────────────────────────

# Test hook: when CSD_SOURCE_ONLY=1, stop here so a test can `source` this script
# to exercise the auto-update helpers (esp. ua_download_verify_swap's same-binary
# guard) WITHOUT running the startup check, the sidecar, the relay launch, or the
# exec. Zero effect on a normal HiveOS run (the var is unset there).
if [ "${CSD_SOURCE_ONLY:-0}" = "1" ]; then
  # shellcheck disable=SC2317
  { return 0 2>/dev/null; exit 0; }
fi

# (a) Run the startup update check now (before relay + exec) so this launch
# starts on the latest verified binary. Fully fail-safe — never blocks mining.
hive_update_check_startup

# (b) Start the background update poll sidecar. `&` + disown semantics: it keeps
# running across the exec below (it becomes an init-owned orphan), polling for a
# newer release and triggering a clean restart when one verifies. Output goes to
# the miner log. It NEVER touches the live binary except via the verified swap.
#
# FIX C-1: REAP any prior sidecar from a previous launch of THIS slot before
# spawning a fresh one. A HiveOS-initiated restart that is NOT the sidecar's own
# pkill (flightsheet edit, OC apply, GPU-watchdog restart, manual restart) re-runs
# h-run.sh -> a NEW sidecar, while the OLD (sleeping) sidecar wakes, sees the miner
# alive, and keeps polling -> N concurrent sidecars accumulate over uptime
# (redundant CDN polls + multiple pkills on update). We tag the sidecar with the
# UNIQUE marker `csd-hive-update-sidecar` (carried as argv[0] of its `bash -c`, so
# it is visible to `pgrep -f`/`pkill -f` via /proc/<pid>/cmdline) and pkill that
# marker here BEFORE spawning the new one — mirroring the relay orphan-cleanup at
# the SP2 launch below. The marker is unique: it does NOT substring-match the
# miner (csd-gpu-miner), the relay (csd-relay-node), or the launcher (h-run.sh),
# so the reap can never kill the freshly-launched instance, the miner, or the
# relay. A bare `pkill -f h-run.sh` is intentionally AVOIDED for that reason.
SIDE_MARKER="csd-hive-update-sidecar"
pkill -f "$SIDE_MARKER" 2>/dev/null || true
# Sweep any stale per-pid staging temp ("$UPDATE_BIN.new.*", FIX C-4) left by an
# updater that was hard-killed mid-download (e.g. a reaped sidecar), so they never
# accumulate. The live binary itself is "$UPDATE_BIN" (no suffix) and is untouched.
rm -f "$UPDATE_BIN".new.* 2>/dev/null || true
# The sidecar is a shell function; to make its marker land in the process argv we
# launch it via `bash -c '<fn>' <marker>` (argv[0]=<marker>). That child bash
# needs the function and its whole call graph + the constants they read, so we
# export them. Keep this export list in sync if a new ua_* helper is added.
export -f hive_update_sidecar ua_latest_tag ua_installed_version ua_should_update \
          ua_download_verify_swap ua_download ua_expected_sha update_variant \
          ua_binary_variant ua_installed_variant ua_write_variant_marker
export UPDATE_BIN REPO CHECK_MIN MINER_PROC LOG EXTRA_FLAGS VARIANT_MARKER
bash -c 'hive_update_sidecar' "$SIDE_MARKER" >> "$LOG" 2>&1 &
echo "[h-run] auto-update sidecar started (PID=$!, marker=$SIDE_MARKER, poll every ${CHECK_MIN} min)." | tee -a "$LOG"

# ── SP2: csd-relay-node — canonical-anchor relay ─────────────────────────────
# Must launch BEFORE exec (exec replaces this shell; once exec runs we cannot
# background anything). The relay runs as an orphan owned by HiveOS init; it is
# naturally killed and re-spawned when HiveOS restarts the miner slot.
#
# Resource cap (nice+ionice only; taskset intentionally absent — see header):
#   nice -n 19   lowest user scheduling priority
#   ionice -c 3  idle I/O class (disk only when nothing else is queued)
#
# SP2 relay-node launch args (REAL flags confirmed against binary):
#   --rpc             127.0.0.1:18645         local RPC port
#   --datadir         /var/lib/csd-relay       relay chain data directory
#   --wallet          /var/lib/csd-relay/wallet.json  placeholder wallet (required by binary)
#   --peer-seeds      <comma-sep multiaddrs>   well-known honest peers
#   --p2p-listen      /ip4/0.0.0.0/tcp/18644  p2p listen (multiaddr)
#   CSD_RELAY_BLACKLIST_ADDR20 env             addr20 blacklist file path (node writes it)
#   CSD_BLACKLIST_URL env                      signed-blacklist source; ENABLES the node's
#                                              built-in 15-min Ed25519-signed fetcher (pulls,
#                                              verifies, writes the addr20 file fail-closed)
#   CSD_CANONICAL_TIP_URL env                 canonical oracle
#   CSD_CANON_REORG_AHEAD env                 SP1.1 auth-reorg depth (= 7)
#
# Seeds + wallet-new subcommand confirmed against the release relay binary
# (csd-node `node` subcommand): `--peer-seeds` accepts the operator's libp2p
# multiaddrs and `wallet new --out <path>` is correct.
#
RELAY_BIN="$(dirname "$0")/csd-relay-node"
RELAY_DATADIR="/var/lib/csd-relay"
RELAY_WALLET="$RELAY_DATADIR/wallet.json"
RELAY_BLACKLIST="$RELAY_DATADIR/blacklist.txt"
RELAY_LOG="/var/log/miner/csdpool/csd-relay-node.log"
# Required by the `node` subcommand (binary rejects its absence). The relay is a
# follower and never mines, so an empty fanout list is correct.
RELAY_PUSH_PEERS="$RELAY_DATADIR/push-peers.txt"
mkdir -p "$RELAY_DATADIR" "$(dirname "$RELAY_LOG")"
[ -f "$RELAY_PUSH_PEERS" ] || : > "$RELAY_PUSH_PEERS"

if [ -x "$RELAY_BIN" ]; then
  # Orphan cleanup: kill any leftover relay from a previous HiveOS slot restart
  # before we launch a fresh one (avoids port conflicts on :18645/:18644).
  pkill -f csd-relay-node 2>/dev/null || true

  # Wallet: required by the binary even when not mining. Generate a throwaway
  # placeholder on first run.
  if [ ! -f "$RELAY_WALLET" ]; then
    echo "[h-run] SP2: relay wallet absent — generating placeholder wallet..." | tee -a "$LOG"
    if "$RELAY_BIN" wallet new --out "$RELAY_WALLET" >> "$RELAY_LOG" 2>&1; then
      echo "[h-run] SP2: relay wallet created at $RELAY_WALLET" | tee -a "$LOG"
    else
      echo "[h-run] SP2: WARNING — wallet generation failed; relay may refuse to start. Check $RELAY_LOG." | tee -a "$LOG"
    fi
  fi

  echo "[h-run] SP2: launching csd-relay-node (nice 19 / ionice idle)" | tee -a "$LOG"
  CSD_RELAY_BLACKLIST_ADDR20="$RELAY_BLACKLIST" \
  CSD_BLACKLIST_URL="https://lisens.yamaduo.no/blacklist" \
  CSD_CANONICAL_TIP_URL="https://explorer.computesubstrate.org" \
  CSD_CANON_REORG_AHEAD="7" \
  nice -n 19 ionice -c 3 \
    "$RELAY_BIN" \
    node \
    --rpc 127.0.0.1:18645 \
    --datadir "$RELAY_DATADIR" \
    --wallet "$RELAY_WALLET" \
    --push-peers-file "$RELAY_PUSH_PEERS" \
    --peer-seeds /ip4/81.167.197.88/tcp/17999/p2p/12D3KooWA2GFgHLyXSZFVnzuchdesWhqnu7HWw637RXF9P6vW6zK,/ip4/141.94.163.242/tcp/18007/p2p/12D3KooWKGhuUhAwGDf3MtqL581h3gttvFg9Z2p1ej9wFTdKfdSM,/ip4/135.125.170.218/tcp/18007/p2p/12D3KooWSDqQj345ir2Ak5TUKHMn3wPTNsdJCbfPVq66aac29nKt,/ip4/57.129.84.73/tcp/18007/p2p/12D3KooWLydGAnXtXH4L37gVZWohAZNvKdFgHwVN4nhUzgrvX8cW,/ip4/158.69.116.36/tcp/17999/p2p/12D3KooWHKcjL8M5snr3GniC8xRtGJGbGhPSdGiqtZNRz6UFj1t3,/ip4/145.239.0.111/tcp/17999/p2p/12D3KooWFsHa5ifqK45Fjd8cYnDkVDN8R8MfjfiETNpEqnbGAEez \
    --p2p-listen /ip4/0.0.0.0/tcp/18644 \
    >> "$RELAY_LOG" 2>&1 &
  echo "[h-run] SP2: csd-relay-node PID=$! (log: $RELAY_LOG)" | tee -a "$LOG"
else
  echo "[h-run] SP2: csd-relay-node not found at $RELAY_BIN — skipping relay launch." | tee -a "$LOG"
fi
# ── end SP2 relay launch ──────────────────────────────────────────────────────

# Driver check: if the flightsheet asks for a GPU backend, make sure the driver
# is actually present, else log a clear line. We do NOT hard-exit (auto-fallback
# in the binary will drop to CPU), but the operator sees why.
case "$EXTRA_FLAGS" in
  *"--backend cuda"*)
    if ! command -v nvidia-smi >/dev/null 2>&1 || ! nvidia-smi -L >/dev/null 2>&1; then
      echo "[h-run] WARNING: --backend cuda requested but nvidia-smi found no GPU; the miner will auto-fall back to CPU." | tee -a "$LOG"
    fi
    ;;
  *"--backend opencl"*)
    if ! command -v clinfo >/dev/null 2>&1 || ! clinfo 2>/dev/null | grep -q 'Device Type.*GPU'; then
      echo "[h-run] WARNING: --backend opencl requested but clinfo listed no GPU; the miner will auto-fall back to CPU." | tee -a "$LOG"
    fi
    ;;
esac

echo "[h-run] starting csd-gpu-miner (stats on 127.0.0.1:$PORT)" | tee -a "$LOG"

# EXEC the real binary. Word-splitting $EXTRA_FLAGS is intentional (each token
# is a separate argv entry), so shellcheck SC2086 is suppressed for that one
# expansion only. $EXTRA_FLAGS can no longer contain --stats-port/--stats-bind
# (h-config.sh strips them), so the forced localhost stats binding always applies.
#
# DO NOT wrap this exec (e.g. `exec taskset … csd-gpu-miner`): the process must
# keep /proc/comm == "csd-gpu-miner" — both the auto-update sidecar's liveness
# probe (pgrep -x) and h-stop.sh's reaper (pkill -x) match on that exact comm.
# shellcheck disable=SC2086
exec "$CUSTOM_BIN" --config "$CONF" \
  --stats-port "$PORT" --stats-bind 127.0.0.1 \
  $EXTRA_FLAGS
