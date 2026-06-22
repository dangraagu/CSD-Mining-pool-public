#!/usr/bin/env bash
# HERMETIC dry-run harness for FIX C-2 in mine-auto.bat (Windows .bat / cmd.exe).
#
# It extracts the REAL :update_check block from mine-auto.bat at runtime (so the
# test always exercises the shipped code, never a hand-copy), wraps it in a
# stubbed environment, and drives it with a fake curl + a fake %BIN% that
# PREDATES verify-file. Two scenarios:
#   (c) installed %BIN% predates verify-file + a VALID SHA256SUMS is published
#       → the new Get-FileHash OS-verifier fallback must verify and SWAP.
#   (d) NO SHA256SUMS published → must still REFUSE (fail-closed) and keep %BIN%.
#
# Faithfulness:
#   * The verify/swap code under test runs byte-for-byte as shipped. The ONLY
#     transform is neutralizing the network version-resolver (one PowerShell
#     Invoke-WebRequest line) -> `set "LATEST=0.1.99"`, since a sandbox can't
#     serve the live GitHub CDN.
#   * `curl` is stubbed with a REAL compiled curl.exe shim (csc.exe), NOT a .bat:
#     a bare `.bat` invoked from inside a `call`ed cmd subroutine does NOT return
#     to the caller (cmd "chains" rather than "calls" bare batch files), which
#     would derail :update_check. A real .exe returns exactly like production
#     curl. The shim serves latest-version.txt / SHA256SUMS / the asset from
#     files named by env vars, and returns exit 1 when the artifact is absent
#     (mirroring curl -f on a 404 — the mechanism scenario (d) depends on).
#   * %BIN% is a tiny .bat returning errorlevel 1 for `check-update --help` and
#     `verify-file --help` (a pre-v0.1.8 binary). It IS `call`ed by the real code
#     ("%BIN%" verify-file ...), so a .bat is correct there.
#   * :start_miners / :run_crash_hook are stubbed no-ops (the swap, not mining, is
#     what we assert).
set -u
# This harness lives in tests/; the launcher under test is in the repo root.
ROOT="$(cd "$(dirname "$0")/.." && pwd)"
SRC="$ROOT/mine-auto.bat"
fails=0
ok()  { printf '  PASS: %s\n' "$*"; }
bad() { printf '  FAIL: %s\n' "$*"; fails=$((fails+1)); }
win() { cygpath -w "$1" 2>/dev/null || printf '%s' "$1" | sed 's|/|\\|g'; }

# ── compile the real curl.exe shim ONCE ──────────────────────────────────────
CSC="$(ls "C:/Windows/Microsoft.NET/Framework64/"*/csc.exe 2>/dev/null | head -n1)"
[ -z "$CSC" ] && CSC="$(ls "C:/Windows/Microsoft.NET/Framework/"*/csc.exe 2>/dev/null | head -n1)"
if [ -z "$CSC" ]; then echo "FATAL: csc.exe not found — cannot build curl shim"; exit 2; fi
SHIMDIR="$(mktemp -d)"
cat > "$SHIMDIR/curlshim.cs" <<'CS'
using System;
using System.IO;
// Minimal curl(-f -L -s -o OUT URL) shim. Dispatches on URL substring and copies
// the matching sandbox file (named by env var) to OUT (or stdout). Exit 1 when
// the source file is missing (== curl -f hitting a 404). Behaves as an external
// process that returns to its caller, exactly like the real curl.exe.
class CurlShim {
  static int Main(string[] a) {
    string outp = null, url = null;
    for (int i = 0; i < a.Length; i++) {
      if (a[i] == "-o") { if (i+1 < a.Length) { outp = a[i+1]; i++; } }
      else if (a[i].StartsWith("-")) { /* ignore -L -f -s etc. */ }
      else { url = a[i]; }
    }
    if (url == null) return 1;
    string src = null;
    if (url.IndexOf("latest-version.txt", StringComparison.OrdinalIgnoreCase) >= 0)
      src = Environment.GetEnvironmentVariable("CSDH_LATEST");
    else if (url.IndexOf("SHA256SUMS", StringComparison.OrdinalIgnoreCase) >= 0)
      src = Environment.GetEnvironmentVariable("CSDH_SUMS");
    else
      src = Environment.GetEnvironmentVariable("CSDH_ASSET");
    if (src == null || src.Length == 0 || !File.Exists(src)) return 1;  // 404 / -f fail
    try {
      if (outp != null) File.Copy(src, outp, true);
      else Console.Out.Write(File.ReadAllText(src));
    } catch { return 1; }
    return 0;
  }
}
CS
"$CSC" -nologo -out:"$(win "$SHIMDIR/curl.exe")" "$(win "$SHIMDIR/curlshim.cs")" >/dev/null 2>&1
if [ ! -f "$SHIMDIR/curl.exe" ]; then echo "FATAL: curl shim failed to compile"; "$CSC" -nologo -out:"$(win "$SHIMDIR/curl.exe")" "$(win "$SHIMDIR/curlshim.cs")"; exit 2; fi

extract_update_check() {
  awk '
    /^:update_check/{f=1}
    !f{next}
    /^:start_miners/{exit}
    /^for \/f .*latest-version\.txt/ { print "set \"LATEST=0.1.99\""; next }
    {print}
  ' "$SRC"
}

run_scenario() {
  scen="$1"
  SB="$(mktemp -d)"
  DIR="$SB/store"; mkdir -p "$DIR"
  EXE="csd-pool-miner-amd.exe"

  NEWCONTENT="NEW-BINARY-v0.1.99-$scen"
  printf '%s' "$NEWCONTENT" > "$SB/asset_payload"
  REALSHA="$(powershell -NoProfile -Command "(Get-FileHash -Algorithm SHA256 -LiteralPath '$(win "$SB/asset_payload")').Hash.ToLower()" | tr -d '\r')"
  echo "0.1.99" > "$SB/latest"
  if [ "$scen" = "c" ]; then
    # Valid SHA256SUMS. CRLF line ending: this is what `findstr /e /c:" <file>"`
    # (the shipped lookup on line ~224, UNCHANGED by C-2) anchors against — a
    # bare-LF line is not matched by /e. We serve the production-shaped file so
    # the test isolates the C-2 verify/swap fallback, not SUMS parsing.
    printf '%s  %s\r\n' "$REALSHA" "$EXE" > "$SB/SHA256SUMS"
  else
    rm -f "$SB/SHA256SUMS"                                     # scenario d: none
  fi

  # %BIN% = pre-verify-file binary (errorlevel 1 for both subcommands).
  BIN="$DIR/$EXE"
  printf '@echo off\necho %%* | findstr /i "verify-file" >nul && exit /b 1\necho %%* | findstr /i "check-update" >nul && exit /b 1\nexit /b 0\nOLD-PRE-VERIFY-FILE-BINARY' > "$BIN"

  HB="$SB/harness.bat"
  {
    echo '@echo off'
    echo 'setlocal EnableExtensions EnableDelayedExpansion'
    echo "set \"REPO=dangraagu/CSD-Mining-pool-public\""
    echo "set \"VARIANT=amd\""
    echo "set \"DIR=$(win "$DIR")\""
    echo "set \"EXE=$EXE\""
    echo "set \"BIN=$(win "$BIN")\""
    echo "set \"INSTALLED=0.1.9\""
    echo 'call :update_check'
    echo 'echo HARNESS_DONE'
    echo 'goto :eof'
    echo ''
    extract_update_check
    echo ''
    echo ':start_miners'
    echo 'echo [stub] start_miners called'
    echo 'goto :eof'
    echo ''
    echo ':run_crash_hook'
    echo 'goto :eof'
  } > "$HB"

  # Env the curl shim reads; SHA256SUMS env points at a missing file in (d) so
  # the shim returns 1 (== 404), exactly the curl -f failure the code expects.
  export CSDH_LATEST="$(win "$SB/latest")"
  export CSDH_SUMS="$(win "$SB/SHA256SUMS")"
  export CSDH_ASSET="$(win "$SB/asset_payload")"

  OUT="$(cd "$SB" && MSYS_NO_PATHCONV=1 PATH="$SHIMDIR:$PATH" cmd.exe /c "$(win "$HB")" 2>&1)"
  echo "----- scenario ($scen) output -----"
  printf '%s\n' "$OUT" | sed 's/^/    | /'

  FINAL="$(cat "$BIN" 2>/dev/null)"
  if [ "$scen" = "c" ]; then
    if printf '%s' "$FINAL" | grep -qF "$NEWCONTENT" && ! printf '%s' "$FINAL" | grep -qF "OLD-PRE-VERIFY-FILE-BINARY"; then
      ok "(c) pre-verify-file binary + valid SHA256SUMS: Get-FileHash verified and SWAPPED in the new binary"
    else
      bad "(c) expected a swap to the new binary; %BIN% content did not change to the new payload"
    fi
    printf '%s\n' "$OUT" | grep -q "now mining 0.1.99" \
      && ok "(c) reached the post-swap 'now mining 0.1.99' path (no longer frozen)" \
      || bad "(c) did not reach the post-swap success path"
    [ -z "$(ls "$DIR"/*.new 2>/dev/null)" ] && ok "(c) no leftover .new temp after swap" || bad "(c) leftover .new temp"
  else
    if printf '%s' "$FINAL" | grep -qF "OLD-PRE-VERIFY-FILE-BINARY"; then
      ok "(d) no SHA256SUMS: %BIN% UNCHANGED — update refused (fail-closed)"
    else
      bad "(d) %BIN% changed despite missing SHA256SUMS — fail-OPEN regression!"
    fi
    printf '%s\n' "$OUT" | grep -qi "refusing unverified update: no SHA256SUMS" \
      && ok "(d) emitted the fail-closed refusal message" \
      || bad "(d) did not emit the no-SHA256SUMS refusal"
    [ -z "$(ls "$DIR"/*.new 2>/dev/null)" ] && ok "(d) staged .new temp was discarded" || bad "(d) leftover .new temp after refusal"
  fi
  rm -rf "$SB"
}

echo "================ (c) pre-verify-file + valid SHA256SUMS → Get-FileHash verifies + swaps ================"
run_scenario c
echo
echo "================ (d) NO SHA256SUMS → still refuses (fail-closed) ================"
run_scenario d
echo
rm -rf "$SHIMDIR"
if [ "$fails" -eq 0 ]; then echo "ALL mine-auto.bat HARNESS ASSERTIONS PASSED"; exit 0
else echo "mine-auto.bat HARNESS FAILURES: $fails"; exit 1; fi
