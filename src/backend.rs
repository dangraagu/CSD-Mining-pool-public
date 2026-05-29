//! Mining backend trait.
//!
//! A backend takes:
//!   - the 84-byte header skeleton (the merkle slot is already filled),
//!   - a 32-byte big-endian PoW target,
//!   - a half-open nonce range `[start, end)`,
//! and returns the first nonce whose `sha256d(header_with_nonce) <= target`,
//! or `None` if no nonce in the range solved.
//!
//! Implementations:
//!   - `backends::cpu::CpuBackend` — uses sha2 with rayon-style threading.
//!   - `backends::opencl::OpenclBackend` (feature = "opencl").
//!   - `backends::cuda::CudaBackend` (feature = "cuda").

#[derive(Debug, Clone, Copy)]
pub struct MiningResult {
    pub nonce: u32,
    pub hash: [u8; 32],
}

pub trait MiningBackend {
    fn name(&self) -> &'static str;

    fn hash_range(
        &self,
        header_84: [u8; 84],
        target: [u8; 32],
        nonce_start: u32,
        nonce_end: u32,
        stop: &std::sync::atomic::AtomicBool,
    ) -> Option<MiningResult>;
}
