# Running csd-pool-miner as a native Linux service (systemd)

Run the miner 24/7 under `systemd` so it **auto-restarts** on a crash, a network
drop, an out-of-memory kill, or a **stalled GPU** — the GPU watchdog exits the
process with code **17** (`EXIT_GPU_STALLED`) to mean *"restart me"*, and these
units force a restart on exactly that code.

Two units are provided:

| File | Use it for |
|------|------------|
| `csd-pool-miner.service`   | A single GPU (one process). |
| `csd-pool-miner@.service`  | A multi-GPU rig — one instance per card (`@0`, `@1`, …). |

The miner is **one GPU per process** by design, so a 4-GPU rig runs four
instances of the templated unit. All instances share one payout address (the
pool sums their shares).

No extra software is required beyond the miner binary and your GPU vendor driver
(CUDA links at runtime for the NVIDIA build; an OpenCL runtime for AMD; the CPU
build needs neither).

---

## 1. Install the binary

Build it (or drop a release binary in place) and install it as
`/usr/local/bin/csd-pool-miner`:

```bash
# from a built checkout:
sudo install -m 0755 target/release/csd-gpu-miner /usr/local/bin/csd-pool-miner
# …or from a downloaded release asset (rename to the canonical name):
sudo install -m 0755 ./csd-pool-miner-linux-nvidia /usr/local/bin/csd-pool-miner
```

The unit's `ExecStart` is `/usr/local/bin/csd-pool-miner`; adjust the path in
the unit if you install elsewhere.

## 2. Create the unprivileged service account

The miner does **not** need root. Make a system account and give it access to
the GPU device nodes (the `video` group on most distros; some also use
`render`):

```bash
sudo useradd --system --no-create-home --shell /usr/sbin/nologin csdminer || true
sudo usermod -aG video csdminer        # GPU device access
# If your distro gates render nodes separately:
getent group render >/dev/null && sudo usermod -aG render csdminer
```

## 3. Set your payout address (EnvironmentFile)

The address lives **outside** the unit so the same packaged file works on every
rig. Copy the example and edit it:

```bash
sudo install -m 0640 deploy/systemd/csd-pool-miner.env.example /etc/csd-pool-miner.env
sudo chown root:csdminer /etc/csd-pool-miner.env
sudoedit /etc/csd-pool-miner.env       # set CSD_ADDRESS=<your 40-hex addr20>
```

`CSD_ADDRESS` is your **addr20** (40 lowercase hex characters; an optional `0x`
prefix is accepted). Keep the file `0640 root:csdminer` so only root and the
service can read it.

## 4. Install and start the unit

### Single GPU

```bash
sudo install -m 0644 deploy/systemd/csd-pool-miner.service /etc/systemd/system/
sudo systemctl daemon-reload
sudo systemctl enable --now csd-pool-miner
```

### Multi-GPU (templated)

Install the template once, then enable one instance per card. The instance name
is the **GPU device index**:

```bash
sudo install -m 0644 deploy/systemd/csd-pool-miner@.service /etc/systemd/system/
sudo systemctl daemon-reload
# e.g. a 3-GPU rig (cards 0,1,2):
sudo systemctl enable --now csd-pool-miner@0 csd-pool-miner@1 csd-pool-miner@2
```

Not sure how many cards the build sees? List them first:

```bash
sudo -u csdminer /usr/local/bin/csd-pool-miner --address $(printf 'a%.0s' {1..40}) devices
```

To pass per-card overrides, drop a `/etc/csd-pool-miner.<i>.env` file (loaded
after the shared one); e.g. `/etc/csd-pool-miner.1.env` only affects `@1`.

## 5. Watch it run

```bash
systemctl status csd-pool-miner            # or  csd-pool-miner@0
journalctl -u csd-pool-miner -f            # live logs
journalctl -u 'csd-pool-miner@*' -f        # all GPU instances
```

You should see it connect to the pool and start submitting shares. A healthy
miner logs accepted shares; the reliability + GPU watchdogs handle the rest.

---

## How the auto-restart works

- **`Restart=always`** — every exit (crash, OOM, network kill, even a clean
  stop that isn't `systemctl stop`) brings the process back.
- **`RestartSec=5`** — a few seconds of backoff so a hard crash-loop can't
  hammer the pool or the PSU.
- **`StartLimitBurst=5` / `StartLimitIntervalSec=60`** — if it dies more than 5
  times in 60 s, systemd parks it in `failed`. Investigate, then clear with:
  ```bash
  sudo systemctl reset-failed csd-pool-miner
  ```
- **`RestartForceExitStatus=17`** — the GPU watchdog's distinct exit code
  (`EXIT_GPU_STALLED`). When a GPU stalls (hashrate floors while fresh jobs flow
  over a healthy link) the miner first tries an in-process CUDA recovery and,
  failing that, exits **17**; systemd then restarts it clean. This number is
  kept in lock-step with the source by `tests/systemd_service.rs` — the build
  fails if the unit and the code ever disagree.

> Tuning the GPU watchdog is via CLI flags on `ExecStart`
> (`--gpu-floor`, `--gpu-watchdog-dwell`, `--gpu-watchdog-recover-secs`,
> `--gpu-watchdog-max-recoveries`, or `--no-gpu-watchdog` to disable). Use
> `systemctl edit csd-pool-miner` to add them without touching the packaged
> unit.

## Updating the miner

### Manual (one-off)

Replace the binary and restart:

```bash
sudo install -m 0755 target/release/csd-gpu-miner /usr/local/bin/csd-pool-miner
sudo systemctl restart csd-pool-miner          # or: restart 'csd-pool-miner@*'
```

### Automatic updates (recommended for a 24/7 rig)

The bare miner unit has **no** update path, so without this a systemd rig stays
frozen on whatever version you installed. A small **oneshot + timer** pair keeps
it current with the same fail-safe, fail-closed logic the `mine-auto.*`
launchers use:

| File | Role |
|------|------|
| `csd-pool-miner-update.sh`      | The update helper: resolve latest → verify → atomic swap → restart. |
| `csd-pool-miner-update.service` | `Type=oneshot` wrapper that runs the helper. |
| `csd-pool-miner-update.timer`   | Fires the oneshot ~every 15 min (and once after boot). |

How it stays safe:

- **Latest is resolved over the CDN**, not the GitHub API. It GETs
  `releases/latest/download/latest-version.txt`, which is **not** subject to the
  unauthenticated API's 60-requests/hour/IP limit — so a whole farm behind one
  public IP keeps updating instead of silently freezing on HTTP 403.
- **Integrity is fail-CLOSED.** The download is staged to a temp path and
  SHA-256-verified against the release `SHA256SUMS` (via the installed binary's
  `verify-file`, else the OS `sha256sum`) **before** the atomic swap. A missing
  `SHA256SUMS`, an unlisted asset, a digest mismatch, or no trusted verifier all
  **refuse** the update — the rig keeps running its existing, known-good binary.
- **It can't brick the miner.** The updater is a *separate* unit from the miner.
  It only ever touches the miner via `systemctl restart` **after** a verified
  swap; any failure logs and exits 0, leaving the running miner untouched.

Install it (after the miner unit is in place):

```bash
# 1. the helper script (note: installed name has NO .sh extension)
sudo install -m 0755 deploy/systemd/csd-pool-miner-update.sh /usr/local/bin/csd-pool-miner-update

# 2. the oneshot + timer
sudo install -m 0644 deploy/systemd/csd-pool-miner-update.service /etc/systemd/system/
sudo install -m 0644 deploy/systemd/csd-pool-miner-update.timer   /etc/systemd/system/

# 3. enable the timer (this does NOT need to touch the running miner)
sudo systemctl daemon-reload
sudo systemctl enable --now csd-pool-miner-update.timer
```

Pick which build to track (optional). By default the helper auto-detects a GPU
(NVIDIA → `nvidia`, AMD/OpenCL → `amd`, else `cpu`) and falls back to the `cpu`
asset on a 404. To pin it, add a line to `/etc/csd-pool-miner.env` (the updater
reads the same EnvironmentFile as the miner):

```ini
CSD_UPDATE_VARIANT=nvidia      # nvidia | amd | cpu
```

Watch / control it:

```bash
systemctl list-timers csd-pool-miner-update.timer     # when it next fires
journalctl -u csd-pool-miner-update.service -f        # what each run did
sudo systemctl start csd-pool-miner-update.service    # force a check right now
sudo systemctl disable --now csd-pool-miner-update.timer   # stop auto-updates
```

> A successful update restarts `csd-pool-miner` (and any running
> `csd-pool-miner@<i>` instances) onto the new binary. If you'd rather pin a
> version, leave the timer disabled and update manually as above.

## Uninstall

```bash
sudo systemctl disable --now csd-pool-miner            # and/or csd-pool-miner@0 …
# if you enabled automatic updates:
sudo systemctl disable --now csd-pool-miner-update.timer
sudo rm /etc/systemd/system/csd-pool-miner.service
sudo rm /etc/systemd/system/csd-pool-miner@.service
sudo rm -f /etc/systemd/system/csd-pool-miner-update.service
sudo rm -f /etc/systemd/system/csd-pool-miner-update.timer
sudo rm -f /usr/local/bin/csd-pool-miner-update
sudo systemctl daemon-reload
# optional cleanup:
sudo rm -f /etc/csd-pool-miner.env
sudo rm -rf /var/lib/csd-pool-miner
sudo userdel csdminer
```

## Hardening notes

The units apply light sandboxing that is safe for a GPU miner
(`NoNewPrivileges`, `ProtectSystem=strict`, `ProtectHome`, `PrivateTmp`, a
dedicated `StateDirectory`). We deliberately do **not** set `PrivateDevices=yes`
— that would hide the GPU device nodes the miner needs. If your specific driver
stack rejects one of the `Protect*` lines (rare), comment it out in a drop-in;
none of them are required for correct mining.
