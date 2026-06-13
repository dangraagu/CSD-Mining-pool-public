# Changelog

## 0.1.7

**Reliability, observability, and packaging.** Everything is automatic or opt-in;
a plain `--address <addr>` run mines to the CSD pool exactly as before, and the
pool share/submit path is byte-for-byte compatible with earlier builds.

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
- **Pool failover + watchdog** — `--pool a:3333,b:3333` (alias `--url`) tries
  endpoints in order, rotating to a backup on a dead connection and failing back
  to the primary; a connection watchdog forces a clean reconnect on a half-open
  link or a stalled job feed; and an accepted-share dead-man rotates off a pool
  that takes shares but stops crediting them for ~30 min (at most once per
  window). The 30-second health heartbeat ends with `conn=<reconnects>/<failovers>`
  so connection churn is visible at a glance.
- **Pool-reachability probe** — `selftest` also resolves + TCP-probes each pool
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
