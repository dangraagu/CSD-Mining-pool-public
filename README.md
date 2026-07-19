# csd-pool-miner™

A standalone miner for the **Compute Substrate (CSD)** network. Point it at your
payout address and it mines to the CSD pool — auto-detecting your GPU (NVIDIA or
AMD) or falling back to CPU.

It connects to the pool **by default**: there is no server/pool flag to set. The
only thing you have to provide is your addr20 (a 40-hex CSD payout address).

Discord channel for mining stats, updates and support/improvements:
https://discord.gg/gSdneDDexm

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
curl -fsSL https://raw.githubusercontent.com/dangraagu/CSD-Mining-pool-public/main/install-csd-miner.sh | CSD_ADDR=<YOUR_ADDR20> bash
```

Auto-detects your GPU (NVIDIA / AMD, else CPU), downloads the matching prebuilt
binary from the latest GitHub Release, and starts mining to `<YOUR_ADDR20>`
(remembered under `~/.config/csd-pool-miner/`, so later runs need no address).
Force a variant by passing it through: `… | CSD_ADDR=<addr> bash -s -- nvidia|amd|cpu`.

**Supply `CSD_ADDR` the first time.** Piping into `bash` leaves no terminal to
prompt on, so the address must come from the environment (as above) or as the 2nd
positional arg. If you run the script from a saved file in a real terminal instead
of piping, it will prompt:

```sh
curl -fsSL https://raw.githubusercontent.com/dangraagu/CSD-Mining-pool-public/main/install-csd-miner.sh -o install-csd-miner.sh
bash install-csd-miner.sh        # prompts for your addr20 the first time
```

Prefer to run it yourself? Download `csd-pool-miner-linux-<nvidia|amd|cpu>` from
[Releases](https://github.com/dangraagu/CSD-Mining-pool-public/releases/latest),
then `chmod +x csd-pool-miner-linux-<variant>` and run it with `--address
<YOUR_ADDR20>`. For 24/7 rigs, `mine-all-gpus.sh` (every card) and `mine-auto.sh`
(every card + auto-update) are fetched alongside the installer.

> **Where the installer puts the binary.** The one-command installer saves the
> miner to `~/.local/share/csd-pool-miner/csd-pool-miner-linux-<variant>` — this
> path is **not on your `PATH`**, so call it with its full path (the examples
> below use a `MINER=` shortcut). The launchers (`mine-auto.sh` /
> `mine-all-gpus.sh`) already know this path; you only need it for ad-hoc runs.

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

The installer leaves the binary at
`~/.local/share/csd-pool-miner/csd-pool-miner-linux-<variant>` (**not on `PATH`**).
Set a shortcut to the variant you installed, then run it by full path:

```sh
MINER=~/.local/share/csd-pool-miner/csd-pool-miner-linux-cpu   # or -nvidia / -amd
"$MINER" --address <YOUR_ADDR20>
```

That's it. The miner will:

1. auto-detect the best backend (tries CUDA → OpenCL → CPU),
2. connect to the CSD pool,
3. start submitting shares for `<YOUR_ADDR20>`.

### Choosing a backend

Auto-detect is the default. To force one:

```sh
"$MINER" --address <YOUR_ADDR20> --backend auto      # default: cuda -> opencl -> cpu
"$MINER" --address <YOUR_ADDR20> --backend cuda       # NVIDIA
"$MINER" --address <YOUR_ADDR20> --backend opencl     # AMD / other
"$MINER" --address <YOUR_ADDR20> --backend cpu        # CPU only
```

### Useful extras

```sh
"$MINER" devices         # list detected GPUs (handy if auto keeps picking CPU)
"$MINER" --list-devices  # same list, as a flag
"$MINER" selftest        # cross-check every backend vs the reference CPU hasher + probe pool reachability
```

### Rig / worker name (per-rig dashboard stats)

Give each rig a name and the pool dashboard shows it per-rig instead of lumping
every machine under one address:

```sh
"$MINER" --address <YOUR_ADDR20> --worker rig1
```

The name is sent as the Stratum worker (`<address>.rig1`) — **display-only,
payouts always go to the bare address**. Allowed chars `A-Za-z0-9_-`, max 24
(anything else is dropped; a `.suffix` is cut). If you don't pass `--worker`,
the miner falls back to env `CSD_WORKER`, then `WORKER_NAME` (HiveOS), then
this machine's hostname. Turn the suffix off entirely (authorize as the bare
address, like older miners) with `--no-worker` or `CSD_NO_WORKER=1`.

> **`solo` is a reserved worker name.** `--worker solo` (or `WORKER_NAME=solo`)
> switches the rig into [**solo mining**](#solo-mining) — it changes *how* your
> address is credited, not just the display label. Every other name is
> display-only as above.

### Solo mining

Prefer to go for the **whole block** yourself instead of a steady pool cut? Set
your worker name to the reserved keyword `solo`:

```sh
"$MINER" --address <YOUR_ADDR20> --worker solo
```

Your miner authorizes as `<address>.solo` and mines **solo**:

- Your shares are **excluded** from the pool (PPLNS) split.
- If *your* rig solves a block, **you** are credited the **full block reward
  minus the 2.5% pool fee** — no sharing — once the block confirms (15
  confirmations), paid on the normal payout schedule.
- If you don't solve one, you earn nothing that round. Solo is **high-variance**:
  it shines with serious hashrate (or if you feel lucky), while pool mining pays
  smoother. Your payout **address is unchanged** — `solo` only changes *how* that
  address is credited, never *which* address.

Switch back to pool mining with any other worker name (e.g. `--worker rig1`) or
none. **HiveOS:** add `--worker solo` to your *Extra config arguments* (see
[docs/HIVEOS.md](docs/HIVEOS.md)).

Watch your results on the pool dashboard: your per-rig stats (including the `solo`
rig) are under **"Your Miner"**, and solo block wins show up in **blue** on the
**Winners** board (`/winners.html`).

## Multi-GPU rigs

**One miner process drives exactly one GPU.** The card is picked with `--device N`
(0-based, default `0`). There is **no flag that makes a single process use every
card** — an 8-GPU rig runs **8 processes**, all with the *same* `--address`, each
with its own `--device` and its own `--stats-port`. The pool sums their shares
automatically; you are one miner as far as payouts are concerned.

`--gpu-id` is **not** the device selector. It is an **include-list the *launcher*
reads** to decide which cards to spawn a process for — useful to skip a card that
is unstable or already busy. The miner itself only validates it and logs it.

```sh
"$MINER" --list-devices        # index -> card, so you know what N means
```

### HiveOS — automatic, nothing to configure

`h-run.sh` fans out on its own: it counts the cards (`nvidia-smi -L`, or `clinfo`
on AMD), launches devices `1..N-1` in the background with `--device <i>
--stats-port <3380+i>`, then `exec`s device 0 on `3380`. Shipped since v0.1.19 —
**apply the Flight Sheet and every card mines.** Every probe is fail-soft: if
anything fails the rig falls back to card 0 only, never a brick.

> ⚠️ **Never put `--device` in *Extra config arguments*.** The launcher already
> emits `--device <i>` *before* your extra flags, and the **last** `--device` on
> the command line wins — so a `--device 0` in that box overrides the launcher's
> per-card assignment and collapses **every** process onto card 0. The rig looks
> like it is mining N GPUs and mines one. Current glue strips a stray `--device`
> and warns, but the glue only refreshes on a Flight-Sheet re-apply. Choose cards
> with `--gpu-id`, never `--device`.

> **Keep *Extra config arguments* on ONE line, space-separated.** `h-run.sh`
> matches ` --gpu-id ` (or `--gpu-id=`) inside a single flag string. Split your
> arguments across newlines and the match fails and the launcher **silently
> falls back to "all cards"** — no warning. Usually harmless, but if you were
> trying to skip a bad card it will keep mining on it.

> **Careful with the rig name `solo`.** HiveOS bakes its `$WORKER_NAME` — your
> rig's name — into the config as the worker, and `solo` is a **reserved opt-in
> mode**, not a label: a rig named `solo` is [solo mining](#solo-mining), its
> shares are excluded from the PPLNS split, and it earns nothing unless it
> solves a whole block. Don't name a rig `solo` unless you mean it.

Full HiveOS detail (including how to skip a bad card): **[docs/HIVEOS.md](docs/HIVEOS.md#multi-gpu-rigs)**.

### Plain Linux — one process per card

Six cards, six processes, six stats ports:

```sh
MINER=~/.local/share/csd-pool-miner/csd-pool-miner-linux-nvidia
BASE=~/.local/share/csd-pool-miner
ADDR=<YOUR_ADDR20>

for i in 0 1 2 3 4 5; do
  mkdir -p "$BASE/gpu$i-log"
  "$MINER" --address "$ADDR" --worker "rig1-GPU$i" \
    --device "$i" --stats-port $((3380 + i)) --stats-bind 127.0.0.1 \
    --log-dir "$BASE/gpu$i-log" \
    >> "$BASE/gpu$i-log/stdout.log" 2>&1 &
done
```

Count the cards instead of hardcoding six:

```sh
N=$(nvidia-smi -L | grep -c '^GPU ')      # AMD: clinfo | grep -c 'Device Type.*GPU'
for i in $(seq 0 $((N - 1))); do … ; done
```

The shipped launchers (`mine-auto.sh`, `mine-all-gpus.sh`) do this fan-out for you
and add auto-update — restrict them to specific cards with `CSD_GPU_IDS=0,2`
(they forward it as `--gpu-id`).

> **They do not pass `--stats-port`.** Under `mine-auto.sh` / `mine-all-gpus.sh`
> the stats endpoint is **off**, so `csd-dashboard.sh` has nothing to read. If you
> want telemetry on a non-HiveOS Linux rig, launch by hand as above or use the
> systemd units.

**systemd:** use the templated unit, one instance per card:

```sh
sudo systemctl enable --now csd-pool-miner@0 csd-pool-miner@1 csd-pool-miner@2
```

> ⚠️ The template reads one shared `CSD_STATS_PORT`, so every instance binds
> **3380** unless you give each card its own `/etc/csd-pool-miner.<i>.env` with a
> distinct port. See [`deploy/systemd/README.md`](deploy/systemd/README.md).

### Plain Windows — one process per card

```bat
set MINER=%LOCALAPPDATA%\csd-pool-miner\csd-pool-miner-nvidia.exe
set ADDR=<YOUR_ADDR20>

for /L %%i in (0,1,5) do start "CSD GPU%%i" "%MINER%" --address %ADDR% ^
  --worker rig1-GPU%%i --device %%i --stats-port 338%%i ^
  --log-dir "%LOCALAPPDATA%\csd-pool-miner\gpu%%i-log"
```

(The `338%%i` port trick is good for up to 10 cards; past that, write the lines
out.) `mine-all-gpus.bat` does the same fan-out — also without `--stats-port`.

### ⚠️ HiveOS under-reports ~1/N until you re-apply the Flight Sheet

`h-stats.sh` used to scrape a **single** stats port (`CUSTOM_API_PORT`, default
`3380`), which is **device 0's**. The extra cards report on `3381`, `3382`, … and
were never summed, so an N-card rig reads roughly **1/N of its real hashrate** on
the HiveOS dashboard. The current glue probes the whole port range and merges
what it finds, one `hs[]` entry per card.

> **The fix does not reach your rig on its own — re-apply the Flight Sheet.**
> `h-run.sh` self-updates the **miner binary** only (see
> [Notes](docs/HIVEOS.md#notes)); the HiveOS glue around it (`h-run.sh`,
> `h-config.sh`, `h-stats.sh`) is installed from the tarball and **only refreshes
> on a Flight-Sheet re-apply**. A rig that has been up since before the fix keeps
> reporting ~1/N however current its binary is. Re-apply once — that is the whole
> procedure.

After a re-apply the HiveOS tile **should** show a rate per card and a total in
line with the pool. That path has not yet been confirmed on a real multi-GPU rig,
so treat it as expected-not-proven: **if your tile still reads low after a
re-apply, please tell us** — open an issue with the output of the per-port probe
(step 2 below) and of `h-stats.sh` itself.

**The cards are mining either way — only the number is wrong.** The pool has
always counted every card. Measured in the field on a real 6-GPU rig: HiveOS
showed **1.64 GH/s** while the pool measured **11.68 GH/s** from that rig's
submitted shares — a ~7× under-report on a rig that was completely healthy. Do
not restart or tear down a rig on the strength of the HiveOS tile; confirm
against the pool dashboard first (step 5 below).

### Verifying every card is actually mining

**1. Count the processes** — one per card:

```sh
pgrep -a 'csd-(gpu|pool)-miner' | wc -l
```

**2. Ask each stats port directly** (the miner scrapes its own endpoint; no `jq`
needed, and it never hangs):

```sh
for p in 3380 3381 3382 3383 3384 3385; do
  printf '%s: ' "$p"
  "$MINER" hiveos-stats --stats-port "$p"      # HiveOS: /hive/miners/custom/csdpool/csd-gpu-miner
done
```

A card that is mining reports a non-zero `hs` value. An all-zero object means
that process is missing or wedged.

**3. Per-GPU logs** — each background process writes its own file:

```sh
tail -n 20 ~/.local/share/csd-pool-miner/gpu*-log/*.log     # plain Linux
tail -n 20 /var/log/miner/csdpool/gpu*.log                  # HiveOS (device 0 is csdpool.log)
grep 'multi-GPU' /var/log/miner/csdpool/*.log               # HiveOS: what the launcher spawned
```

**4. The hardware itself** — every card should show high utilisation and power
near its limit:

```sh
nvidia-smi --query-gpu=index,name,utilization.gpu,power.draw --format=csv
```

**5. The pool dashboard** — the authoritative number, and the one to check
*first* when HiveOS looks wrong. Your per-rig hashrate under **"Your Miner"** is
derived from the shares your rig actually submitted, so it is a measurement of
real work, not a scrape of a local endpoint — it counts every process regardless
of what HiveOS displays. A real 6-GPU rig read **1.64 GH/s** on the HiveOS tile
while the pool measured **11.68 GH/s** from its shares; the rig was fine and the
tile was wrong. **Believe the pool.**

### Worker names on a multi-GPU rig

Each process authorizes as `<address>.<worker>` and shows up under that name on
the pool dashboard. On a rig you control the argv for, **give every card its own
name**:

```sh
--worker rig1-GPU0    --worker rig1-GPU1    …    --worker rig1-GPU7
```

This is worth the two minutes. A rig reporting under one name tells you "the rig
lost a third of its hashrate"; per-card names tell you "GPU3 is dead". One
production miner on this pool runs **8× V100S** exactly this way, one worker per
card. Names allow `A-Za-z0-9_-`, max 24 chars, and payouts always go to the bare
address regardless of the name.

> **HiveOS bakes ONE worker name for the whole rig** (from the HiveOS
> `$WORKER_NAME`, into the config every card shares), so per-card names are not
> available under the HiveOS glue — all cards report under the rig name.

> **Per-card names and [solo mining](#solo-mining) are mutually exclusive.** Only
> the exact name `solo` is solo; `rig1-GPU0` pool-mines. A solo rig's cards all
> report as `solo`.

## Monitoring (stats endpoint + Discord)

Expose an **xmrig-`/1/summary`-compatible** JSON endpoint for dashboards
(Awesome Miner, Home Assistant, custom scrapers) — off unless you ask for it:

```sh
"$MINER" --address <ADDR> --stats-port 3380
# then GET  http://127.0.0.1:3380/1/summary   (and /healthz)
```

It binds **localhost only** by default. To expose it on your LAN, add
`--stats-bind 0.0.0.0` and protect it with `--stats-password <token>` (sent as
`Authorization: Bearer <token>` or `?token=<token>`); `/healthz` stays open.

The `connection` object also reports `reconnects` and `failovers` (lifetime
counts), so a dashboard can flag an unstable link or a flaky pool at a glance.

### Live terminal dashboard

Prefer a live, refreshing view over raw JSON? Point the bundled dashboard at the
same endpoint. It's a **read-only viewer** — it only GETs `/1/summary`, never
touches the miner, its config, or the share path, and never opens a non-loopback
socket. Worst case it prints "endpoint unreachable"; it cannot affect mining.

```sh
# Linux / macOS / HiveOS  (the miner must be running with --stats-port)
./csd-dashboard.sh                       # port 3380, refreshes every 2s
./csd-dashboard.sh --port 3380 --refresh 5
./csd-dashboard.sh --once                # print one frame and exit (scripts/cron)
```
```bat
:: Windows
csd-dashboard.bat
csd-dashboard.bat --port 3380 --refresh 5
```

It shows hashrate (10s / 1m / 15m), accepted / rejected / stale shares with
reject%, GPU temp + power (when the miner reports them), and reconnects /
failovers. **No `jq` required.** Override the port with `CSD_STATS_PORT`, or point
at a non-default host with `CSD_STATS_URL=http://host:port/1/summary`. The
dashboard self-updates with `--update` (fail-closed SHA-verify against the
release `SHA256SUMS`, same as the launchers).

Get a **Discord** ping when you pass an accepted-share milestone:

```sh
"$MINER" --address <ADDR> --discord-webhook https://discord.com/api/webhooks/...
```

Notifications are best-effort and non-blocking — a slow or dead webhook never
affects mining.

## HiveOS

A HiveOS **Custom-miner package** ships with each release
(`csdpool.tar.gz` — hyphen-free name so HiveOS's filename parser derives it cleanly;
the glue is in the [`hiveos/`](hiveos/) dir).
Add it as a Custom miner (miner name `csdpool`) and set your addr20 with
`--address <addr>` in *Extra config arguments* — the HiveOS "Wallet and worker
template" field does **not** work for CSD (HiveOS only passes it for coins it
knows). It reports hashrate and accepted/rejected shares to the HiveOS
dashboard (by scraping the miner's own stats endpoint, so the kH/s is always
correct).

**Step-by-step setup: [docs/HIVEOS.md](docs/HIVEOS.md).**

## Auto-update (24/7 rigs)

Every launcher keeps the binary current on its own — you never have to babysit a
version. They check the latest release with a proper **numeric semver compare**,
**verify the download's SHA-256 against the release `SHA256SUMS` before swapping
it in** (atomic — and they keep the running binary if verification fails), then
restart the miner on the new build:

- **`mine-auto.sh` / `mine-auto.bat`** — every card + auto-update + crash-restart
  (recommended for 24/7 rigs).
- **`mine-all-gpus.sh` / `mine-all-gpus.bat`** — every card; a background poll now
  applies verified updates and restarts the per-GPU miners.
- **The one-click installers** (`install-csd-miner.sh` / `.bat`) hand off to
  `mine-auto` after setup, so the one-click path polls for updates too.
- **HiveOS** now self-updates as well: `h-run.sh` checks at startup *and* on a
  background ~15-min poll, verifies, swaps, and bounces the miner so HiveOS
  relaunches on the new binary — no Flight Sheet change needed. See
  [docs/HIVEOS.md](docs/HIVEOS.md).
- **systemd (native Linux service)** — run the miner 24/7 under `systemd` with
  auto-restart and the same fail-closed auto-update timer. See
  [`deploy/systemd/`](deploy/systemd/) ([README](deploy/systemd/README.md)).

**Fail-safe is the rule everywhere:** any update failure (no network, GitHub
rate-limit, SHA mismatch, partial download, disk full) is logged and the rig
keeps mining on the binary it already has — an update problem can never strand or
brick a rig.

## Reliability (watchdog + auto-reconnect)

For 24/7 rigs the miner keeps itself connected to the official CSD pool and
earning without babysitting:

- **Connection watchdog** — if the link goes half-open (still gets work but
  silently drops your shares) or the pool stops sending new jobs, the miner forces
  a clean reconnect instead of mining into the void.
- **Won't-credit reconnect** — if the link keeps acking and pushing fresh work but
  stops *crediting* your shares for ~30 minutes, the miner forces a fresh
  reconnect on its own — at most once per window, so a transient hiccup can't
  cause a reconnect storm.

The 30-second health heartbeat in the log ends with
`conn=<reconnects>/<failovers>` so connection churn is visible at a glance, and
`selftest` TCP-probes the pool endpoint (PASS/FAIL) so you can confirm
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
# Optional rig name for per-rig dashboard stats (display-only; see --worker).
worker = "rig1"
# CPU threads to mine ALONGSIDE the GPU (dual mining). 0 = GPU-only.
cpu_threads = 0
```

**CPU usage on GPU builds:** by default a GPU build *also* mines on the CPU
(`cpu_threads = 16`) for extra hashrate, so you'll see high CPU use even while
the GPU works. To let the GPU do the work and keep your CPU free — recommended
on laptops, where the CPU and GPU share one power/thermal budget — set
`cpu_threads = 0` (or pass `--cpu-threads 0`).

## Payouts

Payouts are **batched by the pool about every 30 minutes**. Your shares accrue
continuously; the pool settles all eligible miners together each cycle, so you
won't see a payout the instant you find a share — wait for the next settlement.
The **pool fee is 2.5%**, and your balance must reach the **minimum payout of
0.001 CSD** to be included in a settlement (smaller balances roll over to the
next one).

## Where to get an addr20

`--address` is your **addr20** — your CSD payout address: **40 lowercase hex
characters** (an optional `0x` prefix is accepted).

**No address yet? Create a wallet in one step:**

- **Windows** — download & double-click **`create-wallet.bat`**
- **Linux** — `curl -fsSL https://raw.githubusercontent.com/dangraagu/CSD-Mining-pool-public/main/create-wallet.sh | bash`
- **Already have the miner?** — `~/.local/share/csd-pool-miner/csd-pool-miner-linux-<variant> newwallet`

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

**Future versions (v0.1.7 onward)** are licensed under the **PolyForm Perimeter
License 1.0.0** (see [`LICENSE`](LICENSE)). The source stays public so you can
read and audit it, but the license does **not** permit using it to build or
operate a competing product or pool, nor redistributing or reselling it.

This relicensing is **forward-only**. Already-published releases — **v0.1.6 and
earlier** — remain under **MIT OR Apache-2.0** (see [`LICENSE-MIT`](LICENSE-MIT)
and [`LICENSE-APACHE`](LICENSE-APACHE)); those grants are irrevocable and cannot
be clawed back. Only versions tagged v0.1.7 and later carry the new terms.

## Trademark

"CSD Pool Miner" and "Compute Substrate", and the associated names and logos, are
trademarks of the operator. The source license grants **no** rights to these
marks — see [`TRADEMARK.md`](TRADEMARK.md) for the policy (forks must rename and
remove the marks; nominative/referential use is fine).
