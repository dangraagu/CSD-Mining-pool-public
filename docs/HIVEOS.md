# Running CSD Pool Miner on HiveOS

Every release ships a ready-made HiveOS **Custom-miner package**, so setup is just a Flight Sheet.

## Steps

1. **Add a wallet** — *Wallets → Add Wallet*. Use your **CSD payout address** (40-hex `addr20`; a leading `0x` is fine). This is the only value the miner needs.

2. **Create a Flight Sheet** — *Flight Sheets → Add Flight Sheet*, set **Miner → Custom**, and fill:

   | Field | Value |
   |---|---|
   | Installation URL | `https://github.com/dangraagu/CSD-Mining-pool-public/releases/latest/download/csdpool.tar.gz` |
   | Miner name | `csdpool` |
   | Hash algorithm | *anything — not used* |
   | Wallet and worker template | *leave blank* — HiveOS does **not** pass this field for a coinless miner (CSD); set your address with `--address` in *Extra config arguments* (step 3) |
   | Pool URL | `stratum+tcp://127.0.0.1:1` |
   | Extra config arguments | *your `--address` **and** backend flag — see step 3* |

   > ⚠️ **The Miner name must be EXACTLY `csdpool`** (HiveOS auto-fills this from the URL — leave it). HiveOS derives the install folder from the tarball filename by stripping the last `-`-separated token as a "version", so a hyphen in the name breaks it (`csd-pool-miner` → `csd-pool`, `csd-pool` → `csd`). The package is named `csdpool` (no hyphen) precisely so HiveOS derives it cleanly; a mismatched name makes the rig report **"Miner screen is not running."**

   > ⚠️ **Pool URL cannot be blank** — HiveOS won't save a Custom flight sheet without one. The miner **ignores** it (the pool is compiled into the binary), so use the harmless placeholder `stratum+tcp://127.0.0.1:1` exactly as shown. Do **not** point it at a real pool.

   > ⚠️ **Set your CSD address with `--address <addr>` in *Extra config arguments* — the "Wallet and worker template" field does NOT work for CSD.** HiveOS only passes that field for a HiveOS-known coin; CSD isn't one, so it stays empty and the miner exits with *"--address must be 40 hex chars … got 0 chars"*. Put the bare 40-hex `addr20` after `--address` (a `0x`/`0X` prefix and a `.worker` suffix are both fine and get stripped).

3. **Set your address and backend** in *Extra config arguments* — both on one line, e.g.
   `--backend cuda --address <your 40-hex CSD addr20>` (paste YOUR own address, not the example):
   - **Address (required):** `--address <your 40-hex addr20>`
   - **Backend:** NVIDIA → `--backend cuda` · AMD → `--backend opencl` · CPU → `--backend cpu`
   - **`--backend` is REQUIRED — name it explicitly.** Do **not** omit it or use `auto`: HiveOS ships the CPU build as a universal seed and the auto-updater only fetches the **NVIDIA**/**AMD** build when you name `cuda`/`opencl`. A blank/`auto` backend leaves the rig on the CPU seed, which has no GPU compiled in — it connects to the pool and gets jobs but reports *"no GPU backend usable"* and **does not hash**. (Optionally add `--gpu-id 0,1` to pick specific cards.)

4. **Apply** the Flight Sheet to your rig. Done — hashrate and accepted/rejected shares show on the HiveOS dashboard.

## Notes

- **The Pool URL field is ignored** (the pool is compiled into the binary) but HiveOS still requires it non-blank — use `stratum+tcp://127.0.0.1:1` as shown above and never point it at a real pool.
- **Do not put `--pool` in Extra config arguments** — the miner will reject it and won't start.
- **Updating is now automatic — you do not need to touch the Flight Sheet again.** `h-run.sh` self-updates the miner on this rig:
  - **At every start** it checks the latest GitHub release and, if a newer version is published, downloads the matching build, **verifies its SHA-256 against the release `SHA256SUMS`**, and atomically swaps it in *before* launching — so a rig that has been off comes back up current.
  - **While running** a background poll checks roughly every 15 minutes (`CHECK_MIN`); when a newer version verifies, it swaps the binary and bounces the miner so HiveOS relaunches on the new build.
  - **Fail-safe:** any update failure (no network, GitHub rate-limit, SHA mismatch, partial download, disk full) is logged and the rig keeps mining on the **existing** binary — an update problem never strands or bricks a rig. Update activity is written to the miner log under `/var/log/miner/csdpool/`.
  - The **Installation URL above is the non-staling `releases/latest/download/` form**, so re-running the install (or adding a new rig) always pulls the current release. You only ever need to re-apply the Flight Sheet to *reinstall the glue* (e.g. after a HiveOS image wipe), not to update the miner.

## Live terminal dashboard

Every rig ships a **read-only terminal dashboard** next to the miner. Open the **Hive Shell** (the web terminal in the HiveOS dashboard) or SSH into the rig and run:

```sh
/hive/miners/custom/csdpool/csd-dashboard.sh
```

It shows a live, refreshing view of **this rig's** hashrate (10s / 1m / 15m), accepted / rejected / stale shares with reject%, GPU temp + power, and reconnects / failovers. Press **`q`** or **Ctrl-C** to quit; add `--once` for a single snapshot. HiveOS runs the miner's stats endpoint on port **3380**, which is the dashboard's default, so no flags are needed.

It is a **viewer only** — it just reads the miner's own stats endpoint over localhost. It never changes config, never touches the share/submit path, and cannot slow or interrupt mining.
