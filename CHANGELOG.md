# Changelog

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
