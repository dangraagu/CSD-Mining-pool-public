# CSD Pool Miner — Fleet Release Checklist

**Status:** authoritative release gate for the public fleet miner
(`dangraagu/CSD-Mining-pool-public`). This is the single entry point for cutting
a public release — do not tag from memory.

**Why this doc exists (read first).** Fleet miner releases **auto-update with no
clawback**: the moment a tag's assets publish, every rig's `h-run.sh` (HiveOS)
and `mine-auto.{sh,bat}` (systemd/Windows) self-updates to it within ~15 min
(see `docs/HIVEOS.md` "Updating is now automatic"). A bad tag reaches the whole
fleet before anyone can react — there is no recall. This is exactly how:

- **v0.2.0** shipped Linux binaries linked against the runner's glibc 2.39 and
  **crash-looped ~24 % of the HiveOS fleet** (`GLIBC_2.39 not found`, exit-1
  relaunch loop) — hotfixed in v0.2.1 (see `CHANGELOG.md`).
- **v0.1.11** shipped a `.bat` that bricked, caught by an adversarial reviewer
  ~1 min after publish and reverted.

The machine gates below (CI) are necessary but **not sufficient** — the three
gates that actually caught these failures are **human**: execute-in-old-glibc,
real-rig canary, and tag-only-on-explicit-go. **Every box marked `[BLOCKING]`
must pass before the tag is pushed. Default to NOT tagging on any uncertainty.**

**Legend:** `[BLOCKING]` = release stops here on failure. `[CI]` = enforced
automatically by `.github/workflows/release.yml` on the tag push (verify it went
green; do not treat as done just because it is automated). `[HUMAN]` = a person
must run it and read the output — no automation covers it.

---

## Phase 0 — Preconditions (before you build anything)

- [ ] **Working tree clean and on the intended commit.** The fix is already
      merged to `origin/main` and you are releasing that exact commit — not a
      dirty local tree.
      ```bash
      git fetch origin --tags
      git status --porcelain          # must be empty
      git log --oneline -5 origin/main
      ```
- [ ] **Confirm the pool endpoint is the production one.** The endpoint is
      compiled in and cannot be repointed at runtime (see hardening §H1). Verify
      it is the live pool before shipping:
      ```bash
      cargo test --test endpoint_locked   # pool/url/host/... flags stay rejected
      grep -n 'ENDPOINT\|pool_endpoint' src/endpoint.rs
      ```
- [ ] **Decide the version** (semver bump in `Cargo.toml`, no leading `v`). The
      tag is `v<version>`; `latest-version.txt` (CI-generated) is the bare
      version the launchers poll.

## Phase 1 — Docs reconciliation `[HUMAN]` (not machine-enforced — this drift is silent)

Do this **before** tagging: `latest-version.txt` and the release assets go out
atomically, so stale docs ship with the binary.

- [ ] **Update `CHANGELOG.md`** — new top section `## v<ver> — <date> — <title>`
      with a plain-language what/why per change, matching the existing entries'
      style.
- [ ] **Reconcile `README.md` + `docs/HIVEOS.md` against the LIVE bridge**, not
      against memory. Verify these match production (fee, flags, payout cadence,
      solo semantics) by reading the live pool, not this repo:
      - Fee % (README/HIVEOS solo section both cite **2.5 %** — confirm against
        the running bridge, not this file).
      - Payout cadence + `min_payout` + confirmation depth.
      - Any new/removed CLI flag (`--backend`, `--address`, `--worker solo`,
        `--stats-port`, …) — every flag named in the docs must exist in the
        shipped `Cli`.
      - The HiveOS Installation URL stays the **non-staling**
        `releases/latest/download/csdpool.tar.gz` form (never a pinned tag).
- [ ] **If a flag or endpoint changed**, update `RELEASE-v*.bat` header notes and
      any `deploy/systemd/*.env.example` defaults in the same commit.

## Phase 2 — Build glibc-2.27-portable binaries `[HUMAN build]` + `[CI]`

**The floor is glibc 2.27** (stock HiveOS = Ubuntu 18.04 base = glibc 2.27; 20.04
= 2.31). A plain `cargo build` links the build host's glibc and crash-loops old
rigs — this is the v0.2.0 failure. Use `cargo-zigbuild` with an explicit
`.2.27` target triple so the floor is pinned regardless of build-host glibc.

- [ ] **Local Linux build in WSL (Ubuntu) — glibc-2.27 pinned:**
      ```bash
      pip3 install cargo-zigbuild==0.23.0 || pip3 install --break-system-packages cargo-zigbuild==0.23.0
      rustup target add x86_64-unknown-linux-gnu
      # CPU (universal seed) — always builds, no GPU toolchain:
      cargo zigbuild --release --target x86_64-unknown-linux-gnu.2.27
      # NVIDIA (CUDA embeds prebuilt PTX; JITs via driver, no CUDA Toolkit needed):
      cargo zigbuild --release --target x86_64-unknown-linux-gnu.2.27 --features cuda,nvml
      # Output lands in target/x86_64-unknown-linux-gnu/release/ (NOT target/release/)
      ```
      > Note: a `--target` build outputs to
      > `target/x86_64-unknown-linux-gnu/release/csd-gpu-miner`, and the NVIDIA/AMD
      > builds overwrite that path in place. Preserve the CPU build separately if
      > you bundle it as the HiveOS seed (CI does this via
      > `csd-gpu-miner-hiveos-seed`).
- [ ] **`[CI]` The release workflow rebuilds all variants the same way** on the
      tag push (`release.yml` `linux:` job, `cargo zigbuild ... .2.27`). The local
      build above is your pre-flight; CI is the artifact source of record.

## Phase 3 — CI objdump GLIBC-floor gate `[CI][BLOCKING]`

- [ ] **`glibc floor gate` passes** (`release.yml:269-300`). It runs
      `objdump -T` over **every** `dist/csd-pool-miner-linux-*` binary **and** the
      HiveOS seed, takes the max `GLIBC_*` symbol version each needs, and **fails
      the release closed** if any exceeds `2.27`. Reproduce locally before tagging:
      ```bash
      for f in target/x86_64-unknown-linux-gnu/release/csd-gpu-miner; do
        objdump -T "$f" | grep -oE 'GLIBC_[0-9.]+' | sort -Vu | tail -1
      done
      # Every value MUST be <= GLIBC_2.27
      ```
      > objdump proves the binary *declares* no symbol newer than 2.27. It does
      > **not** prove the binary actually runs — that is Phase 5. objdump alone is
      > what let v0.2.0 through; it is necessary, not sufficient.

## Phase 4 — Anti-copy / Windows-Defender-safe hardening `[HUMAN verify]`

Required on **every** release (miner + bundled relay). **No packers, no
obfuscators, no crypters** — those trip Windows Defender heuristics and get the
whole fleet's binary quarantined. Confirm each property still holds:

- [ ] **H1 — Endpoint lock.** The pool is compiled in; no runtime flag can
      repoint it. Regression-guarded:
      ```bash
      cargo test --test endpoint_locked
      # asserts --pool/--url/--host/--server/--node/--endpoint/--connect all rejected
      ```
- [ ] **H2 — PolyForm Perimeter license present.** `LICENSE` is PolyForm
      Perimeter 1.0.0 (competing-use prohibited) and `Cargo.toml`
      `license-file = "LICENSE"`; `NOTICE`/`TRADEMARK.md` intact. Verify a
      downstream clone that strips the NOTICE is a license breach (evidence for
      any DMCA — see the Pacer clone incident).
- [ ] **H3 — Build-path scrub.** `.cargo/config.toml` `--remap-path-prefix`
      rewrites the dev home dir (Windows, MSYS, and `/mnt/c` spellings) to
      `/build` so the username does not leak in panic metadata. Confirm no leak
      in the shipped binary:
      ```bash
      strings dist/csd-pool-miner-linux-cpu | grep -i 'bahs_admin' || echo "clean (no home-dir leak)"
      ```
- [ ] **H4 — Stripped release.** `Cargo.toml [profile.release]` has
      `strip = true`, `lto = "fat"`, `codegen-units = 1`. Confirm the shipped
      binary carries no symbol table:
      ```bash
      file dist/csd-pool-miner-linux-cpu          # expect "stripped"
      ```
- [ ] **H5 — SHA-pin the bundled relay.** `csd-relay-node` is a **prebuilt pinned
      asset**, not built here; `release.yml` (`:316-370`) fetches it and
      **hard-fails on SHA256 mismatch** (`RELAY_NODE_SHA256`). Before tagging,
      confirm `RELAY_NODE_VERSION` + `RELAY_NODE_SHA256` in `release.yml` point at
      the intended relay release and the SHA matches:
      ```bash
      sha256sum <the-relay-binary-you-intend-to-ship>   # must equal RELAY_NODE_SHA256
      ```
- [ ] **H6 — No packer/obfuscator introduced.** Diff dependencies vs the last
      release; reject any UPX/crypter/obfuscation step. TLS is the only network
      dep (`ureq` rustls) — nothing else opens a network endpoint.

## Phase 5 — Execute in an old-glibc WSL sandbox `[HUMAN][BLOCKING]` — the "csd-hiveos-sandbox" gate

**objdump (Phase 3) is not enough** — it proves declared symbols, not that the
binary boots. **Actually RUN each Linux binary on a glibc-2.27/2.31 userland**
before tagging. This is the single gate that would have stopped v0.2.0.

- [ ] **Launch each Linux variant inside the old-glibc sandbox** (WSL Ubuntu-18.04
      rootfs or an equivalent glibc-2.27 container) and confirm it starts, probes
      the GPU/CPU, and does **not** crash-loop:
      ```bash
      # inside the glibc-2.27 sandbox:
      ldd --version | head -1                      # confirm 2.27 / 2.31
      ./csd-pool-miner-linux-cpu    --address <40-hex addr20> --backend cpu   &   # boots, no GLIBC error
      ./csd-pool-miner-linux-nvidia --address <40-hex addr20> --backend cuda  &   # if an NVIDIA rig is reachable
      # WATCH FOR: "version 'GLIBC_2.xx' not found" (FAIL) or an exit-1 relaunch loop (FAIL)
      ```
- [ ] **The HiveOS seed boots too** — it is the binary every fresh install first
      runs. Run `csd-gpu-miner-hiveos-seed` (= the CPU build) in the sandbox.
- [ ] **No `GLIBC_* not found`, no immediate exit, no relaunch loop** on any
      binary. Any such failure `[BLOCKING]` — do not tag.

## Phase 6 — Real-rig accepted-share canary `[HUMAN][BLOCKING]` — BEFORE the tag, never after

**Prove the built binary actually earns accepted shares on real hardware before
it reaches the fleet.** Run the exact artifact you are about to publish on a live
rig pointed at the production pool.

- [ ] **Run the candidate binary on the 5070 Ti dev rig** (or the matching arch
      for the variant under test) against the live pool, and confirm **accepted
      shares accrue with no reject/stale spike**:
      ```bash
      ./csd-pool-miner-<variant> --backend cuda --address <your 40-hex addr20>
      # In another shell, watch the miner's own stats endpoint:
      curl -s 127.0.0.1:3380/1/summary | grep -E 'shares_good|shares_rejected|shares_stale|reconnects'
      ```
- [ ] **Pass criterion (write it down before you start):** **≥ N accepted shares
      over ≥ M minutes** (suggested: ≥ 30 accepted shares over ≥ 30 min) with
      **reject rate and stale rate not elevated vs the current fleet baseline**
      and **no reconnect storm**. Anything less `[BLOCKING]`.
- [ ] **For a Volta/PTX-floor change** (e.g. lowering the fallback PTX arch),
      additionally canary on a real device of that arch (an already-connected
      `C-V100S` rig) and confirm the fatbin loads (no `load_module failed`
      → CPU/OpenCL fallback) and shares are accepted through the intended backend.
- [ ] **Selftest is bit-exact** on every arch you touched, including the JIT
      fallback path:
      ```bash
      cargo test                                  # selftest vs CPU reference
      CUDA_FORCE_PTX_JIT=1 cargo test             # exercise the PTX fallback path
      ```

## Phase 7 — Signed checksums + non-staling distribution `[CI][BLOCKING]`

- [ ] **`[CI]` One `SHA256SUMS` over every published asset** is generated by the
      single `release:` publisher job (`release.yml:496-513`), excluding
      `SHA256SUMS` itself. This is the integrity anchor the auto-updater verifies
      every downloaded binary against **fail-closed** — a rig refuses to swap in a
      binary whose SHA is absent or mismatched. Without it the self-update is a
      no-op, so this job is load-bearing.
- [ ] **`[CI]` Stable-named + versioned assets both publish** so the launchers'
      **non-staling** `releases/latest/download/` URLs resolve to the current
      release: `csdpool.tar.gz`, `latest-version.txt`, `mine-auto.{sh,bat}`,
      `csd-dashboard.{sh,bat}` (`release.yml` Package + Stage steps).
- [ ] **Sign `SHA256SUMS` (recommended).** If release signing is configured,
      attach a detached signature (`SHA256SUMS.asc`) so rigs/operators can verify
      provenance, not just integrity. If not yet wired into CI, treat the pinned
      relay SHA (H5) + the CI-generated `SHA256SUMS` as the integrity floor and
      track "GPG-sign SHA256SUMS in CI" as a follow-up — do **not** hand-sign and
      hand-upload a checksums file out of band (that breaks the single-publisher
      guarantee).

## Phase 8 — Tag ONLY on explicit owner go `[HUMAN][BLOCKING]` — the no-clawback gate

**Every gate above is green, or you do not tag.** Because the fleet auto-updates
with no clawback, the tag push is the point of no return.

- [ ] **All `[BLOCKING]` boxes above are checked** (docs reconciled, glibc floor
      gate green, hardening verified, sandbox-executed, real-rig canary passed,
      checksums covered).
- [ ] **Adversarial review of the diff is complete** and its findings are
      resolved (per the standing no-clawback publish-gate rule — a `.bat` brick or
      a fleet-bricking regression must be caught here, not by the fleet).
- [ ] **Explicit human "go" to tag.** Do not self-authorize. Batch fixes into one
      release; do not churn the fleet with back-to-back tags.
- [ ] **Push the tag via the release `.bat`** (it re-checks tag-free + fix-on-main
      and requires typing `YES`), or manually:
      ```bash
      git tag -a v<ver> <commit> -m "v<ver> — <summary>"
      git push origin v<ver>          # THIS publishes; auto-updating rigs WILL pull it
      ```

## Phase 9 — Post-publish verification `[HUMAN]`

- [ ] **CI build went fully green** (all `[CI][BLOCKING]` jobs), assets present on
      the release page, and `SHA256SUMS` lists every asset:
      ```bash
      gh run watch                                # or watch the Actions tab
      gh release view v<ver> --json assets -q '.assets[].name'
      curl -sL https://github.com/dangraagu/CSD-Mining-pool-public/releases/latest/download/latest-version.txt
      # ^ must equal <ver>
      ```
- [ ] **A real rig picks up the update within ~15 min** and keeps mining on the
      new build (accepted shares continue). Confirm on a canary rig's launcher log
      before declaring the release good:
      ```bash
      tail /var/log/miner/csdpool/*.log | grep '\[h-run\]'
      ```
- [ ] **Watch the fleet for the first ~30 min** for any spike in offline rigs,
      0 H/s rigs, or reject rate. If a regression appears, **roll forward with a
      hotfix tag** (there is no clawback) — do not assume rigs will revert.

---

### Cross-references
- CI gates: `.github/workflows/release.yml` — glibc floor gate (`:269-300`),
  relay SHA-pin (`:316-370`), SHA256SUMS single publisher (`:496-513`).
- HiveOS packaging + auto-update behavior: `docs/HIVEOS.md`.
- Hardening rationale: `.cargo/config.toml` (path scrub), `Cargo.toml`
  `[profile.release]` (strip/lto), `LICENSE` (PolyForm), `tests/endpoint_locked.rs`.
- Publish helper + tag flow: `RELEASE-v0.1.7.bat`.
- Systemd/Windows self-updater: `mine-auto.{sh,bat}`,
  `deploy/systemd/csd-pool-miner-update.sh`.
