# Running CSD Pool Miner on HiveOS

Every release ships a ready-made HiveOS **Custom-miner package**, so setup is just a Flight Sheet.

## Steps

1. **Add a wallet** — *Wallets → Add Wallet*. Use your **CSD payout address** (40-hex `addr20`; a leading `0x` is fine). This is the only value the miner needs.

2. **Create a Flight Sheet** — *Flight Sheets → Add Flight Sheet*, set **Miner → Custom**, and fill:

   | Field | Value |
   |---|---|
   | Installation URL | `https://github.com/dangraagu/CSD-Mining-pool-public/releases/download/v0.1.8/csd-pool-miner-0.1.8.tar.gz` |
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
- **Updating:** HiveOS does not auto-update custom miners. To move to a newer version, change the *Installation URL* to that release's `csd-pool-miner-<version>.tar.gz` and re-apply the Flight Sheet.
