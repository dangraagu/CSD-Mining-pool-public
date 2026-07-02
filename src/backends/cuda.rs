//! CUDA mining backend — 2-stream pipelined.
//!
//! A pre-built PTX (`sha256d.ptx`, compiled offline from `sha256d.cu`) is loaded
//! via the driver's JIT (`cuModuleLoadData`) at startup — NVRTC is never invoked,
//! so only the NVIDIA driver is needed at runtime (no CUDA Toolkit). Two CUDA
//! streams alternate
//! kernel launches; while one stream is hashing the host reads back the
//! other stream's prior flag and decides whether to stop. The kernel
//! itself sweeps `nonces_per_thread` nonces per work-item.
//!
//! Default geometry on a modern desktop GPU:
//!   blocks=560 threads_per_block=256 nonces_per_thread=4096
//!   → 587 M nonces per launch
//!   → 2 launches blanket nearly all 4.29 G u32 nonces
//!
//! cudarc 0.19 API (CudaContext + per-stream ops). Was cudarc 0.11
//! (CudaDevice + device-level ops); 0.19 supports CUDA 13.x natively
//! which fixes the PTX_UNSUPPORTED error the old version hit against
//! the user's CUDA 13.1 driver / 13.2 toolkit.
//!
//! iter-hotpath #2: streams + device allocations live on `CudaBackend`
//! itself, set up once in `new()`. `hash_range` only memcpys the
//! per-launch inputs (midstate, tail_16, target, zeroed found_flag)
//! into the persistent buffers — no `new_stream` / `alloc_zeros` /
//! `clone_htod` thrashing on every template refresh.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use anyhow::{anyhow, Result};
use cudarc::driver::{
    CudaContext, CudaFunction, CudaModule, CudaSlice, CudaStream, LaunchConfig, PushKernelArg,
};
use cudarc::nvrtc::Ptx;

use crate::backend::{DeviceError, MiningBackend, MiningResult};
use crate::backends::autotune::{
    self, candidate_geometries, pick_best, CachedGeometry, GeometryMeasurement,
};
use crate::gpu_watchdog::Recoverable;
use crate::sha256d_cpu::midstate_of_first_chunk_fast as midstate_of_first_chunk;

// CUDA kernel, pre-compiled to PTX *offline* (nvcc -ptx -arch=compute_75
// -maxrregcount=64 --use_fast_math; see scripts/build-ptx). We embed the PTX and
// let the NVIDIA driver JIT it to SASS at module-load time, so the CUDA backend
// needs ONLY the NVIDIA driver at runtime — NOT the CUDA Toolkit / nvrtc shared
// library. The PTX `.version` is pinned LOW (6.3, the sm_75 floor): a newer
// toolkit stamps a too-new ISA that older drivers reject with
// CUDA_ERROR_UNSUPPORTED_PTX_VERSION (→ silent CPU fallback). The kernel uses only
// old integer ops, so 6.3 is valid and loads on any CUDA-10.0+ driver.
// Regenerate sha256d.ptx with scripts/build-ptx after editing the .cu.
const KERNEL_PTX: &str = include_str!("../kernels/sha256d.ptx");
const KERNEL_NAME: &str = "mine_sha256d";

pub struct CudaBackend {
    // iter-hotpath #2: `ctx` and `module` are owned here ONLY to keep
    // the underlying CUDA resources alive for the lifetime of the
    // backend. After the refactor we no longer touch them from the hot
    // path (the streams + function handle inside `pipes` hold their own
    // Arc references), but dropping the owning Arc would invalidate the
    // device buffers / loaded module. `#[allow(dead_code)]` documents
    // that this is a deliberate ownership root, not a leftover field.
    #[allow(dead_code)]
    ctx: Arc<CudaContext>,
    #[allow(dead_code)]
    module: Arc<CudaModule>,
    pub blocks: u32,
    pub threads_per_block: u32,
    pub nonces_per_thread: u32,

    // iter-hotpath #2: persistent per-pipe state. Set up exactly once
    // in `new()` then reused for every `hash_range` invocation. The
    // Mutex enforces single-threaded access from `hash_range`
    // (`MiningBackend::hash_range` takes `&self`, so we need interior
    // mutability for the device buffers; in practice each backend is
    // owned by one mining thread so the lock is uncontended).
    pipes: Mutex<PipePair>,
}

/// Two pipes (A + B) plus the per-launch function handle. All members
/// outlive a single `hash_range` call — they are torn down only when
/// the backend is dropped.
struct PipePair {
    a: PipeRes,
    b: PipeRes,
    func: CudaFunction,
}

struct PipeRes {
    stream: Arc<CudaStream>,
    mid_dev: CudaSlice<u32>,
    tail_dev: CudaSlice<u8>,
    target_dev: CudaSlice<u8>,
    found_nonce: CudaSlice<u32>,
    found_flag: CudaSlice<u32>,
    found_hash: CudaSlice<u32>,
    in_flight: bool,
}

impl CudaBackend {
    pub fn new(
        device_index: usize,
        blocks: u32,
        threads_per_block: u32,
        nonces_per_thread: u32,
    ) -> Result<Self> {
        let ctx = CudaContext::new(device_index)
            .map_err(|e| anyhow!("cuda: open device {} failed: {}", device_index, e))?;
        let name = ctx.name().unwrap_or_else(|_| "<unknown>".into());
        tracing::info!("cuda device: {}", name);

        // iter-hotpath #1: switch the CUDA context from spin-wait
        // (default CU_CTX_SCHED_AUTO -> spin on Windows) to blocking
        // sync. Every `stream.synchronize()` call in `drain_pipe` was
        // pinning one CPU core 100% in a kernel-level spin loop while
        // we waited for the GPU to finish a launch. With two streams
        // alternating that's a whole physical core lost to the
        // bookkeeping thread — CPU mining gets ~7% of its potential
        // because of this. Blocking-sync trades 1-10 us of extra
        // kernel-mode latency for ~1 freed physical core. Worth it.
        ctx.set_blocking_synchronize()
            .map_err(|e| anyhow!("cuda: set_blocking_synchronize failed: {}", e))?;

        // Load the pre-compiled PTX (embedded as `KERNEL_PTX`) and hand it to the
        // driver's built-in PTX->SASS JIT. The PTX targets the compute_75 (Turing)
        // virtual arch, so the driver forward-JITs it onto EVERY CUDA-13-supported
        // NVIDIA GPU (Turing through Blackwell) — this one public binary works
        // across miners' cards. Crucially, loading pre-built PTX uses ONLY the
        // driver (cuModuleLoadData); it never touches NVRTC, so no CUDA Toolkit /
        // nvrtc shared library is needed at runtime. (The previous code
        // NVRTC-compiled the kernel at startup and silently fell back to CPU on
        // any miner without nvrtc.dll / libnvrtc — the reported "full CPU load" bug.)
        let ptx = Ptx::from_src(KERNEL_PTX);
        let module = ctx
            .load_module(ptx)
            .map_err(|e| anyhow!("cuda: load_module failed: {}", e))?;

        // iter-hotpath #2: build the two pipes (streams + device
        // buffers) ONCE here, then reuse for the lifetime of the
        // backend. Previously this work happened inside every
        // hash_range call — 2 stream creations + 8 device allocations
        // + 6 host->device copies every ~1-2s template refresh. The
        // CUDA driver-level cost of those calls (alloc handlers,
        // stream-create syscalls) shows up as ~14 driver calls per
        // template on the host side and as alloc fragmentation on the
        // device side over long runs.
        let a = build_pipe(&ctx).map_err(|e| anyhow!("cuda: pipe A setup failed: {}", e))?;
        let b = build_pipe(&ctx).map_err(|e| anyhow!("cuda: pipe B setup failed: {}", e))?;
        let func: CudaFunction = module
            .load_function(KERNEL_NAME)
            .map_err(|e| anyhow!("cuda: load_function {} failed: {}", KERNEL_NAME, e))?;

        Ok(Self {
            ctx,
            module,
            blocks: blocks.max(1).min(65535),
            threads_per_block: threads_per_block.max(64).min(1024),
            nonces_per_thread: nonces_per_thread.max(1),
            pipes: Mutex::new(PipePair { a, b, func }),
        })
    }

    fn nonces_per_launch(&self) -> u64 {
        self.blocks as u64 * self.threads_per_block as u64 * self.nonces_per_thread as u64
    }

    /// Attempt in-process recovery from a wedged GPU (driver hiccup, TDR reset,
    /// transient CUDA error) WITHOUT restarting the process. Called by the
    /// GPU-stall watchdog (`gpu_watchdog`) when hashrate has flatlined while
    /// fresh work is flowing over a healthy link.
    ///
    /// Strategy, cheapest-first: reload the module from the SAME context (a
    /// re-JIT of the embedded PTX, which clears a faulted module), reload the
    /// kernel function, and rebuild BOTH pipes (fresh CUDA streams + freshly
    /// allocated device buffers). The CUDA context itself is kept — a fresh
    /// `CudaContext::new` would race the in-flight `hash_range` that still holds
    /// the OLD context's resources, and most transient wedges (a single TDR, a
    /// stuck stream) clear with new streams/buffers on the existing context. We
    /// do NOT touch PoW/hash logic: the rebuilt module loads the IDENTICAL PTX
    /// and the same kernel, so every hash computed after recovery is bit-for-bit
    /// what it was before.
    ///
    /// Returns `true` if the rebuild succeeded (the watchdog then waits its grace
    /// window for hashrate to return), `false` if recovery itself failed (e.g.
    /// the context is unusable / the device fell off the bus) — in which case the
    /// watchdog escalates to a process exit for a supervisor restart. NEVER
    /// panics: any driver error becomes a `false`.
    pub fn recover(&self) -> bool {
        // Serialize against the mining thread via the same pipes lock
        // `hash_range` takes — but NON-BLOCKING (`try_lock`). Two failure modes,
        // both ⇒ return `false` so the watchdog escalates to a process restart
        // (the only thing that helps when we can't rebuild in-process):
        //   - POISONED: a prior panic left the lock poisoned.
        //   - WOULD-BLOCK: `hash_range` currently HOLDS the lock. The normal hot
        //     path releases it between launches, so a held lock here means the
        //     kernel is wedged INSIDE `hash_range` (blocked in `synchronize()`)
        //     and will not release it. Blocking on `lock()` would deadlock the
        //     watchdog; instead we bail and let it exit(EXIT_GPU_STALLED) — which
        //     tears down the wedged process regardless of the held lock.
        let mut pipes = match self.pipes.try_lock() {
            Ok(g) => g,
            Err(std::sync::TryLockError::WouldBlock) => {
                tracing::error!(
                    "cuda recover: hash_range is holding the pipes lock (kernel wedged inside a launch); cannot rebuild in-process — escalating to process restart"
                );
                return false;
            }
            Err(std::sync::TryLockError::Poisoned(e)) => {
                tracing::error!("cuda recover: pipes mutex poisoned ({e}); cannot recover in-process");
                return false;
            }
        };

        // Re-JIT the embedded PTX on the existing context (clears a faulted
        // module) and reload the kernel function from the fresh module.
        let ptx = Ptx::from_src(KERNEL_PTX);
        let module = match self.ctx.load_module(ptx) {
            Ok(m) => m,
            Err(e) => {
                tracing::error!("cuda recover: load_module failed: {e}");
                return false;
            }
        };
        let func = match module.load_function(KERNEL_NAME) {
            Ok(f) => f,
            Err(e) => {
                tracing::error!("cuda recover: load_function {KERNEL_NAME} failed: {e}");
                return false;
            }
        };

        // Rebuild both pipes: new streams + freshly allocated device buffers.
        let a = match build_pipe(&self.ctx) {
            Ok(p) => p,
            Err(e) => {
                tracing::error!("cuda recover: rebuild pipe A failed: {e}");
                return false;
            }
        };
        let b = match build_pipe(&self.ctx) {
            Ok(p) => p,
            Err(e) => {
                tracing::error!("cuda recover: rebuild pipe B failed: {e}");
                return false;
            }
        };

        // Swap in the rebuilt resources atomically under the lock. The old
        // PipePair (streams/buffers) drops here, freeing the wedged handles.
        //
        // Lifetime of the new module: cudarc's `CudaFunction` holds its own
        // `Arc<CudaModule>` (which in turn holds the `Arc<CudaContext>`), so the
        // freshly loaded `func` we store keeps the new module loaded for as long
        // as the pipes live — we do NOT need to write it back into the `&self`
        // `module` field (which we couldn't through a shared borrow anyway). The
        // local `module` binding here is therefore redundant after `func` is
        // built; it drops at end of scope, decrementing only the extra reference
        // we held. The backend's original `self.module` (the pre-recovery one)
        // stays loaded but unused — a bounded, at-most-`max_recoveries`
        // (default 3) idle module, acceptable for this rare path and far cheaper
        // than a full process restart.
        *pipes = PipePair { a, b, func };
        let _ = &module; // documents: func retains the new module; this ref is extra
        tracing::warn!("cuda recover: module re-JIT'd + both pipes rebuilt on existing context");
        true
    }
}

/// Allocate the persistent buffers for ONE pipe. Buffer sizes are
/// fixed by the kernel ABI: midstate is 8 u32s, tail_16 is 16 bytes,
/// target is 32 bytes, found_flag/found_nonce are 1 u32 each, found_hash
/// is 8 u32s. Buffers are zero-initialised here; `hash_range` overwrites
/// them with the per-launch input on every invocation.
fn build_pipe(ctx: &Arc<CudaContext>) -> Result<PipeRes, cudarc::driver::DriverError> {
    let stream = ctx.new_stream()?;
    let mid_dev: CudaSlice<u32> = stream.alloc_zeros::<u32>(8)?;
    let tail_dev: CudaSlice<u8> = stream.alloc_zeros::<u8>(16)?;
    let target_dev: CudaSlice<u8> = stream.alloc_zeros::<u8>(32)?;
    let found_nonce: CudaSlice<u32> = stream.alloc_zeros::<u32>(1)?;
    let found_flag: CudaSlice<u32> = stream.alloc_zeros::<u32>(1)?;
    let found_hash: CudaSlice<u32> = stream.alloc_zeros::<u32>(8)?;
    Ok(PipeRes {
        stream,
        mid_dev,
        tail_dev,
        target_dev,
        found_nonce,
        found_flag,
        found_hash,
        in_flight: false,
    })
}

/// Re-prime ONE persistent pipe with the current call's per-launch
/// inputs and clear the found-flag + in_flight state ready for a fresh
/// launch sequence. Cheap path: 3 small H2D memcpys, no allocations.
fn prime_pipe(
    pipe: &mut PipeRes,
    midstate: &[u32; 8],
    tail_16: &[u8; 16],
    target: &[u8; 32],
) -> Result<(), cudarc::driver::DriverError> {
    pipe.stream.memcpy_htod(midstate.as_slice(), &mut pipe.mid_dev)?;
    pipe.stream.memcpy_htod(tail_16.as_slice(), &mut pipe.tail_dev)?;
    pipe.stream.memcpy_htod(target.as_slice(), &mut pipe.target_dev)?;
    // found_flag is zeroed at the top of each launch inside the inner
    // loop anyway, but reset here too so a stale "found from previous
    // hash_range" flag can never leak in if a previous call returned
    // mid-pipeline.
    let zeros = [0u32];
    pipe.stream.memcpy_htod(&zeros, &mut pipe.found_flag)?;
    pipe.in_flight = false;
    Ok(())
}

impl MiningBackend for CudaBackend {
    fn name(&self) -> &'static str {
        "cuda"
    }

    fn hash_range(
        &self,
        header_84: [u8; 84],
        target: [u8; 32],
        nonce_start: u32,
        nonce_end: u32,
        stop: &AtomicBool,
    ) -> Result<Option<MiningResult>, DeviceError> {
        if nonce_end <= nonce_start {
            return Ok(None);
        }

        let midstate = midstate_of_first_chunk(&header_84);
        let mut tail_16 = [0u8; 16];
        tail_16.copy_from_slice(&header_84[64..80]);

        // iter-hotpath #2: borrow the persistent pipes for this call
        // instead of rebuilding them. The mutex is uncontended in
        // practice — each backend is owned by one mining thread.
        //
        // IMP-1b: every device-level failure below returns `Err(DeviceError)`
        // instead of the old bare `None`, so the mining loop can tell a
        // faulted sweep (log + recover + starve the wedge heartbeat + skip
        // hashrate accounting) apart from a clean empty one.
        let mut pipes = match self.pipes.lock() {
            Ok(g) => g,
            Err(e) => {
                return Err(DeviceError(format!("cuda: pipes mutex poisoned: {e}")));
            }
        };

        // Re-prime BOTH pipes with the current header's midstate, tail
        // and target. This is the per-launch hot path that replaces
        // the old setup_pipe() — 6 small H2D memcpys (3 per pipe) and
        // zero allocations.
        if let Err(e) = prime_pipe(&mut pipes.a, &midstate, &tail_16, &target) {
            return Err(DeviceError(format!("cuda: prime pipe A failed: {e}")));
        }
        if let Err(e) = prime_pipe(&mut pipes.b, &midstate, &tail_16, &target) {
            return Err(DeviceError(format!("cuda: prime pipe B failed: {e}")));
        }

        let cfg = LaunchConfig {
            grid_dim: (self.blocks, 1, 1),
            block_dim: (self.threads_per_block, 1, 1),
            shared_mem_bytes: 0,
        };

        let nonces_per_launch = self.nonces_per_launch();
        let mut next_start: u64 = nonce_start as u64;
        let mut current_pipe = 0u8;

        // Destructure once so the borrow checker treats a / b / func as
        // disjoint fields (otherwise selecting `&mut pipes.a` inside
        // the loop locks the whole `pipes` binding and conflicts with
        // `&pipes.func` for `launch_builder`).
        let PipePair { a, b, func } = &mut *pipes;

        loop {
            if stop.load(Ordering::Relaxed) {
                return Ok(None);
            }
            let pipe: &mut PipeRes = if current_pipe == 0 { &mut *a } else { &mut *b };

            // Drain if in flight. A drain-side device error (failed
            // synchronize / readback) surfaces as `Err` — the launch did NOT
            // provably sweep its nonces.
            if pipe.in_flight {
                if let Some(res) = drain_pipe(pipe)? {
                    return Ok(Some(res));
                }
                pipe.in_flight = false;
            }

            if next_start < nonce_end as u64 {
                let launch_size = nonces_per_launch.min(nonce_end as u64 - next_start);
                if launch_size > 0 {
                    let start_u32 = next_start as u32;
                    let zeros = [0u32];
                    if let Err(e) = pipe.stream.memcpy_htod(&zeros, &mut pipe.found_flag) {
                        return Err(DeviceError(format!(
                            "cuda: zero found_flag memcpy failed: {e}"
                        )));
                    }
                    let end_u32 = nonce_end;

                    let mut builder = pipe.stream.launch_builder(func);
                    builder.arg(&pipe.mid_dev);
                    builder.arg(&pipe.tail_dev);
                    builder.arg(&pipe.target_dev);
                    builder.arg(&start_u32);
                    builder.arg(&end_u32);
                    builder.arg(&self.nonces_per_thread);
                    builder.arg(&mut pipe.found_nonce);
                    builder.arg(&mut pipe.found_flag);
                    builder.arg(&mut pipe.found_hash);
                    let launch_result = unsafe { builder.launch(cfg) };
                    if let Err(e) = launch_result {
                        return Err(DeviceError(format!("cuda: kernel launch failed: {e}")));
                    }
                    pipe.in_flight = true;
                    next_start = next_start.saturating_add(launch_size);
                }
            } else if !a.in_flight && !b.in_flight {
                return Ok(None);
            }

            current_pipe ^= 1;
        }
    }
}

impl Recoverable for CudaBackend {
    /// Forward to the inherent [`CudaBackend::recover`] — rebuild module + pipes
    /// in-process on a wedged GPU. See that method for the full strategy.
    fn recover(&self) -> bool {
        CudaBackend::recover(self)
    }
}

/// Wait for `pipe`'s pending launch and read out the flag/nonce/hash.
/// `Ok(None)` = the launch completed cleanly with no find; `Err` = the DEVICE
/// faulted (failed synchronize / readback) — pre-IMP-1b these errors were
/// `.ok()?`-mapped to the same `None` as "no find", silently swallowing e.g.
/// a TDR reset mid-drain.
fn drain_pipe(pipe: &mut PipeRes) -> Result<Option<MiningResult>, DeviceError> {
    pipe.stream
        .synchronize()
        .map_err(|e| DeviceError(format!("cuda: stream synchronize failed: {e}")))?;
    let flag_host: Vec<u32> = pipe
        .stream
        .clone_dtoh(&pipe.found_flag)
        .map_err(|e| DeviceError(format!("cuda: read found_flag failed: {e}")))?;
    if flag_host[0] == 0 {
        return Ok(None);
    }
    let nonce_host: Vec<u32> = pipe
        .stream
        .clone_dtoh(&pipe.found_nonce)
        .map_err(|e| DeviceError(format!("cuda: read found_nonce failed: {e}")))?;
    let hash_host: Vec<u32> = pipe
        .stream
        .clone_dtoh(&pipe.found_hash)
        .map_err(|e| DeviceError(format!("cuda: read found_hash failed: {e}")))?;
    let mut hash = [0u8; 32];
    for i in 0..8 {
        let be = hash_host[i].to_be_bytes();
        hash[4 * i..4 * i + 4].copy_from_slice(&be);
    }
    Ok(Some(MiningResult {
        nonce: nonce_host[0],
        hash,
    }))
}

// ---------------------------------------------------------------------------
// Startup GPU auto-tune + reproducible benchmark.
//
// IMPORTANT: this is *client* engineering only — it never changes PoW/hash
// logic. The benchmark drives the SAME `MiningBackend::hash_range` / SAME
// embedded SHA-256d kernel; it only varies the launch GEOMETRY (how many
// blocks/threads/nonces a launch sweeps) and TIMES the throughput. Every hash
// computed during a bench is bit-for-bit the production hash.
// ---------------------------------------------------------------------------

/// A deterministic, non-trivial 84-byte header for benchmarking. Fixed content
/// ⇒ a `--bench` run is reproducible (the geometry MH/s a card reports is
/// stable, not data-dependent — sha256d cost is independent of the bytes). The
/// version word is set to 1 like a real header; the rest is a fixed ramp.
fn bench_header() -> [u8; 84] {
    let mut h = [0u8; 84];
    h[0..4].copy_from_slice(&1u32.to_le_bytes());
    for (i, b) in h.iter_mut().enumerate().skip(4) {
        *b = (i as u8).wrapping_mul(31).wrapping_add(7);
    }
    h
}

/// Benchmark ONE launch geometry on `device_index` for ~`budget`, returning
/// measured throughput in MH/s.
///
/// Method: build a `CudaBackend` with this geometry, then repeatedly sweep the
/// FULL `[0, u32::MAX)` nonce range with an ALL-ZERO target. An all-zero target
/// is (essentially) never satisfied — `hash <= 0` is false for any non-zero
/// digest — so `hash_range` does NOT stop early on a "found" share; it sweeps
/// every nonce, giving a clean nonces-per-second figure. We accumulate fully-
/// completed sweeps (each = 2^32 nonces) until the wall-clock budget elapses,
/// then divide attempted nonces by elapsed seconds. At least one full sweep is
/// always performed so even a tiny budget yields a real number. Returns `Err`
/// if the backend can't be built for this geometry (caller records that
/// geometry as failed and moves on).
pub fn benchmark_geometry(
    device_index: usize,
    blocks: u32,
    threads_per_block: u32,
    nonces_per_thread: u32,
    budget: Duration,
) -> Result<f64> {
    let backend = CudaBackend::new(device_index, blocks, threads_per_block, nonces_per_thread)?;
    let target_never = [0u8; 32]; // all-zero ⇒ never "found" ⇒ full sweep
    let header = bench_header();
    let stop = AtomicBool::new(false);

    let started = Instant::now();
    let mut nonces_done: u128 = 0;
    // Sweep the whole 32-bit nonce space at least once, then keep going until
    // the time budget is spent. Each completed `hash_range` over [0, MAX)
    // attempts the full 2^32 span (it returns None at the end since nothing is
    // "found").
    loop {
        let _ = backend.hash_range(header, target_never, 0, u32::MAX, &stop);
        nonces_done += u32::MAX as u128;
        if started.elapsed() >= budget {
            break;
        }
    }
    let secs = started.elapsed().as_secs_f64();
    if secs <= 0.0 {
        return Ok(0.0);
    }
    // nonces/sec → MH/s.
    Ok((nonces_done as f64) / secs / 1.0e6)
}

/// Result of an auto-tune sweep: the winning geometry plus every per-geometry
/// measurement (so the caller can log the full table). Returned by
/// [`auto_tune`].
#[derive(Debug, Clone)]
pub struct AutoTuneResult {
    pub device_index: usize,
    pub device_name: String,
    pub best: GeometryMeasurement,
    pub measurements: Vec<GeometryMeasurement>,
    /// Where the winner was persisted (`None` if persistence was skipped/failed
    /// — non-fatal; the geometry is still returned + used this run).
    pub cache_path: Option<std::path::PathBuf>,
}

/// Run the startup auto-tune on `device_index`: benchmark every
/// [`candidate_geometries`] entry for `per_geom` each, pick the fastest via the
/// pure [`pick_best`], PERSIST the winner to the geometry cache (so later starts
/// skip the bench), and return the full result. Logs each geometry's MH/s as it
/// goes.
///
/// Failure handling: a geometry that errors during its bench is recorded with
/// `mh_s = 0.0` (and thus ignored by `pick_best`) rather than aborting the whole
/// sweep — one bad geometry never sinks the tune. If NO geometry produced a
/// usable measurement, returns `Err` and the caller falls back to the default
/// geometry. Persistence failure is logged but NOT fatal.
pub fn auto_tune(device_index: usize, per_geom: Duration) -> Result<AutoTuneResult> {
    // Resolve the device name once (used to scope the cache entry to this card).
    let device_name = CudaContext::new(device_index)
        .and_then(|ctx| ctx.name())
        .unwrap_or_else(|_| "<unknown>".to_string());

    tracing::info!(
        "auto-tune: benchmarking {} candidate geometries on device {device_index} ({device_name}), ~{:?} each",
        candidate_geometries().len(),
        per_geom,
    );

    let mut measurements: Vec<GeometryMeasurement> = Vec::new();
    for (blocks, tpb, npt) in candidate_geometries() {
        let mh = match benchmark_geometry(device_index, blocks, tpb, npt, per_geom) {
            Ok(v) => {
                tracing::info!(
                    "auto-tune: geom {blocks}x{tpb}x{npt} = {:.1} MH/s",
                    v
                );
                v
            }
            Err(e) => {
                tracing::warn!(
                    "auto-tune: geom {blocks}x{tpb}x{npt} failed ({e}); recording 0 MH/s and skipping"
                );
                0.0
            }
        };
        measurements.push(GeometryMeasurement::new(blocks, tpb, npt, mh));
    }

    let best = pick_best(&measurements)
        .ok_or_else(|| anyhow!("auto-tune: no candidate geometry produced a usable measurement"))?;

    tracing::info!(
        "auto-tune: WINNER {}x{}x{} at {:.1} MH/s",
        best.blocks,
        best.threads_per_block,
        best.nonces_per_thread,
        best.mh_s,
    );

    // Persist the winner (best-effort). The geometry is already chosen + will be
    // used this run regardless; the cache only saves the bench on the NEXT start.
    let cached = CachedGeometry {
        device_index,
        device_name: device_name.clone(),
        blocks: best.blocks,
        threads_per_block: best.threads_per_block,
        nonces_per_thread: best.nonces_per_thread,
        mh_s: best.mh_s,
    };
    let cache_path = match autotune::save_cached(&cached) {
        Ok(p) => {
            tracing::info!("auto-tune: persisted winning geometry to {}", p.display());
            Some(p)
        }
        Err(e) => {
            tracing::warn!("auto-tune: could not persist geometry cache ({e}); will re-bench next start");
            None
        }
    };

    Ok(AutoTuneResult {
        device_index,
        device_name,
        best,
        measurements,
        cache_path,
    })
}
