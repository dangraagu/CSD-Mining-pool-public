# csd-pool-miner

A standalone miner for the **Compute Substrate (CSD)** network. Point it at your
payout address and it mines to the CSD pool — auto-detecting your GPU (NVIDIA or
AMD) or falling back to CPU.

It connects to the pool **by default**: there is no server/pool flag to set. The
only thing you have to provide is your csd1 address.

## Requirements

One of:

- **NVIDIA GPU** — recent NVIDIA driver (the CUDA backend links at runtime; no
  toolkit install needed). Use a build with the `cuda` feature.
- **AMD / other GPU** — an OpenCL driver/runtime for your card. Use a build with
  the `opencl` feature.
- **CPU only** — no GPU or driver required; works out of the box.

The default prebuilt binary is **CPU-only**. For GPU mining, use a release built
with the matching feature (see [Building](#building)).

## Quick start

```sh
csd-pool-miner --address <YOUR_CSD1_ADDRESS>
```

That's it. The miner will:

1. auto-detect the best backend (tries CUDA → OpenCL → CPU),
2. connect to the CSD pool,
3. start submitting shares for `<YOUR_CSD1_ADDRESS>`.

### Choosing a backend

Auto-detect is the default. To force one:

```sh
csd-pool-miner --address <YOUR_CSD1_ADDRESS> --backend auto    # default: cuda -> opencl -> cpu
csd-pool-miner --address <YOUR_CSD1_ADDRESS> --backend cuda     # NVIDIA
csd-pool-miner --address <YOUR_CSD1_ADDRESS> --backend opencl   # AMD / other
csd-pool-miner --address <YOUR_CSD1_ADDRESS> --backend cpu      # CPU only
```

### Useful extras

```sh
csd-pool-miner devices     # list detected GPUs (handy if auto keeps picking CPU)
csd-pool-miner selftest    # cross-check every backend against the reference CPU hasher
```

## Payouts

Payouts are **batched hourly by the pool, at the top of every hour (:00)**. Your
shares accrue continuously; the pool settles all eligible miners together once an
hour, so you won't see a payout the instant you find a share — wait for the next
:00 settlement.

## Where to get a csd1 address

`--address` is your CSD payout address: **40 lowercase hex characters** (an
optional `0x` prefix is accepted). Create one with your CSD node/wallet — it's
the same address you'd receive coinbase rewards on when solo mining. Anything
that isn't 40 hex chars is rejected at startup with a clear error.

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
