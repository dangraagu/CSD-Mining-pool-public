# csd-pool-miner

A standalone miner for the **Compute Substrate (CSD)** network. Point it at your
payout address and it mines to the CSD pool — auto-detecting your GPU (NVIDIA or
AMD) or falling back to CPU.

It connects to the pool **by default**: there is no server/pool flag to set. The
only thing you have to provide is your addr20 (a 40-hex CSD payout address).

Discord channel for mining stats, updates and support/improvements:
https://discord.gg/Gr9gCjzC9e

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
csd-pool-miner devices     # list detected GPUs (handy if auto keeps picking CPU)
csd-pool-miner selftest    # cross-check every backend against the reference CPU hasher
```

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
