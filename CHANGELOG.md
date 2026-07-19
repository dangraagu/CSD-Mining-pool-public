# Changelog

## v0.2.4 — 2026-07-19 — whole-rig HiveOS stats + GPU-model reporting

- **HiveOS multi-GPU stats: a rig now reports the WHOLE rig, not just GPU 0.**
  The miner is one-process-one-device and `h-run.sh` puts each extra card on
  `PORT+1, PORT+2, …`, but `h-stats.sh` scraped only `$CUSTOM_API_PORT` — so an
  N-card rig under-reported by ~1/N (a 6-GPU rig showed **1.64 GH/s against
  11.68 GH/s of accepted shares**). It now TCP-probes each port, scrapes every
  live one, and merges the objects into a single payload with one `hs[]` element
  **per card**; `ar[]` is summed, `uptime` is the max, `temp[]` stays positional
  (a dead card's slot is `0`, and `temp[]` stays empty when no card reports).
  **This is an uptime bug, not a cosmetic one:** HiveOS's own hashrate watchdog
  compares the reported rate against a threshold, so a healthy multi-GPU rig
  could be killed and restarted in a loop. A 1-GPU rig's output is
  byte-identical to before.
- **The port scan is bounded by the real device count, not a blind 32.** The
  scan probes `nvidia-smi`'s device count (mirroring `h-run.sh:hive_gpu_count`),
  which is a complete bound — `h-run.sh` launches device `d` only when
  `d < gpu_count` and places it on `PORT+d`, so every live stats port is in
  `[PORT, PORT+gpu_count-1]`, including the sparse `--gpu-id 0,8` case. Measured
  end-to-end: all-dead **279 ms → 25 ms**, 6 live **132 ms → 45 ms**. At 32 ports
  the probe consumed ~93% of the budget against a ~10 s HiveOS poll, which broke
  the scrape loop on iteration 1 and silently fell back to scraping `$PORT`
  alone — re-entering the very under-report this change fixes. The budget is now
  split so the probe cannot starve the scrape (`HS_PROBE_BUDGET`), and **the
  single-port fallback LOGS**, so that regression can never again be silent. If
  no device count is available the legacy 32-port bound is kept — degraded, but
  never narrower than before.
- **GPU model reported in the `mining.subscribe` user-agent.** The miner appends
  its normalized device name, e.g. `csd-gpu-miner/0.2.4 (RTX 5070 Ti)`, so the
  pool dashboard can show per-card hashrate expectations instead of hand-
  maintained guesses.
- **`--no-hardware-report` opts out of that.** The flag suppresses the model
  suffix and sends the plain `csd-gpu-miner/<ver>` user-agent. Payout address,
  share submission and PoW are unaffected either way — the model is display-only
  telemetry.
- **`h-config.sh` strips an operator-supplied `--device` from `EXTRA_ARGS`.** The
  per-GPU sidecars set their own device, so a stray `--device` in the flightsheet
  pinned **every** sidecar to the same card.
- **`CUSTOM_VERSION` is stamped into the staged HiveOS manifest from the release
  tag** (fail-closed if the sed matches nothing), so a published tarball always
  self-describes correctly — it shipped `0.2.1` through the entire 0.2.2/0.2.3
  cycle. The checked-in value is locked equal to the `Cargo.toml` version by
  `tests/hiveos_seed_variant.sh`.
- **No consensus / PoW / wire-format change.** The kernel, nonce search, digest
  and target comparison are byte-identical to v0.2.3; the stats merge is pure
  aggregation of values the Rust `hiveos-stats` subcommand already converted to
  kH/s, so no unit maths moved into shell.

## v0.2.3 — 2026-07-15 — inner-prefix precompute (+~5.9%) + telemetry on packaged rigs

- **Faster CUDA SHA-256d kernel: inner-block prefix precompute (~+5.9% hashrate,
  PoW byte-identical).** The second SHA-256 block's working vars after rounds 0–3
  plus the three constant schedule words (`w16..w18`) are precomputed once per
  template on the host; the kernel resumes the inner compress from round 4 instead
  of round 0. Only the nonce word varies per hash, so the result is bit-exact —
  proven by a CPU oracle (`inner_prefix_matches_full_compress`, 2000 random
  midstate/tail/nonce) and the GPU selftest (re-hash every found nonce on CPU,
  assert equal) on **both** the native sm_120 path and the compute_75 PTX-JIT
  fallback. Benched **4057.8 vs 3832.8 MH/s (+5.87%)** on an RTX 5070 Ti; a
  105-min live-pool canary accepted **89 shares, 0 stale, 0 reconnects** (3
  timing-rejects). No consensus/PoW/protocol change — the shipped hash is
  identical to v0.2.2, just computed faster.
- **Localhost telemetry enabled on packaged systemd rigs.** The xmrig-compatible
  `/1/summary` endpoint (already shipped in v0.2.2) is now on by default on
  packaged (non-HiveOS) rigs via `Environment=CSD_STATS_PORT=3380`, bound to
  `127.0.0.1` (not reachable off-box, no token needed). No binary change; the
  unit hardcodes the port so a rig with no override still starts.

## v0.2.2 — 2026-07-11 — job-change preemption

- **job-change preemption — abort in-flight sweeps on a new block (fewer stale
  shares).** A session-lived `new_job` cancel flag is threaded into
  `MiningBackend::hash_range` and OR'd into every backend's EXISTING
  between-launch abort check (cuda / opencl / cpu). The stratum reader sets it
  the instant a `mining.notify` arrives on a CHANGED prev-hash (a genuine new
  block) — the only writer that runs while the mining thread is blocked inside
  an in-flight sweep — so the GPU/CPU abandons now-stale work within one
  between-launch step instead of grinding the old prev-hash to the end of the
  slice. The loop clears the flag when it picks up the fresh job.
- **PoW-safe / control-flow only.** The kernel (fatbin/PTX/CPU inner loop) is
  untouched; the flag is polled ONLY at the between-launch boundary, never in the
  hash inner loop — every hash, nonce, digest and target-compare stays
  byte-identical to v0.2.1. Preemption changes only WHEN a sweep stops, never
  WHAT it computes. Bench and selftest pass an always-false flag so they stay
  deterministic / uninterrupted.
- **Preempts on a changed prev-hash only (a new block), never on a same-prev
  refresh** — the bridge re-publishes a fresh `job_id` (with `clean_jobs=true`)
  for ntime/template drift on an UNCHANGED prev; that work is still node-valid,
  so preempting it would drop a legit in-flight share for zero staleness benefit.
  (`clean_jobs` is therefore NOT a sufficient gate — the bridge sets it on
  same-prev refreshes too.)

## v0.2.1 — 2026-07-02 — Linux compatibility hotfix

- **Linux binaries are now built portable to glibc ≥ 2.27** (via `cargo zigbuild
  --target x86_64-unknown-linux-gnu.2.27`), so they run on stock HiveOS images
  again. v0.2.0's Linux binaries were built on GitHub's ubuntu-24.04 runners
  against glibc 2.39 and crash-looped on HiveOS rigs (glibc 2.27/2.31) with
  `/lib/x86_64-linux-gnu/libc.so.6: version 'GLIBC_2.39' not found` (exit 1,
  relaunch loop).
- **Binary logic is unchanged from v0.2.0** — same code, same features per
  variant (cpu / cuda+nvml / opencl); only the Linux build toolchain changed.
- **New permanent CI gate ("glibc floor gate"):** the release workflow now runs
  an objdump max-`GLIBC_*` check over every `csd-pool-miner-linux-*` asset and
  hard-fails if any requires > 2.27 — fail-closed against future runner
  migrations. (The bundled prebuilt `csd-relay-node` is fetched, not built here,
  and is a known separate issue — not covered by this gate.)
- **Windows rigs are unaffected** (they were never broken; v0.2.0 Windows
  binaries mine fine).

## v0.2.0 — 2026-07-02 — the refactor release

### ⚠️ Behavior changes (operator-visible)

- **No silent CPU fallback — the default backend is now per build variant.**
  Each build defaults to its own compiled backend: the nvidia build defaults to
  `--backend cuda`, the amd build to `--backend opencl`, and the cpu build to
  `--backend cpu`. No build silently falls back to CPU: a GPU variant whose GPU
  API fails to init HARD-ERRORS at startup and mines nothing until the
  GPU/driver is fixed (previously `--backend auto` could quietly degrade to CPU
  mining at kH/s — invisible hashrate bleed); the cpu build's purpose IS CPU
  mining, so its `cpu` default is deliberate, not a fallback. Deliberate CPU
  mining on a GPU build still works: pass `--backend cpu` (explicit) or
  `--backend auto --allow-cpu-fallback` (permits the auto→CPU descent).
  Healthy GPU rigs are unaffected. HiveOS `h-run.sh` driver-check warnings
  updated to match.
- **Auto-tune runs at every session start (default ON).** The full geometry sweep
  (~10 candidates × 5 s ≈ 50 s, no shares submitted during it) now runs before
  mining on every start, so each rig always mines on its measured-best geometry —
  worth ~+7% on large cards vs the old fixed default (e.g. RTX 5070 Ti:
  560×256×4096 ≈ 4.06 GH/s → 2048×256×1024+ ≈ 4.4–4.6 GH/s). Opt out with
  `--no-auto-tune`, or pin `--blocks/--threads-per-block/--nonces-per-thread`
  explicitly (an explicit geometry also suppresses the sweep). If a sweep fails,
  the miner falls back to the last-known-good cached geometry, then the shipped
  default — never a crash, never CPU.

### Performance

- **Multi-arch CUDA fatbin: native Blackwell (sm_120) SASS + universal
  `compute_75` PTX fallback.** The embedded kernel is now a fatbin containing
  exactly two images: a native `sm_120` cubin (`-maxrregcount=128`, ~+3–6% on
  RTX 50-series over the JIT path) and the byte-identical, production-proven
  `compute_75` PTX pinned at `.version 6.3`. The CUDA driver picks the native
  image on Blackwell and JITs the PTX everywhere else — **every pre-Blackwell
  card (e.g. RTX 2080) runs the exact same kernel image as v0.1.x**, and future
  >sm_120 cards JIT the PTX too. No PTX with `.version > 6.3` is embedded, so
  the old-driver `CUDA_ERROR_UNSUPPORTED_PTX_VERSION` → silent-CPU trap cannot
  fire. Bit-exact gated: `selftest` proves GPU==CPU sha256d on BOTH images
  (native sm_120 and forced `compute_75` JIT) before any release. Built by
  `scripts/build-fatbin.bat`; the fatbin is a committed artifact so CI/release
  builds embed the exact verified bytes (no nvcc needed at CI time).
  Residual note: the `compute_75` fallback is bit-exact-proven via forced JIT on
  Blackwell (CUDA 13.2 driver); a live accepted-share run on physical Turing
  hardware with an older driver has not yet been observed on this build.
- Auto-tune candidate set gains large-card geometries `4096×256×512`
  (5070 Ti winner, 4.60 GH/s) alongside `2048×256×1024`.

- launcher auto-restart on self-update — mine-auto.sh now re-execs the refreshed
  launcher (staged handoff, fail-safe, no-brick) so a launcher/script update takes
  effect without a manual restart (fixes the observed update-but-run-old bug). The
  on-disk swap stays exec-free; a separate, multiply-guarded `reexec_new_launcher`
  step applies it only on a real byte change (rc=0) — every guard (real-file only,
  swap-time SHA re-verify, `bash -n`, bounded generation cap, execfail recovery)
  fails safe to "keep running the old in-memory launcher, miners keep mining". [M4]
- GPU device errors now surface (`hash_range` → `Err(DeviceError)`) and trigger
  in-process recovery + the GPU watchdog (heartbeat starved, no phantom hashrate
  from faulted sweeps) — was: silently mapped to the same `None` as a clean empty
  sweep, so a fast-failing GPU looked alive while mining nothing. [IMP-1b]

## 0.1.19

HiveOS + Windows: auto-detect the GPU and fetch the matching build when `--backend`
is omitted or `auto`. No pool, payout, or csd consensus change. (Binary logic
unchanged; version bumped so the auto-updater converges on the new tag.)

### Added
- **Multi-GPU HiveOS rigs now mine every card.** The miner is one-process-per-GPU,
  but `h-run.sh` only launched one process (one card). It now spawns one background
  process per extra GPU (each its own `--device i` + stats port) immediately before
  the unchanged device-0 `exec`. Brick-safe by construction: the device-0 `exec` is
  the guaranteed last action, and the multi-GPU launch is fully fail-soft (timeout-
  guarded probes, no error/exit path) — any failure (no `nvidia-smi`, wedged driver,
  spawn error, CPU variant, <2 GPUs) falls back to the single-GPU start = the prior
  behaviour, never a brick. Honours an operator `--gpu-id` include-list; CPU and
  single-GPU rigs are unaffected. `h-stop.sh` already reaps all per-GPU processes.

### Fixed
- **"Connected but 0 H/s" on HiveOS when `--backend` was omitted/`auto`/`--backend=cuda`.**
  `h-run.sh`'s `update_variant()` matched only the space-form `--backend cuda|opencl|cpu`,
  so every other case defaulted to the CPU seed → "no GPU backend usable". It now lifts
  the proven `detect_variant()` (nvidia-smi / device-node / libcuda → nvidia; lspci/clinfo
  → amd; else cpu) and honours the `--backend=` equals-form, so an omitted/`auto` backend
  fetches the correct GPU build. `detect_variant` returns cpu when no GPU is found, so the
  asset name is never empty (brick-safe); its external probes are `timeout`-guarded. The
  GPU-driver warning now derives from the resolved variant. `--backend` is now OPTIONAL.
- **Windows launchers had the same trap** — `mine-auto.bat` / `mine-all-gpus.bat`
  hard-defaulted to the `amd` build, so a bare run on an NVIDIA box fetched the opencl
  build. They now use the same Win32_VideoController probe as the installer, and
  normalize+validate the payout address (lowercase, strip `0x`/`.worker`, 40-hex) before
  saving it.
- **HiveOS quoted address** — a pasted `--address "<40hex>"` (or quoted wallet field) is
  now accepted (a matched quote pair is stripped); an unrecognised `--backend` token warns.

### Internal
- New `tests/hiveos_flow_e2e.sh` replays HiveOS's real two-call sequence (h-config WITH
  env → h-run env-less) and asserts the resolved (address, variant, exec-argv) — it fails
  on the old code and passes on the fix. New `tests/win_launcher_parity.sh`. The release
  workflow now runs the full shell suite as a required gate before packaging.

## 0.1.18

P0 HiveOS hotfix — a fresh v0.1.17 HiveOS install (or any HiveOS restart) blanked
its own config and refused to start. No pool, payout, or csd consensus change.

### Fixed
- **HiveOS config.toml + extra-flags were blanked on launch → `--address must be 40
  hex chars; got 0 chars`, and the miner fell back to the CPU build.** HiveOS runs
  `h-config.sh` *with* the flight-sheet env to bake the config, then runs `h-run.sh`,
  which **does not** receive `$CUSTOM_USER_CONFIG` / `$CUSTOM_TEMPLATE`. `h-run.sh`
  re-ran `h-config.sh` unconditionally, so that env-less second run overwrote the
  baked `config.toml` (address → empty) and `extra-flags` (dropped `--backend` →
  variant-aware updater fetched the CPU build). One cause, both symptoms. Fix:
  `h-run.sh` now re-renders only when `config.toml` lacks a valid 40-hex address, and
  `h-config.sh` is idempotent — an env-less call recovers the baked address and
  preserves the existing `extra-flags` instead of blanking them. Regression test
  reproduces the two-call / stripped-env flow (fails before, passes after).

## 0.1.17

HiveOS install fixes — a fresh HiveOS rig now installs, gets its **correct GPU
build**, and mines with zero manual steps. No pool, payout, or csd consensus change.

### Fixed
- **Fresh HiveOS installs ran the WRONG GPU binary.** The release bundled whatever
  variant built LAST in CI (AMD/opencl — the NVIDIA/AMD builds overwrite the same
  output path), and `h-run.sh`'s startup auto-update only swapped on a *version*
  change, so an NVIDIA rig with `--backend cuda` stayed stuck on the opencl binary
  (`cuda=false opencl=true`). The tarball now bundles the **CPU build** as a
  brick-safe universal seed (runs on any card, no driver dep) plus a
  `.installed-variant` marker, and `h-run.sh` is **variant-aware**: it detects the
  installed variant from the binary's own `devices` self-report (marker fallback)
  and fetches+verifies+swaps the build matching `--backend` even when the version
  already matches. Fail-closed SHA-verified; any fetch/verify failure keeps the
  working CPU seed mining (never bricks).
- **HiveOS address could not be set.** HiveOS does not pass the "Wallet and worker
  template" field for a coinless custom miner (CSD), so the old documented path left
  the address empty and the miner refused to start. `h-config.sh` now takes the
  address from `--address <addr>` in the "Extra config arguments" box, stripping a
  `0x`/`0X` prefix and a `.worker` suffix; docs updated to use this path.
- **SP2 relay was missing from the HiveOS tarball** — staged to a directory the
  package step never packed (`csd-pool-miner/` vs the renamed `csdpool/`). Now
  staged into `csdpool/`, so the bundled canonical-anchor relay ships again.

### Notes
- Existing HiveOS rigs already on the wrong variant self-heal after a **one-time
  Flight Sheet re-apply** (the auto-updater swaps the binary; the install-time glue
  comes from the tarball). New installs are correct automatically.

## 0.1.16

Live terminal dashboard (script-only) **plus** a miner-only fix to the startup
suggest-difficulty benchmark. No pool, payout, or csd consensus change.

### Fixed
- **Startup benchmark under-reported GPU hashrate ~67×** (`src/bench.rs`) — the
  old loop swept only `CHUNK_NONCES` (200k) per `hash_range` call, far too small
  to fill even one GPU launch, so it measured per-call setup overhead instead of
  throughput and derived a difficulty floored to ~diff-1. It now sweeps the full
  `[0, u32::MAX)` saturating geometry per call (matching the proven mining path),
  so a fast rig's `mining.suggest_difficulty` starts near its true rate instead of
  ramping from diff-8. Fully fail-safe: any benchmark error/timeout/panic still
  returns `None` ⇒ the rig mines normally.
- **Suggested-difficulty over-read is now capped** (`guarded_suggestion`, new
  `MAX_SUGGEST_DIFFICULTY = 100_000`) — a backend instant-`None` error path could
  count phantom nonce sweeps and derive a diff of order 1e6; such an over-read is
  now **rejected** (not clamped), so the rig falls back to the pool default +
  vardiff rather than being handed near-unsolvable work. 100k is ~100× above any
  real single-worker rate, so a legitimate suggestion is never rejected.

### Added
- **`csd-dashboard.sh` (Linux/macOS/HiveOS) and `csd-dashboard.bat` (Windows)** —
  a live, refreshing terminal view of a running miner: hashrate (10s / 1m / 15m),
  accepted / rejected / stale shares with reject%, GPU temp + power (when the
  miner reports them), and reconnects / failovers. It only GETs the miner's own
  xmrig-`/1/summary` endpoint over localhost — it never writes config, never
  touches the share/submit path, and never opens a non-loopback socket; worst
  case it prints "endpoint unreachable". The Linux script needs **no `jq`** (dual
  jq / pure-`sed` parse) so it runs on a stock HiveOS shell. Flags: `--port`,
  `--refresh`, `--once`, `--no-color`, `--update`. Self-updates with `--update`
  (fail-closed SHA-verify against the release `SHA256SUMS`, same as the
  launchers). Bundled into the HiveOS tarball and published as a release asset.



**Launcher / installer / docs hardening — no binary or consensus change.** All
fixes are brick-safe: any failure leaves the rig mining on the binary it already
has, and the always-on relay helper is unchanged.

### Fixed
- **`curl | bash` no longer aborts under `set -u`** — `install-csd-miner.sh` and
  `create-wallet.sh` computed `SCRIPT_DIR` from `${BASH_SOURCE[0]}`, which is
  unset when the script is delivered over a pipe (`$0` is `bash`, `BASH_SOURCE`
  is empty). Under `set -euo pipefail` that tripped an "unbound variable" abort
  before anything ran. Now `${BASH_SOURCE[0]:-$0}`.
- **NVIDIA driver-only / container rigs are detected as `nvidia`** — a shared
  `detect_variant()` (identical in `install-csd-miner.sh`, `mine-auto.sh`,
  `mine-all-gpus.sh`) returns `nvidia` when **any** of: `nvidia-smi` runs, an
  NVIDIA device node (`/dev/nvidiactl` or `/dev/nvidia*`) exists, or `ldconfig`
  lists `libcuda.so`. Previously a container with passed-through device nodes but
  no `nvidia-smi` was mis-detected as `amd`/`cpu` and ran the wrong build.
- **No more hard `amd` default** — `mine-auto.sh` / `mine-all-gpus.sh` launched
  with no build arg defaulted to `amd` (running the amd build on NVIDIA rigs).
  They now call `detect_variant()` for the no-arg case.
- **Crash reason is surfaced, not swallowed** — per-GPU stdout is now appended
  (`>>`) so a crash message survives restarts instead of being overwritten, and
  the "miners not running — restarting" path tails the newest
  `gpu*-log/stdout.log` and prints an actionable hint naming the running build and
  the `nvidia | amd | cpu` re-run options. Mirrored in `mine-auto.bat`.
  (Message-only — the launcher never silently swaps which build it runs.)

### Added
- **First-run banner** — `mine-auto.sh` / `mine-auto.bat` now print the selected
  build and the per-GPU log path on start, so you can confirm what is running and
  find the logs immediately.

### Docs
- README: the primary Linux install is now the address-included one-liner
  (`curl … | CSD_ADDR=<addr> bash`), and the ad-hoc run examples use the real
  installed path `~/.local/share/csd-pool-miner/csd-pool-miner-linux-<variant>`
  (which is **not** on `PATH`) instead of a bare `csd-pool-miner`.

## 0.1.14

**Launcher robustness.** `download()` staging in the launchers was hardened
against symlink-follow on the temp path (a local-attacker edge), and the relay
helper's user-facing messages were softened so an absent/blocked relay reads as
informational rather than an error. The README Discord invite was pointed at the
current server. No miner-binary behaviour change.

## 0.1.13

**Standalone relay auto-install.** The launchers can now best-effort download,
SHA-verify, and place a trusted `csd-relay-node` when one is not already on disk,
so a standalone rig gets the relay helper without a manual step. Fail-closed and
fully decoupled from hashing — a failed or blocked relay install can never abort
or delay the miner launch.

## 0.1.12

**GPU telemetry + thermal safety + Windows service (all opt-in / gated).**
Folded into the fleet variants behind cargo features so the default and
`linux-cpu` builds stay lean.

### Added
- **NVML GPU telemetry + thermal pause** (`nvml` feature) — reads real GPU
  temperature/power and pauses hashing above a configurable limit (with
  hysteresis) so a hot card backs off instead of cooking. `gpu_temp_c` /
  `gpu_power_w` are exposed in the stats endpoint and the HiveOS `temp[]`.
- **Windows-Service mode** (`winsvc` feature) — run the miner as a Windows
  service (install/start/stop/uninstall), so a rig keeps mining across logouts.

## 0.1.11

**Launcher self-update (brick-safe) + relay launch fix.**

### Fixed
- **Launcher can update itself** — `mine-auto` swapped only the miner *binary*, so
  a fix to the launcher script never reached a rig running the on-disk launcher.
  `update_launcher_self` now refreshes the launcher too: **fail-closed** (any
  download/verify failure keeps the old launcher byte-for-byte) and **no-brick**
  (atomic on-disk replace via a startup trampoline, never a mid-run re-exec).
- **Relay launch fix** — corrected the standalone relay launch path so the relay
  helper starts cleanly alongside the miner.

## 0.1.10

**Self-update semver correctness.** The auto-updater compared versions with a
string `!=`, which mis-ordered `0.1.10` vs `0.1.9`. Updates are now decided by a
numeric semver compare (the miner's own `check-update`), so a higher patch
release is correctly recognised as newer. The download is still SHA-256 verified
against the release `SHA256SUMS` before any atomic swap, and the updater still
fails closed.

## 0.1.9

**Bundled canonical-follow relay node.** This release ships a background
`csd-relay-node` alongside the miner. The relay follows the canonical chain and
relays valid tips to its peers, improving block/transaction propagation for the
pool's network. It is strictly a good-citizen network helper and is decoupled
from hashing.

### Added
- **Background relay node** — the launchers (`hiveos/h-run.sh`, `mine-auto.sh`,
  `mine-auto.bat`) start `csd-relay-node` before the miner. It is **resource-capped**
  so it never competes with the GPU miner: `nice -n 19` + `ionice -c 3` (idle CPU
  and I/O class) on Linux/HiveOS, and `/LOW /B` (below-normal priority, detached)
  on Windows. The relay is launched as a separate process and **never blocks or
  delays hashing** — if the relay binary is absent the miner runs exactly as
  before.
- **Canonical-follow + relay** — the relay node follows the canonical tip and
  relays valid headers/tips to its peers, **except** tips from blacklisted
  addresses, which it refuses to follow or propagate.
- **Signed-blacklist auto-pull** — the relay node periodically (every ~15 min)
  fetches an Ed25519-signed address blacklist, verifies the signature, and
  writes the verified `addr20` list **fail-closed** (a missing or
  signature-invalid list never widens what the node will follow). Enabled by
  pointing `CSD_BLACKLIST_URL` at the published signed list.

The miner's pool share/submit path is unchanged and byte-for-byte compatible
with earlier builds; the relay node is an additive, best-effort network helper.

## 0.1.7

**Official-pool-only, build hardening, and relicensing.** This release:

- **Hardcodes the official CSD pool endpoint** — the miner connects only to the
  official pool; there is no endpoint/server flag to configure.
- **Build hardening** — build-host paths are scrubbed from the release binaries.
- **Relicensed to the PolyForm Perimeter License 1.0.0** (see `LICENSE`). The
  relicensing is **forward-only**: v0.1.6 and earlier stay under MIT OR Apache-2.0
  and cannot be clawed back; only v0.1.7+ carry the new terms.

Everything else is automatic or opt-in; a plain `--address <addr>` run mines to
the CSD pool exactly as before, and the pool share/submit path is byte-for-byte
compatible with earlier builds.

### Added
- **Stats endpoint** — `--stats-port <port>` serves an xmrig
  `/1/summary`-compatible JSON endpoint (plus `/healthz`) for dashboards
  (Awesome Miner, Home Assistant, custom scrapers). Binds `127.0.0.1` only by
  default; `--stats-bind` to expose on a LAN and `--stats-password` to protect it
  (`Authorization: Bearer <tok>` or `?token=`). The `connection` object also
  reports `reconnects` + `failovers` so you can spot an unstable link.
- **Discord alerts** — `--discord-webhook <url>` pings on an accepted-share
  milestone. Best-effort and non-blocking — a slow/dead webhook never affects
  mining. HTTPS-Discord URLs only.
- **HiveOS package** — a Custom-miner package (`hiveos/`, shipped as a release
  tarball) that reports hashrate + accepted/rejected to the HiveOS dashboard by
  scraping the miner's own stats endpoint.
- **Connection watchdog** — forces a clean reconnect to the official pool on a
  half-open link or a stalled job feed; and an accepted-share dead-man forces a
  fresh reconnect if the pool takes shares but stops crediting them for ~30 min
  (at most once per window). The 30-second health heartbeat ends with
  `conn=<reconnects>/<failovers>` so connection churn is visible at a glance.
- **Pool-reachability probe** — `selftest` also resolves + TCP-probes the pool
  endpoint (PASS/FAIL, non-fatal).
- **Device UX** — `--list-devices` (flag form of the `devices` subcommand) and
  `--gpu-id <list>` (e.g. `0,2,3`).
- **Hardened self-update** — `mine-auto` decides updates with a numeric semver
  compare, verifies the download's SHA-256 against the release `SHA256SUMS`
  **before** an atomic swap, and **fails closed** if it can't verify. New helper
  subcommands: `check-update`, `verify-file`, `hiveos-stats`.

### Changed
- **Fail-loud config validation** — nonsensical mining parameters (`--cpu-share`
  outside `0..=1`, `--threads 0`, or `--blocks` / `--threads-per-block` /
  `--nonces-per-thread` equal to 0) now error with a clear message before any
  socket opens, instead of being silently clamped (which hid typos like
  `--blocks 0`). Sensible-but-high values (e.g. a large `--cpu-threads`) still
  clamp, as before.

### Fixed
- Reconnect no longer storms on an idle link, and shutdown/teardown is prompt and
  bounded on every platform.
- A deliberate endpoint failover no longer instantly snaps back to a
  still-healthy primary (the failback clock starts on first use on a backup).
- Per-process coinbase seeding so co-located rigs don't duplicate work.
- Submit/heartbeat report real accepted/rejected/stale share accounting.
