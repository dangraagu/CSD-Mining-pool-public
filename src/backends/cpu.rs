//! CPU mining backend.
//!
//! Uses the precomputed 64-byte midstate so the inner loop only runs one
//! SHA-256 compression over the 20-byte tail (merkle_tail | time | bits | nonce)
//! followed by the outer SHA-256 over the 32-byte digest. This matches the GPU
//! kernel exactly and is also the only backend that runs out-of-the-box
//! on every platform — perfect for end-to-end smoke testing.

use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::thread;

use crate::backend::{MiningBackend, MiningResult};
use crate::sha256d_cpu::{finish_sha256d_from_midstate_fast, midstate_of_first_chunk_fast};

pub struct CpuBackend {
    pub threads: usize,
}

impl CpuBackend {
    pub fn new(threads: usize) -> Self {
        Self {
            threads: threads.max(1),
        }
    }
}

#[inline]
fn hash_leq_target(hash: &[u8; 32], target: &[u8; 32]) -> bool {
    // Lexicographic big-endian compare. hash <= target.
    for i in 0..32 {
        if hash[i] < target[i] {
            return true;
        }
        if hash[i] > target[i] {
            return false;
        }
    }
    true
}

impl MiningBackend for CpuBackend {
    fn name(&self) -> &'static str {
        "cpu"
    }

    fn hash_range(
        &self,
        header_84: [u8; 84],
        target: [u8; 32],
        nonce_start: u32,
        nonce_end: u32,
        stop: &AtomicBool,
    ) -> Option<MiningResult> {
        if nonce_end <= nonce_start {
            return None;
        }
        let midstate = midstate_of_first_chunk_fast(&header_84);

        // The 16 fixed bytes of the tail (merkle_tail|time|bits) come from the
        // header and don't change inside the loop; nonce is appended per attempt.
        let mut tail_template = [0u8; 20];
        tail_template[..16].copy_from_slice(&header_84[64..80]);

        let next_nonce = AtomicU32::new(nonce_start);
        let found = std::sync::Arc::new(std::sync::Mutex::new(None::<MiningResult>));
        let local_stop = AtomicBool::new(false);

        thread::scope(|scope| {
            for _ in 0..self.threads {
                let midstate = midstate;
                let tail_template = tail_template;
                let target = target;
                let next_nonce = &next_nonce;
                let found = found.clone();
                let local_stop = &local_stop;

                scope.spawn(move || {
                    let mut tail = tail_template;
                    loop {
                        if stop.load(Ordering::Relaxed) || local_stop.load(Ordering::Relaxed) {
                            return;
                        }
                        // Grab a small chunk of nonces so threads don't
                        // hammer the atomic.
                        const CHUNK: u32 = 4096;
                        let start = next_nonce.fetch_add(CHUNK, Ordering::Relaxed);
                        if start >= nonce_end {
                            return;
                        }
                        let end = start.saturating_add(CHUNK).min(nonce_end);
                        for n in start..end {
                            tail[16..20].copy_from_slice(&n.to_le_bytes());
                            let h = finish_sha256d_from_midstate_fast(&midstate, &tail);
                            if hash_leq_target(&h, &target) {
                                let mut g = found.lock().unwrap();
                                if g.is_none() {
                                    *g = Some(MiningResult { nonce: n, hash: h });
                                }
                                local_stop.store(true, Ordering::Relaxed);
                                return;
                            }
                        }
                    }
                });
            }
        });

        let g = found.lock().unwrap();
        *g
    }
}
