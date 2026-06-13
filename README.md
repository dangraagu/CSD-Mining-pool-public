# csd-pool-miner

A standalone miner for the **Compute Substrate (CSD)** network. Point it at your
payout address and it mines to the CSD pool — auto-detecting your GPU (NVIDIA or
AMD) or falling back to CPU.

It connects to the pool **by default**: there is no server/pool flag to set. The
only thing you have to provide is your addr20 (a 40-hex CSD payout address).

Discord channel for mining stats, updates and support/improvements:
https://discord.gg/Gr9gCjzC9e

## What this is — and what it isn't

This is **opt-in mining software you run on your own hardware.** Compute
Substrate (CSD) is a public proof-of-work blockchain; this repository is the
miner / pool client for it, maintained by the chain's operator.

- You choose to download and run it. It mines **only when you start it**, on
  **your own machine**, to **your own payout address** — nothing happens until
  you do.
- It is **not** silent, hidden, or self-spreading. There is no mechanism here to
  install or run it on anyone else's computer, and nothing in this repo accesses
  systems you don't control.
- It is standard cryptocurrency-mining infrastructure (a Stratum pool client) —
  the same category as the miner for any public proof-of-work coin.
- The source is open and auditable, and the chain is public:
  site <https://computesubstrate.org> · explorer <https://explorer.computesubstrate.org>.

## Install (Windows — one click)

Easiest path — no toolchain, no manual download:

1. Download **`install-csd-miner.bat`** from this repo (or from a release).
2. Double-click it.

It auto-detects your GPU (NVIDIA / AMD, else CPU), installs the VC++ runtime via
`winget` if needed, downloads the matching prebuilt binary from the latest
GitHub Release, asks for your addr20 payout address the first time (and remembers
it), then starts mining. To force a build: `install-csd-miner.bat nvidia|amd|cpu`.

Prefer to run the binary yourself? Grab the matching
`csd-pool-miner-<nvidia|amd|cpu>.exe` from
[Releases](https://github.com/dangraagu/CSD-Mining-pool-public/releases/latest)
and see [Quick start](#quick-start).

## Install (Ubuntu / Linux — one command)

```sh
curl -fsSL https://raw.githubusercontent.com/dangraagu/CSD-Mining-pool-public/main/install-csd-miner.sh | bash
```

Auto-detects your GPU (NVIDIA / AMD, else CPU), downloads the matching prebuilt
binary from the latest GitHub Release, asks for your addr20 the first time (and
remembers it under `~/.config/csd-pool-miner/`), then starts mining. Force a
variant by passing it through: `… | bash -s -- nvidia|amd|cpu`.

Piping into `bash` leaves no terminal to prompt on, so supply your addr20 in the
environment (or as the 2nd arg) the first time:

```sh
curl -fsSL https://raw.githubusercontent.com/dangraagu/CSD-Mining-pool-public/main/install-csd-miner.sh | CSD_ADDR=<YOUR_ADDR20> bash
```

Prefer to run it yourself? Download `csd-pool-miner-linux-<nvidia|amd|cpu>` from
[Releases](https://github.com/dangraagu/CSD-Mining-pool-public/releases/latest),
then `chmod +x csd-pool-miner-linux-<variant>` and run it with `--address
<YOUR_ADDR20>`. For 24/7 rigs, `mine-all-gpus.sh` (every card) and `mine-auto.sh`
(every card + auto-update) are fetched alongside the installer.

## Requirements

One of:

- **NVIDIA GPU** — a recent NVIDIA driver is enough. The CUDA backend ships a
  pre-built kernel (PTX) and JITs it through the driver, so **no CUDA Toolkit /
  nvrtc is required**. Use a build with the `cuda` feature.
- **AMD / other GPU** — an OpenCL driver/runtime for your card. Use a build with
  the `opencl` feature.
- **CPU only** — no GPU or driver required; works out of the box.

The default prebuilt binary is **CPU-only**. For GPU mining, use a release built
with the matching feature (see [Building](#building)).

## Quick start

```sh
csd-pool-miner --address <YOUR_ADDR20>
```

That's it. The miner will:

1. auto-detect the best backend (tries CUDA → OpenCL → CPU),
2. connect to the CSD pool,
3. start submitting shares for `<YOUR_ADDR20>`.

### Choosing a backend

Auto-detect is the default. To force one:

```sh
csd-pool-miner --address <YOUR_ADDR20> --backend auto    # default: cuda -> opencl -> cpu
csd-pool-miner --address <YOUR_ADDR20> --backend cuda     # NVIDIA
csd-pool-miner --address <YOUR_ADDR20> --backend opencl   # AMD / other
csd-pool-miner --address <YOUR_ADDR20> --backend cpu      # CPU only
```

### Useful extras

```sh
csd-pool-miner devices         # list detected GPUs (handy if auto keeps picking CPU)
csd-pool-miner --list-devices  # same list, as a flag
csd-pool-miner selftest        # cross-check every backend vs the reference CPU hasher + probe pool reachability
```

## Monitoring (stats endpoint + Discord)

Expose an **xmrig-`/1/summary`-compatible** JSON endpoint for dashboards
(Awesome Miner, Home Assistant, custom scrapers) — off unless you ask for it:

```sh
csd-pool-miner --address <ADDR> --stats-port 3380
# then GET  http://127.0.0.1:3380/1/summary   (and /healthz)
```

It binds **localhost only** by default. To expose it on your LAN, add
`--stats-bind 0.0.0.0` and protect it with `--stats-password <token>` (sent as
`Authorization: Bearer <token>` or `?token=<token>`); `/healthz` stays open.

The `connection` object also reports `reconnects` and `failovers` (lifetime
counts), so a dashboard can flag an unstable link or a flaky pool at a glance.

Get a **Discord** ping when you pass an accepted-share milestone:

```sh
csd-pool-miner --address <ADDR> --discord-webhook https://discord.com/api/webhooks/...
```

Notifications are best-effort and non-blocking — a slow or dead webhook never
affects mining.

## HiveOS

A HiveOS **Custom-miner package** ships with each release
(`csd-pool-miner-hiveos-<version>.tar.gz`, also in the [`hiveos/`](hiveos/) dir).
Add it as a Custom miner with your addr20 as the wallet; it reports hashrate and
accepted/rejected shares to the HiveOS dashboard (by scraping the miner's own
stats endpoint, so the kH/s is always correct).

## Auto-update (24/7 rigs)

`mine-auto.sh` (Linux) / `mine-auto.bat` (Windows) run every card and keep the
binary current: they check the latest release with a proper semver compare,
**verify the download's SHA-256 against the release `SHA256SUMS` before swapping
it in** (atomic, and they keep the running binary if verification fails), and
restart the miner if it ever exits.

## Reliability (failover + watchdog)

For 24/7 rigs the miner keeps itself connected and earning without babysitting:

- **Multiple pools / failover** — give a comma-separated list and it tries them in
  order, rotating to the next on a dead connection and failing back to the first
  once it recovers:

  ```sh
  csd-pool-miner --address <ADDR> --pool pool.example:3333,backup.example:3333
  ```

  (`--url` is an alias.) With no `--pool`, it uses the built-in CSD pool.
- **Connection watchdog** — if the link goes half-open (still gets work but
  silently drops your shares) or the pool stops sending new jobs, the miner forces
  a clean reconnect instead of mining into the void.
- **Won't-credit failover** — if a pool keeps acking and pushing fresh work but
  stops *crediting* your shares for ~30 minutes (a forked or misconfigured pool),
  the miner rotates to the next endpoint on its own — at most once per window, so
  one bad pool can't cause a reconnect storm.

The 30-second health heartbeat in the log ends with
`conn=<reconnects>/<failovers>` so connection churn is visible at a glance, and
`selftest` TCP-probes each pool endpoint (PASS/FAIL) so you can confirm
reachability before a long run.

Invalid mining parameters (e.g. `--blocks 0`, `--cpu-share 5`) now **fail loudly**
with a clear message at startup instead of being silently clamped, so a typo can't
quietly leave you mining at a fraction of your hardware.

## Config file (optional)

Instead of passing flags every run, drop a `config.toml` next to the binary, at
`~/.config/csd-pool-miner/config.toml` (Linux/macOS) or
`%APPDATA%\csd-pool-miner\config.toml` (Windows), or point at one with `--config
<path>`. Any explicit CLI flag overrides the file, which overrides the built-in
defaults. See [`config.example.toml`](config.example.toml) for every key — a
minimal example:

```toml
address = "your40charhexaddr20goeshere0000000000000"
# CPU threads to mine ALONGSIDE the GPU (dual mining). 0 = GPU-only.
cpu_threads = 0
```

**CPU usage on GPU builds:** by default a GPU build *also* mines on the CPU
(`cpu_threads = 16`) for extra hashrate, so you'll see high CPU use even while
the GPU works. To let the GPU do the work and keep your CPU free — recommended
on laptops, where the CPU and GPU share one power/thermal budget — set
`cpu_threads = 0` (or pass `--cpu-threads 0`).

## Payouts

Payouts are **batched hourly by the pool, at the top of every hour (:00)**. Your
shares accrue continuously; the pool settles all eligible miners together once an
hour, so you won't see a payout the instant you find a share — wait for the next
:00 settlement.

## Where to get an addr20

`--address` is your **addr20** — your CSD payout address: **40 lowercase hex
characters** (an optional `0x` prefix is accepted).

**No address yet? Create a wallet in one step:**

- **Windows** — download & double-click **`create-wallet.bat`**
- **Linux** — `curl -fsSL https://raw.githubusercontent.com/dangraagu/CSD-Mining-pool-public/main/create-wallet.sh | bash`
- **Already have the miner?** — `csd-pool-miner newwallet`

It generates a fresh key locally, prints your **addr20**, and writes it (with the
private key) to `csd-wallet.txt`. ⚠️ **Back up that file — losing the private key
means losing access to any coins paid to the address.** The saved key imports into
a full node with `csd wallet recover` when you want to spend.

Already have a CSD node/wallet? Your existing address works too — it's the same
one you'd receive payouts to. Anything that isn't 40 hex chars is rejected at
startup with a clear error.

## Building

CPU-only (no GPU toolchain needed):

```sh
cargo build --release
```

With a GPU backend:

```sh
cargo build --release --features cuda          # NVIDIA
cargo build --release --features opencl        # AMD / other
cargo build --release --features "cuda,opencl" # both; auto-pick best at runtime
```

The pool endpoint is compiled into the binary. (Operators cutting a release: set
it in `src/endpoint.rs` — see the module docs there.)

## License

MIT OR Apache-2.0.
