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
csd-pool-miner selftest        # cross-check every backend against the reference CPU hasher
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

Get a **Discord** ping when you find a block (solo) or pass a share milestone
(pool):

```sh
csd-pool-miner --address <ADDR> --discord-webhook https://discord.com/api/webhooks/...
# add --discord-solutions-only to ping ONLY on solved blocks
```

Notifications are best-effort and non-blocking — a slow or dead webhook never
affects mining.

## Solo mining — mine to your own node

By default you mine to the pool. To mine **directly to your own csd-node**
instead — no pool, no fee, no PPLNS; every block you find is yours:

```sh
csd-pool-miner --address <ADDR> --solo --node http://127.0.0.1:8799
```

The miner pulls work from `<node>/work/get` and submits solved blocks to
`<node>/work/submit`. It's mutually exclusive with the pool — you earn nothing
until you find a block, but keep the whole block when you do. `--stats-port` and
`--discord-webhook` work in solo too (Discord fires on a found block).

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
one you'd receive coinbase on when solo mining. Anything that isn't 40 hex chars
is rejected at startup with a clear error.

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
