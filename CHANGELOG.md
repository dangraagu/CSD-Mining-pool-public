# Changelog

## 0.1.15

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
