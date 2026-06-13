# Changelog

## 0.1.9

**Reliability hardening + connection observability.** Builds on 0.1.8; the pool
share/submit path is unchanged and byte-for-byte compatible, and everything here
is automatic or opt-in.

### Added
- **Endpoint failover on a non-crediting pool** — an accepted-share dead-man
  rotates to the next `--pool` endpoint when the current one keeps acking and
  pushing fresh work but stops crediting shares for ~30 minutes (a forked or
  misconfigured primary). Bounded to at most one rotation per window (no churn),
  and falls back to the primary after a quiet interval.
- **Connection-churn telemetry** — the `/1/summary` `connection` object and the
  INFO heartbeat now report `reconnects` and `failovers`, so a dashboard or
  operator can spot an unstable link or a flaky primary at a glance.
- **Live pool in telemetry** — the heartbeat and `/1/summary` now report the
  endpoint actually connected to (it tracks a failover) instead of the static
  configured primary.
- **Pool-reachability probe** — `selftest` now also resolves and TCP-probes each
  pool endpoint and prints PASS/FAIL per endpoint. Non-fatal (the exit code stays
  backend-correctness-only), so one command answers both "are the backends
  correct?" and "can I actually reach the pool?".

### Changed
- **Fail-loud config validation** — nonsensical mining parameters (`--cpu-share`
  outside `0..=1`, `--threads 0`, or `--blocks` / `--threads-per-block` /
  `--nonces-per-thread` equal to 0) now error with a clear message before any
  socket opens, instead of being silently clamped (which hid typos like
  `--blocks 0`). Sensible-but-high values (e.g. a large `--cpu-threads`) still
  clamp, as before.

### Fixed
- A deliberate endpoint failover no longer instantly snaps back to a
  still-healthy primary — the failback clock now starts on first use on a backup
  and holds it for a full interval.

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
