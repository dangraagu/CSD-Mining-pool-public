# Changelog

## 0.1.8

**Solo mining, observability, and packaging.** Pool mining is unchanged and
byte-for-byte compatible with earlier builds — everything below is **opt-in and
off by default**, so a plain `--address <addr>` run behaves exactly as before.

### Added
- **Solo mode** — `--solo --node http://host:port` mines **directly to your own
  csd-node** (no pool, no fee, no PPLNS; every block you find is yours). Pulls
  work from `<node>/work/get` and submits solved blocks to `<node>/work/submit`.
  Mutually exclusive with the pool.
- **Stats endpoint** — `--stats-port <port>` serves an xmrig
  `/1/summary`-compatible JSON endpoint (plus `/healthz`) for dashboards
  (Awesome Miner, Home Assistant, custom scrapers). Binds `127.0.0.1` only by
  default; `--stats-bind` to expose on a LAN and `--stats-password` to protect it
  (`Authorization: Bearer <tok>` or `?token=`).
- **Discord alerts** — `--discord-webhook <url>` pings on a found block (solo) or
  an accepted-share milestone (pool); `--discord-solutions-only` limits it to
  solved blocks. Best-effort and non-blocking — a slow/dead webhook never affects
  mining. HTTPS-Discord URLs only.
- **HiveOS package** — a Custom-miner package (`hiveos/`, shipped as a release
  tarball) that reports hashrate + accepted/rejected to the HiveOS dashboard by
  scraping the miner's own stats endpoint.
- **Reliability** — pool failover (`--pool a:1,b:2`, alias `--url`), a connection
  watchdog, and a 30-second health heartbeat in the log.
- **Device UX** — `--list-devices` (flag form of the `devices` subcommand) and
  `--gpu-id <list>` (e.g. `0,2,3`).
- **Hardened self-update** — `mine-auto` now decides updates with a numeric
  semver compare, verifies the download's SHA-256 against the release
  `SHA256SUMS` **before** an atomic swap, and **fails closed** if it can't verify.
  New helper subcommands: `check-update`, `verify-file`, `hiveos-stats`.

### Fixed
- Reconnect no longer storms on an idle link, and shutdown/teardown is prompt and
  bounded on every platform.
- Per-process coinbase seeding so co-located rigs don't duplicate work.
- Submit/heartbeat now report real accepted/rejected/stale share accounting.
