//! Mining backend trait.
//!
//! A backend takes:
//!   - the 84-byte header skeleton (the merkle slot is already filled),
//!   - a 32-byte big-endian PoW target,
//!   - a half-open nonce range `[start, end)`,
//! and returns `Ok(Some(..))` with the first nonce whose
//! `sha256d(header_with_nonce) <= target`, `Ok(None)` for a CLEAN sweep that
//! found nothing (range exhausted, or cancelled via `stop`), or
//! `Err(DeviceError)` when the DEVICE faulted mid-sweep (driver error, failed
//! launch/memcpy, dead context) — the sweep did NOT cover its range.
//!
//! The `Ok(None)` / `Err` distinction is load-bearing (IMP-1b): a fast-failing
//! GPU used to return the same `None` as a clean empty sweep, so the mining
//! loop kept feeding the wedge-watchdog heartbeat and published phantom
//! hashrate while mining nothing. Callers treat `Err` as "no work was done":
//! log it, attempt recovery, and do NOT count the range as hashed.
//!
//! Implementations:
//!   - `backends::cpu::CpuBackend` — uses sha2 with rayon-style threading
//!     (never returns `Err`; it has no device to fault).
//!   - `backends::opencl::OpenclBackend` (feature = "opencl").
//!   - `backends::cuda::CudaBackend` (feature = "cuda").

#[derive(Debug, Clone, Copy)]
pub struct MiningResult {
    pub nonce: u32,
    pub hash: [u8; 32],
}

/// A device-level failure during a sweep — DISTINGUISHABLE from a clean
/// "swept the range / cancelled, found nothing" (`Ok(None)`). Carries the
/// driver/runtime error text so the caller's WARN log names the real fault.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DeviceError(pub String);

impl std::fmt::Display for DeviceError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for DeviceError {}

pub trait MiningBackend {
    fn name(&self) -> &'static str;

    fn hash_range(
        &self,
        header_84: [u8; 84],
        target: [u8; 32],
        nonce_start: u32,
        nonce_end: u32,
        stop: &std::sync::atomic::AtomicBool,
    ) -> Result<Option<MiningResult>, DeviceError>;
}
