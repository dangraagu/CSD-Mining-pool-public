#!/usr/bin/env bash
# Docs-vs-clap drift guard.
#
# WHY THIS EXISTS: v0.2.4's README shipped an "Opt out" section documenting
# `--no-gpu-model` / `CSD_NO_GPU_MODEL=1` — a PLACEHOLDER flag that did not
# exist in src/main.rs. clap would have rejected it, so every user who followed
# the README would have hit "unexpected argument" and had their miner refuse to
# start. It was caught only because a human left a RELEASE BLOCKER comment
# next to it. docs/RELEASE-CHECKLIST.md calls this class of drift "silent"
# precisely because nothing machine-enforced it. Now something does.
#
# Asserts: every `--long-flag` mentioned in README.md / docs/HIVEOS.md is either
#   (a) a real clap long flag on the miner's Cli/subcommands, or
#   (b) on the explicit NON_MINER allowlist below (flags belonging to OTHER
#       tools the docs legitimately show: cargo, nvidia-smi, apt, and the
#       deliberately-REJECTED endpoint-override flags).
#
# Purely static: greps source, builds nothing, needs no toolchain.
set -uo pipefail

REPO="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
MAIN="$REPO/src/main.rs"
DOCS=("$REPO/README.md" "$REPO/docs/HIVEOS.md")

fail=0
note() { printf '%s\n' "$*"; }

# --- flags that belong to other tools, not to the miner ---------------------
# Each MUST carry a reason; an unexplained entry here would defeat the guard.
NON_MINER=(
  --features   # cargo build --features cuda
  --release    # cargo build --release
  --format     # nvidia-smi --format=csv
  --query-gpu  # nvidia-smi --query-gpu=...
  --once       # systemd-run / watch-style examples
  --now        # systemctl enable --now
  --refresh    # hive/systemctl style refresh examples
  --update     # apt-get update / selfupdate examples
  --port       # generic port examples in prose (not a miner flag)
  --pool       # DELIBERATELY REJECTED: endpoint lock (tests/endpoint_locked)
)

is_allowlisted() {
  local f="$1" a
  for a in "${NON_MINER[@]}"; do [ "$f" = "$a" ] && return 0; done
  return 1
}

# --- 1. the miner's REAL long flags, straight from the clap definitions -----
# Two sources: an explicit `long = "name"`, and the far more common bare
# `#[arg(long)]` whose flag name is the NEXT field identifier, snake->kebab.
real_flags="$(
  {
    grep -oE 'long[[:space:]]*=[[:space:]]*"[a-z0-9-]+"' "$MAIN" \
      | sed -E 's/.*"([a-z0-9-]+)"/\1/'
    awk '
      /#\[arg\(/ && /long/ { want=1; next }
      want && match($0, /^[[:space:]]*(pub[[:space:]]+)?[a-z_][a-z0-9_]*[[:space:]]*:/) {
        line=$0
        sub(/^[[:space:]]*/, "", line)
        sub(/^pub[[:space:]]+/, "", line)
        sub(/[[:space:]]*:.*$/, "", line)
        gsub(/_/, "-", line)
        print line
        want=0
      }
    ' "$MAIN"
  } | sort -u
)"

if [ -z "$real_flags" ]; then
  note "FAIL: extracted ZERO clap flags from $MAIN — the parser broke, not the docs."
  exit 1
fi

n_real="$(printf '%s\n' "$real_flags" | grep -c .)"
if [ "$n_real" -lt 20 ]; then
  note "FAIL: only $n_real clap flags extracted; expected >=20. Parser regression."
  exit 1
fi

# --- 2. every long flag the docs tell a user to type ------------------------
doc_flags="$(grep -ohE '\-\-[a-z0-9][a-z0-9-]+' "${DOCS[@]}" | sort -u)"

for f in $doc_flags; do
  bare="${f#--}"
  if printf '%s\n' "$real_flags" | grep -qxF "$bare"; then
    continue
  fi
  if is_allowlisted "$f"; then
    continue
  fi
  note "FAIL: docs mention '$f' but no such clap flag exists in src/main.rs"
  note "      (if it belongs to another tool, add it to NON_MINER with a reason)"
  fail=1
done

# --- 3. the specific placeholder that nearly shipped ------------------------
# A named regression test: this exact string must never reappear in the docs.
for ghost in --no-gpu-model CSD_NO_GPU_MODEL; do
  if grep -qF -- "$ghost" "${DOCS[@]}"; then
    note "FAIL: the v0.2.4 placeholder '$ghost' is back in the docs."
    note "      The implemented flag is --no-hardware-report / CSD_NO_HARDWARE_REPORT."
    fail=1
  fi
done

# --- 4. the opt-out must stay documented ------------------------------------
# Reporting hardware without a documented way to decline is the thing we
# promised not to do, so the docs losing this section is a release-blocker.
if ! grep -qF -- "--no-hardware-report" "$REPO/README.md"; then
  note "FAIL: README.md no longer documents --no-hardware-report."
  note "      A public miner must document how to decline hardware reporting."
  fail=1
fi
if ! grep -qF -- "CSD_NO_HARDWARE_REPORT" "$REPO/README.md"; then
  note "FAIL: README.md no longer documents the CSD_NO_HARDWARE_REPORT env form."
  fail=1
fi

if [ "$fail" -eq 0 ]; then
  note "PASS: docs_flags_exist ($n_real clap flags known; $(printf '%s\n' "$doc_flags" | grep -c .) doc flags checked)"
fi
exit "$fail"
