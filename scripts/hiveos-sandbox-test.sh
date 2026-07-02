#!/usr/bin/env bash
# hiveos-sandbox-test.sh — execute a Linux csd-pool-miner binary inside the
# PERMANENT old-glibc WSL sandbox (`csd-hiveos-sandbox`, Ubuntu 18.04 /
# glibc 2.27 = the stock-HiveOS floor) to prove it actually RUNS on
# HiveOS-era glibc BEFORE a fleet tag.
#
# WHY THIS EXISTS: v0.2.0 Linux binaries were built on ubuntu-24.04 runners
# (GLIBC_2.39 symbols) and crash-looped on stock HiveOS (glibc 2.27/2.31),
# taking ~24% of the fleet down. This harness is the standing pre-tag gate:
# every Linux release binary must PASS here before tagging. Operator
# directive: "always covered and tested in sandbox for HiveOS".
#
# USAGE (from Git Bash on Windows, or from inside any WSL distro with
# Windows interop enabled):
#   scripts/hiveos-sandbox-test.sh <path-to-linux-miner-binary>
# The path may be a Windows path (C:\... or C:/...), a Git-Bash path
# (/c/...), or a WSL path (/mnt/c/...). With no argument it defaults to the
# repo-local zigbuild output:
#   target/x86_64-unknown-linux-gnu/release/csd-gpu-miner
#
# CHECKS (all executed INSIDE the csd-hiveos-sandbox distro):
#   1. --version            must print something and exit 0
#   2. devices              must execute; "no usable gpu/runtime" printed
#                           gracefully is a PASS — only a loader/glibc
#                           error, signal death, or exec failure is a FAIL
#   3. selftest --trials 4  must exit 0 (CPU backend cross-check must pass;
#                           CUDA/OpenCL absent => skipped is fine)
# Exit code: 0 = all checks PASS, 1 = any FAIL.
#
# REBUILDING THE SANDBOX (if the distro is ever lost):
#   curl -LO https://cdimage.ubuntu.com/ubuntu-base/releases/18.04/release/ubuntu-base-18.04.5-base-amd64.tar.gz
#   wsl --import csd-hiveos-sandbox C:\Users\bahs_admin\wsl\csd-hiveos-sandbox ubuntu-base-18.04.5-base-amd64.tar.gz --version 2
#   wsl -d csd-hiveos-sandbox -e bash -lc 'ldd --version | head -1'   # must say 2.27

set -u

DISTRO="csd-hiveos-sandbox"
SANDBOX_DIR="/tmp/csd-hiveos-test"
SANDBOX_BIN="$SANDBOX_DIR/bin-under-test"

# --- Keep Git-Bash (MSYS) from mangling /tmp/... args passed to wsl.exe ----
export MSYS_NO_PATHCONV=1
export MSYS2_ARG_CONV_EXCL="*"

# --- Locate wsl.exe (works from Git Bash and from inside a WSL distro) -----
WSL="wsl.exe"
if ! command -v "$WSL" >/dev/null 2>&1; then
    for cand in /mnt/c/Windows/System32/wsl.exe /c/Windows/System32/wsl.exe; do
        [ -x "$cand" ] && WSL="$cand" && break
    done
fi
if ! command -v "$WSL" >/dev/null 2>&1 && [ ! -x "$WSL" ]; then
    echo "FATAL: wsl.exe not found (need Windows interop or Git Bash)" >&2
    exit 1
fi

# Run a command inside the sandbox distro. stdout+stderr merged to stdout.
in_sandbox() {
    "$WSL" -d "$DISTRO" -- sh -c "$1" 2>&1
}

# --- Sandbox must exist -----------------------------------------------------
if ! "$WSL" -d "$DISTRO" -e true >/dev/null 2>&1; then
    echo "FATAL: WSL distro '$DISTRO' not found. Rebuild it (see header of this script)." >&2
    exit 1
fi

# --- Resolve the binary argument -------------------------------------------
SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
DEFAULT_BIN="$SCRIPT_DIR/../target/x86_64-unknown-linux-gnu/release/csd-gpu-miner"
ARG="${1:-$DEFAULT_BIN}"

# Convert whatever path style we were given into a /mnt/... path the sandbox
# can read (Windows drives are automounted at /mnt/<drive> inside WSL).
to_mnt_path() {
    local p="$1"
    case "$p" in
        [A-Za-z]:[\\/]*)   # Windows path C:\... or C:/...
            local drive rest
            drive="$(printf '%s' "${p:0:1}" | tr '[:upper:]' '[:lower:]')"
            rest="${p:2}"
            rest="${rest//\\//}"
            printf '/mnt/%s%s' "$drive" "$rest"
            ;;
        /mnt/*)            # already a WSL path
            printf '%s' "$p"
            ;;
        /[A-Za-z]/*)       # Git-Bash path /c/...
            printf '/mnt/%s' "${p:1}"
            ;;
        *)                 # relative or plain POSIX path: resolve on this side
            local abs
            abs="$(cd "$(dirname "$p")" 2>/dev/null && pwd)/$(basename "$p")" || abs="$p"
            to_mnt_path "$abs"
            ;;
    esac
}

SRC_MNT="$(to_mnt_path "$ARG")"

echo "=== csd-pool-miner HiveOS-glibc sandbox gate ==="
echo "binary : $ARG"
echo "sandbox: $DISTRO ($(in_sandbox 'ldd --version | head -1' | tr -d '\r'))"
echo

# --- Copy binary into the sandbox and mark executable -----------------------
if ! in_sandbox "test -f '$SRC_MNT'" >/dev/null; then
    echo "FATAL: binary not visible inside sandbox at: $SRC_MNT" >&2
    echo "       (checked from Windows arg: $ARG)" >&2
    exit 1
fi
if ! in_sandbox "mkdir -p $SANDBOX_DIR && cp '$SRC_MNT' $SANDBOX_BIN && chmod +x $SANDBOX_BIN" >/dev/null; then
    echo "FATAL: failed to copy binary into sandbox /tmp" >&2
    exit 1
fi

# --- Check runner ------------------------------------------------------------
# Loader / glibc failure signatures. Any of these in the output = the binary
# cannot even start on HiveOS-era glibc (exactly the v0.2.0 fleet outage).
# NB: the "(" in "not found (required by" MUST stay escaped — an unescaped
# paren makes grep -E error out and silences loader detection entirely
# (caught by the v0.2.0 negative control on 2026-07-02).
LOADER_ERR_RE="GLIBC_[0-9.]+.{0,2}not found|GLIBCXX_[0-9.]+.{0,2}not found|error while loading shared libraries|cannot execute binary file|Exec format error|No such file or directory\$|not found \(required by"

FAILED=0

# run_check <name> <timeout-s> <argv-tail> <mode>
#   mode=strict   : exit must be 0 AND output non-empty AND no loader error
#   mode=graceful : loader error / exec failure (rc>=126) / signal = FAIL;
#                   any graceful run (rc 0 or 1 with real output) = PASS
run_check() {
    local name="$1" tmo="$2" tail="$3" mode="$4"
    local out rc loader_line
    # rc MUST be captured from the wsl/binary invocation itself — piping
    # through tr here would make rc the exit code of tr (always 0) and let a
    # crash-looping binary sail through (caught by the v0.2.0 negative
    # control on 2026-07-02). Strip \r in a separate step.
    out="$(in_sandbox "timeout $tmo $SANDBOX_BIN $tail")"
    rc=$?
    out="$(printf '%s\n' "$out" | tr -d '\r')"
    loader_line="$(printf '%s\n' "$out" | grep -E -m1 "$LOADER_ERR_RE")" || true

    local verdict="PASS" reason=""
    if [ -n "$loader_line" ]; then
        verdict="FAIL"; reason="glibc/loader error: $loader_line"
    elif [ "$rc" -eq 124 ]; then
        verdict="FAIL"; reason="timed out after ${tmo}s"
    elif [ "$rc" -ge 126 ]; then
        verdict="FAIL"; reason="exec failure or killed by signal (exit $rc)"
    elif [ "$mode" = "strict" ] && [ "$rc" -ne 0 ]; then
        verdict="FAIL"; reason="nonzero exit $rc"
    elif [ "$mode" = "strict" ] && [ -z "$out" ]; then
        verdict="FAIL"; reason="no output"
    elif [ "$mode" = "graceful" ] && [ "$rc" -gt 1 ]; then
        verdict="FAIL"; reason="unexpected exit $rc"
    elif [ "$mode" = "graceful" ] && [ -z "$out" ]; then
        verdict="FAIL"; reason="no output (exit $rc)"
    fi

    if [ "$verdict" = "PASS" ]; then
        echo "[PASS] $name"
        printf '%s\n' "$out" | head -4 | sed 's/^/       | /'
    else
        echo "[FAIL] $name — $reason"
        printf '%s\n' "$out" | head -6 | sed 's/^/       | /'
        FAILED=1
    fi
    echo
}

run_check "--version           (must print + exit 0)"        60  "--version"           strict
run_check "devices             (graceful no-GPU = PASS)"     60  "devices"             graceful
run_check "selftest --trials 4 (CPU backend must PASS)"      300 "selftest --trials 4" strict

if [ "$FAILED" -eq 0 ]; then
    echo "=== OVERALL: PASS — binary runs on glibc 2.27 (HiveOS floor) ==="
    exit 0
else
    echo "=== OVERALL: FAIL — DO NOT TAG. Binary will crash-loop on stock HiveOS. ==="
    exit 1
fi
