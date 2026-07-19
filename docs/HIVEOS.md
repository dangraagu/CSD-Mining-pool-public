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
   | Extra config arguments | *`--backend cuda --address <40-hex addr20>` — set the backend explicitly; see step 3* |

   > ⚠️ **The Miner name must be EXACTLY `csdpool`** (HiveOS auto-fills this from the URL — leave it). HiveOS derives the install folder from the tarball filename by stripping the last `-`-separated token as a "version", so a hyphen in the name breaks it (`csd-pool-miner` → `csd-pool`, `csd-pool` → `csd`). The package is named `csdpool` (no hyphen) precisely so HiveOS derives it cleanly; a mismatched name makes the rig report **"Miner screen is not running."**

   > ⚠️ **Pool URL cannot be blank** — HiveOS won't save a Custom flight sheet without one. The miner **ignores** it (the pool is compiled into the binary), so use the harmless placeholder `stratum+tcp://127.0.0.1:1` exactly as shown. Do **not** point it at a real pool.

   > ⚠️ **Set your CSD address with `--address <addr>` in *Extra config arguments* — the "Wallet and worker template" field does NOT work for CSD.** HiveOS only passes that field for a HiveOS-known coin; CSD isn't one, so it stays empty and the miner exits with *"--address must be 40 hex chars … got 0 chars"*. Put the bare 40-hex `addr20` after `--address` (a `0x`/`0X` prefix and a `.worker` suffix are both fine and get stripped).

3. **Set your backend AND address** in *Extra config arguments*, both on one line, e.g.
   `--backend cuda --address <your 40-hex CSD addr20>` (paste YOUR own address, not the example):
   - **Backend — SET IT EXPLICITLY (recommended on HiveOS):** NVIDIA → `--backend cuda` · AMD → `--backend opencl` · CPU → `--backend cpu`. You *can* omit it to auto-detect the GPU, but on HiveOS the GPU probe can occasionally come up empty at launch and fall back to the **CPU build, which mines nothing** ("online but 0 H/s"). Naming `--backend` explicitly is deterministic — do that.
   - **Address (required):** `--address <your 40-hex addr20>` (a `0x`/`0X` prefix and a `.worker` suffix are both stripped).
   - *(Multi-GPU rigs need **nothing** here — the launcher mines every card automatically. To **skip** a card, add `--gpu-id 0,2,3`; see [Multi-GPU rigs](#multi-gpu-rigs).)*

4. **Apply** the Flight Sheet to your rig. Done — hashrate and accepted/rejected shares show on the HiveOS dashboard.

## Solo mining

To **solo-mine** on this rig (go for the whole block yourself instead of a steady pool cut), add `--worker solo` to *Extra config arguments* alongside your backend and address:

```
--backend cuda --address <your 40-hex addr20> --worker solo
```

The rig then authorizes as `<address>.solo`: your shares are excluded from the pool (PPLNS) split, and if *your* rig solves a block you're credited the **full reward minus the 2.5% fee** (after 15 confirmations, on the normal payout schedule). It's **high-variance** — you win the whole block or nothing that round. Your payout address is unchanged; `solo` only changes how it's credited. Remove `--worker solo` (or use any other name) to go back to pool mining. Solo wins appear in **blue** on the pool's Winners board. See the [main README](../README.md#solo-mining) for the full rundown.

## Multi-GPU rigs

**One miner process drives exactly one GPU** — the card is picked with `--device N`. There is **no flag that makes a single process use every card**. An 8-GPU rig runs 8 processes, all on the same address; the pool sums their shares.

**On HiveOS you do not have to do any of that.** `h-run.sh` fans out for you: it counts the cards (`nvidia-smi -L`, or `clinfo` on AMD), launches devices `1..N-1` in the background with `--device <i> --stats-port <3380+i>`, then `exec`s device 0 on `3380`. Shipped since v0.1.19 — apply the Flight Sheet from the steps above and every card mines. Every probe is fail-soft: if a probe or a spawn fails the rig degrades to card 0 only, never a brick.

> ⚠️ **Never put `--device` in *Extra config arguments*.** The launcher emits `--device <i>` *before* your extra flags and the **last** `--device` on the command line wins — so a `--device 0` in that box overrides the launcher's per-card assignment and collapses **every** process onto card 0. The rig looks like it is mining N GPUs and mines one. Current glue strips a stray `--device` and logs a `h-config: WARNING` instead, but **the glue only refreshes when you re-apply the Flight Sheet** — a rig that hasn't been re-applied is still exposed. Use `--gpu-id` to choose cards; never `--device`.

### Skipping a bad card — `--gpu-id`

`--gpu-id` is **not** a device selector. It is an **include-list the launcher reads** to decide which cards to spawn a process for. Use it to drop a card that is unstable or thermally throttling:

```
--backend cuda --address <your 40-hex addr20> --gpu-id 0,2,3
```

That mines cards 0, 2 and 3 and leaves card 1 idle.

> ⚠️ **Keep *Extra config arguments* on ONE line, space-separated.** `h-run.sh` matches ` --gpu-id ` (or `--gpu-id=`) inside a single flag string. If the flight sheet splits your arguments across newlines the match fails and the launcher **silently falls back to "all cards"** — no warning. That is usually what people want anyway, but if you were trying to skip a card it will keep mining on it.

> ⚠️ **Card 0 cannot be excluded on HiveOS.** Device 0 is always the `exec`ed process, so `--gpu-id 1,2` still mines card 0. If card 0 is the bad one, pull it or run outside the HiveOS glue (see the [main README](../README.md#multi-gpu-rigs)).

Ids at or beyond the detected GPU count are ignored, and the stats port always follows the device index (card 2 → `3382`) whether or not card 1 was included.

### ⚠️ HiveOS under-reports ~1/N until you re-apply the Flight Sheet

`h-stats.sh` used to scrape a **single** stats port (`CUSTOM_API_PORT`, default `3380`), which is **device 0's**. The extra cards report on `3381`, `3382`, … and were never summed — so an N-card rig reads roughly **1/N of its real hashrate** on the HiveOS dashboard. The current glue probes the whole port range and merges what it finds, one `hs[]` entry per card.

> ⚠️ **The fix does not reach your rig on its own — re-apply the Flight Sheet.** `h-run.sh` self-updates the **miner binary** only (see [Notes](#notes)); the glue around it (`h-run.sh`, `h-config.sh`, `h-stats.sh`) is installed from the tarball and **only refreshes on a Flight-Sheet re-apply**. A rig that has been up since before the fix keeps reporting ~1/N however current its binary is. Re-apply once — *Flight Sheets → your sheet → Apply* — and that is the whole procedure. Same one-time re-apply as the pre-0.1.17 glue upgrade in [Notes](#notes); if you have already done that one, do it again for this.

After a re-apply the tile **should** show a rate per card and a total in line with the pool. That path has not yet been confirmed on a real multi-GPU rig, so treat it as expected-not-proven: **if your tile still reads low after a re-apply, please tell us** — open an issue with the output of step 3 below and of `h-stats.sh` itself.

**The cards are mining either way; only the number is wrong.** The pool has always counted every card. Measured in the field on a real 6-GPU rig: HiveOS showed **1.64 GH/s** while the pool measured **11.68 GH/s** from that rig's submitted shares — a ~7× under-report on a completely healthy rig. **Do not restart or rebuild a rig on the strength of the HiveOS tile** — check the pool dashboard first (below), because chasing this number is chasing a fault that is not there.

### Verifying every card is actually mining

Open the **Hive Shell** (or SSH into the rig):

```sh
# 1. one process per card
pgrep -a csd-gpu-miner | wc -l

# 2. what the launcher decided to spawn
grep 'multi-GPU' /var/log/miner/csdpool/*.log

# 3. ask each card's stats port directly (no jq; never hangs)
for p in 3380 3381 3382 3383 3384 3385; do
  printf '%s: ' "$p"
  /hive/miners/custom/csdpool/csd-gpu-miner hiveos-stats --stats-port "$p"
done

# 4. per-GPU logs (device 0 logs to csdpool.log; extra cards to gpuN.log)
tail -n 20 /var/log/miner/csdpool/gpu*.log

# 5. the hardware — every card should show high util and power near its limit
nvidia-smi --query-gpu=index,name,utilization.gpu,power.draw --format=csv
```

A card that is mining reports a non-zero `hs` value in step 3; an all-zero object means that process is missing or wedged.

The **pool dashboard** is the authoritative number, and the one to check *first* when HiveOS looks wrong. Your per-rig hashrate under **"Your Miner"** is derived from the shares your rig actually submitted — a measurement of real work, not a scrape of a local endpoint — so it counts every process regardless of what HiveOS displays. A real 6-GPU rig read **1.64 GH/s** on the HiveOS tile while the pool measured **11.68 GH/s** from its shares; the rig was healthy and the tile was wrong. **Believe the pool.**

### Worker names

Each process authorizes as `<address>.<worker>`, and HiveOS bakes **one** worker name for the whole rig (from the HiveOS `$WORKER_NAME`, into the config every card shares). So **all cards on a HiveOS rig report under a single name** — per-card names are not available under the HiveOS glue.

> ⚠️ **Don't name a HiveOS rig `solo` casually.** Because the glue bakes your rig's `$WORKER_NAME` as the worker, the rig's *name in HiveOS* decides this — and `solo` is a **reserved opt-in mode**, not a display label. A rig called `solo` is [solo mining](#solo-mining): its shares are excluded from the PPLNS split and it earns nothing in a round it doesn't win outright. Every other name is display-only. Use `solo` only when you mean it.

If you want per-card names (`RIG1-GPU0` … `RIG1-GPU7`, so a dead card is obvious at a glance instead of "the rig lost a third of its hashrate"), run the miner outside HiveOS with one `--worker` per process — see the [main README](../README.md#worker-names-on-a-multi-gpu-rig). One production miner on this pool runs **8× V100S** exactly that way.

## Hardware reporting (v0.2.4)

From **v0.2.4** the miner appends the **GPU model** to the version string it has always sent in the Stratum handshake: `csd-gpu-miner/0.2.4 (RTX 5070 Ti)` instead of `csd-gpu-miner/0.2.3`. It lets the pool publish what each card *actually* does on this coin, measured from real submitted shares across the fleet, instead of our own bench figures on the few cards we own.

It is telemetry and we will be plain about it. **What goes out:** the card's marketing name, vendor prefix stripped, printable ASCII, capped at 28 characters. Nothing else — no serial, no driver version, no hostname, no IP, no rig or OS detail. **When:** once per connection — in the opening handshake, on the line that already carried the version, and again in the same handshake on a reconnect or failover. Nothing polls it. **What it can affect:** nothing that pays you. Payouts key off `mining.authorize`, a different message carrying your address; the handshake string never reaches the share, credit, or payout path.

**CUDA only, best-effort.** The AMD/OpenCL and CPU builds have no lookup compiled in at all. On an NVIDIA build any failure to read the card name falls silently back to the plain string — it can't stall or fail a launch.

Because `h-run.sh` gives every process its own `--device <i>`, **each card reports its own model**, so a mixed rig shows up as the mix it is rather than N copies of card 0.

> **Opt out** by adding `--no-hardware-report` to *Extra config arguments*, next to your backend and address:
>
> ```
> --backend cuda --no-hardware-report
> ```
>
> Or, if you would rather not touch the flight sheet's arguments, set the environment variable `CSD_NO_HARDWARE_REPORT=1` — either one alone is enough. Setting the flag always wins, so `--no-hardware-report` still opts out even on a rig where something else has exported `CSD_NO_HARDWARE_REPORT=0`. Either way the miner logs `hardware reporting DECLINED` on start, so you can confirm it took effect in the rig's log rather than trusting this page.
>
> Running the **AMD or CPU build**, or forcing `--backend cpu` / `--backend opencl`, also turns it off with no flag at all. See [What the miner reports about your hardware](../README.md#what-the-miner-reports-about-your-hardware) in the main README for the full detail.

## Notes

- **The Pool URL field is ignored** (the pool is compiled into the binary) but HiveOS still requires it non-blank — use `stratum+tcp://127.0.0.1:1` as shown above and never point it at a real pool.
- **Do not put `--pool` in Extra config arguments** — the miner will reject it and won't start.
- **Updating is now automatic — you do not need to touch the Flight Sheet again.** `h-run.sh` self-updates the miner on this rig:
  - **At every start** it checks the latest GitHub release and, if a newer version is published, downloads the matching build, **verifies its SHA-256 against the release `SHA256SUMS`**, and atomically swaps it in *before* launching — so a rig that has been off comes back up current.
  - **While running** a background poll checks roughly every 15 minutes (`CHECK_MIN`); when a newer version verifies, it swaps the binary and bounces the miner so HiveOS relaunches on the new build.
  - **Fail-safe:** any update failure (no network, GitHub rate-limit, SHA mismatch, partial download, disk full) is logged and the rig keeps mining on the **existing** binary — an update problem never strands or bricks a rig. Update activity is written to the miner log under `/var/log/miner/csdpool/`.
  - The **Installation URL above is the non-staling `releases/latest/download/` form**, so re-running the install (or adding a new rig) always pulls the current release. You only ever need to re-apply the Flight Sheet to *reinstall the glue* (e.g. after a HiveOS image wipe), not to update the miner.
- **Upgrading from a pre-0.1.17 rig:** the auto-detect + auto-fetch of the right GPU build lives in the *glue* (`h-run.sh`/`h-config.sh`), which only refreshes when the Flight Sheet is re-applied. A rig installed before 0.1.17 needs a **one-time Flight-Sheet re-apply** to pick up the new glue; after that it self-updates as above.
- **Rig shows "online" but 0 H/s?** It almost always means the rig is running the CPU seed with no GPU build (the bug the auto-detect fixes) or an address problem. Open the Hive Shell (or SSH) and check the launcher log:

  ```sh
  tail /var/log/miner/csdpool/*.log | grep '\[h-run\]'
  ```

  Look for the variant the launcher resolved and any `WARNING` lines (e.g. a `--backend cuda` requested on a box with no NVIDIA driver). Then re-apply the Flight Sheet so the rig re-detects and fetches the correct GPU build.

## Live terminal dashboard

Every rig ships a **read-only terminal dashboard** next to the miner. Open the **Hive Shell** (the web terminal in the HiveOS dashboard) or SSH into the rig and run:

```sh
/hive/miners/custom/csdpool/csd-dashboard.sh
```

It shows a live, refreshing view of **this rig's** hashrate (10s / 1m / 15m), accepted / rejected / stale shares with reject%, GPU temp + power, and reconnects / failovers. Press **`q`** or **Ctrl-C** to quit; add `--once` for a single snapshot. HiveOS runs the miner's stats endpoint on port **3380**, which is the dashboard's default, so no flags are needed.

It is a **viewer only** — it just reads the miner's own stats endpoint over localhost. It never changes config, never touches the share/submit path, and cannot slow or interrupt mining.
