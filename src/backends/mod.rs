//! Backend implementations.

pub mod cpu;

#[cfg(feature = "opencl")]
pub mod opencl;

#[cfg(feature = "cuda")]
pub mod cuda;
