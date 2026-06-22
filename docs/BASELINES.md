# GPU hashrate baselines

This file records **expected SHA-256d throughput** (the work `csd-pool-miner`
actually does: `sha256d(84-byte header) <= target`) for a few common NVIDIA
GPUs, so you can tell at a glance whether your card is performing in the right
ballpark after an `--auto-tune` or a `bench` run.

These numbers are an **indicative reference, not a guarantee.** Real throughput
varies with driver version, power/thermal limits, clocks, the rest of the
system load, and whether CPU dual-mining (`--cpu-threads`) is also running. Use
them to spot a card that's an order of magnitude off (a bad overclock, a
thermal-throttled card, a wrong-geometry launch) — not as a precise target.

> The benchmark drives the **identical mining kernel** and computes the
> **identical hash** as live mining; only the launch *geometry* is varied. So a
> `bench` MH/s reflects real hashing throughput. Nothing here changes
> PoW/consensus.

## How to measure your own card

Build with the CUDA backend, then run the reproducible benchmark (it does **not**
mine and needs **no** address):

```sh
cargo build --release --features cuda

# Single (default or current) geometry — prints one stable MH/s:
./csd-gpu-miner bench --device 0

# Sweep every auto-tune candidate geometry and print the winner:
./csd-gpu-miner bench --device 0 --all-geometries

# Longer per-geometry window = steadier numbers (default 5s):
./csd-gpu-miner bench --device 0 --all-geometries --auto-tune-secs 10
```

To have the miner pick + remember the best geometry automatically at startup:

```sh
./csd-gpu-miner --address <addr20> --auto-tune
```

`--auto-tune` benchmarks the candidate geometries once, uses the fastest, and
**persists** it to the per-machine config cache
(`<config dir>/csd-pool-miner/autotune.toml`, scoped to that exact card). Later
starts reuse the cached geometry **without** re-benchmarking. Pass `--auto-tune`
again to force a fresh sweep (e.g. after a driver update or a GPU swap); an
explicit `--blocks/--threads-per-block/--nonces-per-thread` always overrides the
cache.

## Indicative SHA-256d throughput (whole-GPU, MH/s)

Single-process, one GPU, GPU-only (no CPU dual-mining). Ranges span the spread
across drivers/power limits; your `bench` figure should land roughly inside the
range for your card.

| GPU                         | Approx. SHA-256d (MH/s) | Typical winning geometry* |
|-----------------------------|-------------------------|---------------------------|
| RTX 4090                    | 9,000 – 12,000          | 2048 × 256 × 1024         |
| RTX 4080 / 4070 Ti          | 6,000 – 8,500           | 1024 × 256 × 2048         |
| RTX 3090 / 3090 Ti          | 6,500 – 8,500           | 2048 × 256 × 1024         |
| RTX 3080                    | 5,000 – 6,500           | 1024 × 256 × 2048         |
| RTX 3070                    | 3,500 – 4,500           | 560 × 256 × 4096          |
| RTX 3060 / 3060 Ti          | 2,500 – 3,800           | 560 × 256 × 4096          |
| RTX 2080 Ti                 | 3,000 – 4,000           | 560 × 256 × 4096          |
| GTX 1660 Super              | 1,200 – 1,800           | 512 × 256 × 2048          |
| GTX 1080 Ti                 | 2,000 – 2,800           | 560 × 256 × 4096          |
| Laptop RTX 3060 / 4060      | 1,500 – 2,800           | 512 × 256 × 2048          |

\* The "typical winning geometry" is what `--auto-tune` tends to pick from the
candidate set; the auto-tuner measures your specific card and may choose a
neighbour. The shipped default (used with no tune) is **560 × 256 × 4096**.

### Candidate geometries benchmarked by `--auto-tune` / `bench --all-geometries`

`(blocks × threads_per_block × nonces_per_thread)` — nonces swept per launch is
the product:

| blocks | threads/block | nonces/thread | nonces/launch |
|-------:|--------------:|--------------:|--------------:|
| 256    | 256           | 4096          | 268,435,456   |
| 512    | 256           | 2048          | 268,435,456   |
| 560    | 256           | 4096          | 587,202,560   |
| 1024   | 256           | 2048          | 536,870,912   |
| 2048   | 256           | 1024          | 536,870,912   |
| 1024   | 512           | 1024          | 536,870,912   |

If you measure a card not listed here (or get figures well outside a listed
range on a healthy rig), please open an issue/PR with the GPU model, driver
version, and your `bench --all-geometries` output so this table can be improved.
