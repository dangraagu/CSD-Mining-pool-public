# Running CSD Pool Miner on HiveOS

Every release ships a ready-made HiveOS **Custom-miner package**, so setup is just a Flight Sheet.

## Steps

1. **Add a wallet** — *Wallets → Add Wallet*. Use your **CSD payout address** (40-hex `addr20`; a leading `0x` is fine). This is the only value the miner needs.

2. **Create a Flight Sheet** — *Flight Sheets → Add Flight Sheet*, set **Miner → Custom**, and fill:

   | Field | Value |
   |---|---|
   | Installation URL | `https://github.com/dangraagu/CSD-Mining-pool-public/releases/latest/download/csd-pool-miner.tar.gz` |
   | Miner name | `csd-pool-miner` |
   | Hash algorithm | *anything — not used* |
   | Wallet and worker template | `%WAL%` |
   | Pool URL | *leave blank — ignored (the pool is built into the binary)* |
   | Extra config arguments | *backend flag — see step 3* |

   > ⚠️ **The Miner name must be EXACTLY `csd-pool-miner`** — NOT `csd-pool-miner-hiveos`. HiveOS derives the install folder from this name, and the package's folder is `csd-pool-miner`; a mismatched name makes HiveOS look for a folder that isn't there and the rig reports **"Miner screen is not running."**

3. **Pick your backend** in *Extra config arguments*:
   - NVIDIA → `--backend cuda`
   - AMD → `--backend opencl`
   - CPU → `--backend cpu`
   - *(leave empty to auto-detect; add `--gpu-id 0,1` to choose specific cards)*

4. **Apply** the Flight Sheet to your rig. Done — hashrate and accepted/rejected shares show on the HiveOS dashboard.

## Notes

- **The Pool URL field does nothing.** The pool endpoint is compiled into the binary and can't be changed — just set the wallet and apply.
- **Do not put `--pool` in Extra config arguments** — the miner will reject it and won't start.
- **Updating is now automatic — you do not need to touch the Flight Sheet again.** `h-run.sh` self-updates the miner on this rig:
  - **At every start** it checks the latest GitHub release and, if a newer version is published, downloads the matching build, **verifies its SHA-256 against the release `SHA256SUMS`**, and atomically swaps it in *before* launching — so a rig that has been off comes back up current.
  - **While running** a background poll checks roughly every 15 minutes (`CHECK_MIN`); when a newer version verifies, it swaps the binary and bounces the miner so HiveOS relaunches on the new build.
  - **Fail-safe:** any update failure (no network, GitHub rate-limit, SHA mismatch, partial download, disk full) is logged and the rig keeps mining on the **existing** binary — an update problem never strands or bricks a rig. Update activity is written to the miner log under `/var/log/miner/csd-pool-miner/`.
  - The **Installation URL above is the non-staling `releases/latest/download/` form**, so re-running the install (or adding a new rig) always pulls the current release. You only ever need to re-apply the Flight Sheet to *reinstall the glue* (e.g. after a HiveOS image wipe), not to update the miner.

## Live terminal dashboard

Every rig ships a **read-only terminal dashboard** next to the miner. Open the **Hive Shell** (the web terminal in the HiveOS dashboard) or SSH into the rig and run:

```sh
/hive/miners/custom/csd-pool-miner/csd-dashboard.sh
```

It shows a live, refreshing view of **this rig's** hashrate (10s / 1m / 15m), accepted / rejected / stale shares with reject%, GPU temp + power, and reconnects / failovers. Press **`q`** or **Ctrl-C** to quit; add `--once` for a single snapshot. HiveOS runs the miner's stats endpoint on port **3380**, which is the dashboard's default, so no flags are needed.

It is a **viewer only** — it just reads the miner's own stats endpoint over localhost. It never changes config, never touches the share/submit path, and cannot slow or interrupt mining.
