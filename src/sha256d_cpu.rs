//! Reference CPU sha256d, plus a helper that precomputes the SHA-256
//! midstate over the first 64-byte chunk of an 84-byte block header.
//!
//! Used by:
//!   - the CPU backend directly,
//!   - GPU backends to upload the midstate to the device once per template
//!     (avoiding re-hashing the first 64 bytes inside every kernel thread).
//!
//! The CPU backend uses `finish_sha256d_from_midstate_fast` which goes
//! through `sha2::compress256` — on every modern x86 CPU this dispatches
//! to the SHA-NI hardware instructions via the `cpufeatures` crate.
//! `finish_sha256d_from_midstate` (hand-coded) is kept as a verified
//! reference and to mirror the GPU kernel structure exactly.

use sha2::digest::generic_array::GenericArray;
use sha2::digest::typenum::U64;
use sha2::{compress256, Digest, Sha256};

/// Plain reference. Used by the CPU backend and for cross-checking GPU
/// results during development.
#[inline]
pub fn sha256d(buf: &[u8]) -> [u8; 32] {
    let h1 = Sha256::digest(buf);
    let h2 = Sha256::digest(h1);
    let mut out = [0u8; 32];
    out.copy_from_slice(&h2);
    out
}

/// 8-word SHA-256 IV.
pub const SHA256_IV: [u32; 8] = [
    0x6a09e667, 0xbb67ae85, 0x3c6ef372, 0xa54ff53a, 0x510e527f, 0x9b05688c, 0x1f83d9ab, 0x5be0cd19,
];

/// SHA-256 round constants.
pub const SHA256_K: [u32; 64] = [
    0x428a2f98, 0x71374491, 0xb5c0fbcf, 0xe9b5dba5, 0x3956c25b, 0x59f111f1, 0x923f82a4, 0xab1c5ed5,
    0xd807aa98, 0x12835b01, 0x243185be, 0x550c7dc3, 0x72be5d74, 0x80deb1fe, 0x9bdc06a7, 0xc19bf174,
    0xe49b69c1, 0xefbe4786, 0x0fc19dc6, 0x240ca1cc, 0x2de92c6f, 0x4a7484aa, 0x5cb0a9dc, 0x76f988da,
    0x983e5152, 0xa831c66d, 0xb00327c8, 0xbf597fc7, 0xc6e00bf3, 0xd5a79147, 0x06ca6351, 0x14292967,
    0x27b70a85, 0x2e1b2138, 0x4d2c6dfc, 0x53380d13, 0x650a7354, 0x766a0abb, 0x81c2c92e, 0x92722c85,
    0xa2bfe8a1, 0xa81a664b, 0xc24b8b70, 0xc76c51a3, 0xd192e819, 0xd6990624, 0xf40e3585, 0x106aa070,
    0x19a4c116, 0x1e376c08, 0x2748774c, 0x34b0bcb5, 0x391c0cb3, 0x4ed8aa4a, 0x5b9cca4f, 0x682e6ff3,
    0x748f82ee, 0x78a5636f, 0x84c87814, 0x8cc70208, 0x90befffa, 0xa4506ceb, 0xbef9a3f7, 0xc67178f2,
];

/// One SHA-256 compression function call. `state` is updated in place over
/// a single 64-byte `block`.
pub fn sha256_compress(state: &mut [u32; 8], block: &[u8; 64]) {
    let mut w = [0u32; 64];
    for i in 0..16 {
        w[i] = u32::from_be_bytes([
            block[4 * i],
            block[4 * i + 1],
            block[4 * i + 2],
            block[4 * i + 3],
        ]);
    }
    for i in 16..64 {
        let s0 = w[i - 15].rotate_right(7) ^ w[i - 15].rotate_right(18) ^ (w[i - 15] >> 3);
        let s1 = w[i - 2].rotate_right(17) ^ w[i - 2].rotate_right(19) ^ (w[i - 2] >> 10);
        w[i] = w[i - 16]
            .wrapping_add(s0)
            .wrapping_add(w[i - 7])
            .wrapping_add(s1);
    }
    let mut a = state[0];
    let mut b = state[1];
    let mut c = state[2];
    let mut d = state[3];
    let mut e = state[4];
    let mut f = state[5];
    let mut g = state[6];
    let mut h = state[7];
    for i in 0..64 {
        let s1 = e.rotate_right(6) ^ e.rotate_right(11) ^ e.rotate_right(25);
        let ch = (e & f) ^ (!e & g);
        let t1 = h
            .wrapping_add(s1)
            .wrapping_add(ch)
            .wrapping_add(SHA256_K[i])
            .wrapping_add(w[i]);
        let s0 = a.rotate_right(2) ^ a.rotate_right(13) ^ a.rotate_right(22);
        let maj = (a & b) ^ (a & c) ^ (b & c);
        let t2 = s0.wrapping_add(maj);
        h = g;
        g = f;
        f = e;
        e = d.wrapping_add(t1);
        d = c;
        c = b;
        b = a;
        a = t1.wrapping_add(t2);
    }
    state[0] = state[0].wrapping_add(a);
    state[1] = state[1].wrapping_add(b);
    state[2] = state[2].wrapping_add(c);
    state[3] = state[3].wrapping_add(d);
    state[4] = state[4].wrapping_add(e);
    state[5] = state[5].wrapping_add(f);
    state[6] = state[6].wrapping_add(g);
    state[7] = state[7].wrapping_add(h);
}

/// Compute the SHA-256 midstate after consuming the FIRST 64-byte chunk
/// of an 84-byte block header. The remaining 20 bytes (merkle_tail(4) | time(8) |
/// bits(4) | nonce(4)) are fed to the GPU kernel along with this midstate; the
/// kernel finishes the inner hash, then runs the outer SHA-256 over the 32-byte
/// digest.
pub fn midstate_of_first_chunk(header_84: &[u8; 84]) -> [u32; 8] {
    let mut state = SHA256_IV;
    let mut chunk = [0u8; 64];
    chunk.copy_from_slice(&header_84[..64]);
    sha256_compress(&mut state, &chunk);
    state
}

/// CPU finisher: given the midstate of the first 64 bytes of an 84-byte
/// header and the final 20 bytes (`tail`), compute sha256d. Used by the CPU
/// backend so its loop matches the GPU's exactly.
pub fn finish_sha256d_from_midstate(midstate: &[u32; 8], tail: &[u8; 20]) -> [u8; 32] {
    // Second SHA-256 chunk = 20 message bytes + 1 (0x80) + zero padding +
    // 8 bytes length (in bits). Total message length = 84 bytes = 672 bits.
    let mut block = [0u8; 64];
    block[..20].copy_from_slice(tail);
    block[20] = 0x80;
    // Length in bits = 672 = 0x2A0; place as big-endian u64 in last 8 bytes.
    let bitlen: u64 = 84 * 8;
    let len_be = bitlen.to_be_bytes();
    block[56..64].copy_from_slice(&len_be);

    let mut state = *midstate;
    sha256_compress(&mut state, &block);

    // Now hash that 32-byte digest with a fresh SHA-256.
    let mut inner_digest = [0u8; 32];
    for i in 0..8 {
        inner_digest[4 * i..4 * i + 4].copy_from_slice(&state[i].to_be_bytes());
    }
    let mut outer = SHA256_IV;
    let mut outer_block = [0u8; 64];
    outer_block[..32].copy_from_slice(&inner_digest);
    outer_block[32] = 0x80;
    let outer_bitlen: u64 = 32 * 8;
    outer_block[56..64].copy_from_slice(&outer_bitlen.to_be_bytes());
    sha256_compress(&mut outer, &outer_block);

    let mut out = [0u8; 32];
    for i in 0..8 {
        out[4 * i..4 * i + 4].copy_from_slice(&outer[i].to_be_bytes());
    }
    out
}

/// SHA-NI-accelerated CPU finisher. Same contract as
/// `finish_sha256d_from_midstate` but uses `sha2::compress256` which the
/// cpufeatures crate dispatches to SHA-NI on modern x86_64. Output is
/// byte-identical.
#[inline]
pub fn finish_sha256d_from_midstate_fast(midstate: &[u32; 8], tail: &[u8; 20]) -> [u8; 32] {
    // Second SHA-256 chunk of the inner hash: 20 message bytes + 0x80 +
    // zero padding + 64-bit BE bitlen (84*8 = 672).
    let mut block_buf = [0u8; 64];
    block_buf[..20].copy_from_slice(tail);
    block_buf[20] = 0x80;
    let bitlen: u64 = 84 * 8;
    block_buf[56..64].copy_from_slice(&bitlen.to_be_bytes());

    let mut state = *midstate;
    let block = GenericArray::<u8, U64>::clone_from_slice(&block_buf);
    compress256(&mut state, core::slice::from_ref(&block));

    // Outer hash over the 32-byte inner digest.
    let mut outer_block = [0u8; 64];
    for i in 0..8 {
        outer_block[4 * i..4 * i + 4].copy_from_slice(&state[i].to_be_bytes());
    }
    outer_block[32] = 0x80;
    let outer_bitlen: u64 = 32 * 8;
    outer_block[56..64].copy_from_slice(&outer_bitlen.to_be_bytes());

    let mut outer_state = SHA256_IV;
    let outer = GenericArray::<u8, U64>::clone_from_slice(&outer_block);
    compress256(&mut outer_state, core::slice::from_ref(&outer));

    let mut out = [0u8; 32];
    for i in 0..8 {
        out[4 * i..4 * i + 4].copy_from_slice(&outer_state[i].to_be_bytes());
    }
    out
}

/// Same dispatch path as the per-nonce finisher but for the initial
/// midstate. The kernel uploads this midstate verbatim, so it must match
/// the hand-coded version byte-for-byte; tests below assert it.
#[inline]
pub fn midstate_of_first_chunk_fast(header_84: &[u8; 84]) -> [u32; 8] {
    let mut state = SHA256_IV;
    let block = GenericArray::<u8, U64>::clone_from_slice(&header_84[..64]);
    compress256(&mut state, core::slice::from_ref(&block));
    state
}

/// Precompute the constant-per-job prefix of the INNER second-block SHA-256
/// compression, so the GPU kernel can resume it from round 4 on every nonce
/// instead of re-running the 4 constant rounds + 3 constant schedule
/// expansions each time.
///
/// The inner second block of an 84-byte-header sha256d is
///   w[0..=3] = merkle_tail | time_lo | time_hi | bits   (constant per job)
///   w[4]     = BSWAP32(nonce)                            (VARIES per nonce)
///   w[5]     = 0x80000000, w[6..=14] = 0, w[15] = 672    (constant padding)
///
/// Round `i` of the compression consumes schedule word `w[i]`, so rounds
/// 0..=3 depend ONLY on the constant words w[0..=3] — their result (the
/// working vars a..h after round 3) is identical for every nonce. Likewise the
/// schedule words w16, w17, w18 depend only on constant inputs (w4 first
/// appears in w19 = w3 + s0(w4) + w12 + s1(w17)); they are the last
/// nonce-independent schedule words.
///
/// Returns `(prefix_state, sched3)` where
///   `prefix_state` = [a,b,c,d,e,f,g,h] after rounds 0..=3 from `midstate`, and
///   `sched3`       = [w16, w17, w18].
///
/// IMPORTANT: `prefix_state` is the WORKING state after round 3, NOT a new
/// midstate — the final `state[i] += a..h` fold at the end of the compression
/// still adds the ORIGINAL `midstate`, so the kernel keeps uploading `midstate`
/// too. This is a pure schedule/round-prefix precompute; the emitted digest is
/// bit-for-bit identical to running the full `sha256_compress`.
///
/// `tail_16` = bytes 64..80 of the 84-byte header (merkle_tail|time|bits). The
/// nonce (bytes 80..84) is deliberately NOT an input — it enters at round 4.
pub fn inner_prefix_precompute(midstate: &[u32; 8], tail_16: &[u8; 16]) -> ([u32; 8], [u32; 3]) {
    #[inline]
    fn small_sigma0(x: u32) -> u32 {
        x.rotate_right(7) ^ x.rotate_right(18) ^ (x >> 3)
    }
    #[inline]
    fn small_sigma1(x: u32) -> u32 {
        x.rotate_right(17) ^ x.rotate_right(19) ^ (x >> 10)
    }

    // Constant message words (BE-packed exactly as the kernel packs tail_16 in
    // mine_sha256d, sha256d.cu:210-217).
    let w0 = u32::from_be_bytes([tail_16[0], tail_16[1], tail_16[2], tail_16[3]]);
    let w1 = u32::from_be_bytes([tail_16[4], tail_16[5], tail_16[6], tail_16[7]]);
    let w2 = u32::from_be_bytes([tail_16[8], tail_16[9], tail_16[10], tail_16[11]]);
    let w3 = u32::from_be_bytes([tail_16[12], tail_16[13], tail_16[14], tail_16[15]]);
    // Constant padding words used below: w9 = w10 = w11 = w14 = 0, w15 = 672.
    let w15: u32 = 672;

    // Constant schedule words. General recurrence w[i] = w[i-16] + s0(w[i-15])
    // + w[i-7] + s1(w[i-2]); the zero terms are kept explicit so this mirrors
    // the kernel's expansion one-to-one.
    //   w16 = w0 + s0(w1) + w9(0)  + s1(w14=0)
    //   w17 = w1 + s0(w2) + w10(0) + s1(w15)
    //   w18 = w2 + s0(w3) + w11(0) + s1(w16)
    let w16 = w0
        .wrapping_add(small_sigma0(w1))
        .wrapping_add(small_sigma1(0));
    let w17 = w1
        .wrapping_add(small_sigma0(w2))
        .wrapping_add(small_sigma1(w15));
    let w18 = w2
        .wrapping_add(small_sigma0(w3))
        .wrapping_add(small_sigma1(w16));

    // Rounds 0..=3 from the midstate consume only w[0..=3].
    let wc = [w0, w1, w2, w3];
    let mut a = midstate[0];
    let mut b = midstate[1];
    let mut c = midstate[2];
    let mut d = midstate[3];
    let mut e = midstate[4];
    let mut f = midstate[5];
    let mut g = midstate[6];
    let mut h = midstate[7];
    for i in 0..4 {
        let s1 = e.rotate_right(6) ^ e.rotate_right(11) ^ e.rotate_right(25);
        let ch = (e & f) ^ (!e & g);
        let t1 = h
            .wrapping_add(s1)
            .wrapping_add(ch)
            .wrapping_add(SHA256_K[i])
            .wrapping_add(wc[i]);
        let s0 = a.rotate_right(2) ^ a.rotate_right(13) ^ a.rotate_right(22);
        let maj = (a & b) ^ (a & c) ^ (b & c);
        let t2 = s0.wrapping_add(maj);
        h = g;
        g = f;
        f = e;
        e = d.wrapping_add(t1);
        d = c;
        c = b;
        b = a;
        a = t1.wrapping_add(t2);
    }

    ([a, b, c, d, e, f, g, h], [w16, w17, w18])
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The GPU kernel's specialized inner compress: init the working vars from
    /// `prefix_state`, splice the constant schedule words `sched3` into
    /// w16..w18, expand w19..w63 with the standard recurrence, run ONLY rounds
    /// 4..=63, then fold in the ORIGINAL `midstate`. This is the exact CPU
    /// mirror of the specialized path added to sha256d.cu::try_one_nonce, used
    /// as the bit-exact oracle in `inner_prefix_matches_full_compress`.
    fn inner_compress_via_prefix(midstate: &[u32; 8], tail_16: &[u8; 16], nonce: u32) -> [u32; 8] {
        let (prefix, sched3) = inner_prefix_precompute(midstate, tail_16);

        let mut w = [0u32; 64];
        w[0] = u32::from_be_bytes([tail_16[0], tail_16[1], tail_16[2], tail_16[3]]);
        w[1] = u32::from_be_bytes([tail_16[4], tail_16[5], tail_16[6], tail_16[7]]);
        w[2] = u32::from_be_bytes([tail_16[8], tail_16[9], tail_16[10], tail_16[11]]);
        w[3] = u32::from_be_bytes([tail_16[12], tail_16[13], tail_16[14], tail_16[15]]);
        w[4] = nonce.swap_bytes(); // BSWAP32(nonce)
        w[5] = 0x8000_0000;
        w[15] = 672;
        w[16] = sched3[0];
        w[17] = sched3[1];
        w[18] = sched3[2];
        for i in 19..64 {
            let s0 = w[i - 15].rotate_right(7) ^ w[i - 15].rotate_right(18) ^ (w[i - 15] >> 3);
            let s1 = w[i - 2].rotate_right(17) ^ w[i - 2].rotate_right(19) ^ (w[i - 2] >> 10);
            w[i] = w[i - 16]
                .wrapping_add(s0)
                .wrapping_add(w[i - 7])
                .wrapping_add(s1);
        }

        let mut a = prefix[0];
        let mut b = prefix[1];
        let mut c = prefix[2];
        let mut d = prefix[3];
        let mut e = prefix[4];
        let mut f = prefix[5];
        let mut g = prefix[6];
        let mut h = prefix[7];
        for i in 4..64 {
            let s1 = e.rotate_right(6) ^ e.rotate_right(11) ^ e.rotate_right(25);
            let ch = (e & f) ^ (!e & g);
            let t1 = h
                .wrapping_add(s1)
                .wrapping_add(ch)
                .wrapping_add(SHA256_K[i])
                .wrapping_add(w[i]);
            let s0 = a.rotate_right(2) ^ a.rotate_right(13) ^ a.rotate_right(22);
            let maj = (a & b) ^ (a & c) ^ (b & c);
            let t2 = s0.wrapping_add(maj);
            h = g;
            g = f;
            f = e;
            e = d.wrapping_add(t1);
            d = c;
            c = b;
            b = a;
            a = t1.wrapping_add(t2);
        }

        [
            midstate[0].wrapping_add(a),
            midstate[1].wrapping_add(b),
            midstate[2].wrapping_add(c),
            midstate[3].wrapping_add(d),
            midstate[4].wrapping_add(e),
            midstate[5].wrapping_add(f),
            midstate[6].wrapping_add(g),
            midstate[7].wrapping_add(h),
        ]
    }

    /// Build the full 64-byte inner second block exactly as the kernel does and
    /// run the UNMODIFIED `sha256_compress` — the ground-truth reference.
    fn inner_compress_full(midstate: &[u32; 8], tail_16: &[u8; 16], nonce: u32) -> [u32; 8] {
        let mut block = [0u8; 64];
        block[0..16].copy_from_slice(tail_16);
        // block[16..20] BE-packs to w[4]=BSWAP32(nonce); BSWAP32(n).to_be_bytes()
        // == n.to_le_bytes(), i.e. the header's LE nonce bytes (bytes 80..84).
        block[16..20].copy_from_slice(&nonce.to_le_bytes());
        block[20] = 0x80;
        block[56..64].copy_from_slice(&(84u64 * 8).to_be_bytes());
        let mut state = *midstate;
        sha256_compress(&mut state, &block);
        state
    }

    #[test]
    fn inner_prefix_matches_full_compress() {
        // Fail-closed bit-exact gate: resuming the inner compress from the
        // precomputed prefix+sched3 (round 4 onward, folding the original
        // midstate) must equal the full compress for every random input.
        let mut rng = 0x1234_5678_9abc_def0u64;
        let mut next = || {
            // xorshift64* — deterministic, no dep needed.
            rng ^= rng >> 12;
            rng ^= rng << 25;
            rng ^= rng >> 27;
            rng.wrapping_mul(0x2545_F491_4F6C_DD1D)
        };
        for _ in 0..2000 {
            let mut midstate = [0u32; 8];
            for m in midstate.iter_mut() {
                *m = next() as u32;
            }
            let mut tail_16 = [0u8; 16];
            for t in tail_16.iter_mut() {
                *t = next() as u8;
            }
            let nonce = next() as u32;

            let via_prefix = inner_compress_via_prefix(&midstate, &tail_16, nonce);
            let full = inner_compress_full(&midstate, &tail_16, nonce);
            assert_eq!(
                via_prefix, full,
                "inner-prefix resume diverged: midstate={midstate:08x?} tail={tail_16:02x?} nonce={nonce:08x}"
            );
        }
    }

    #[test]
    fn midstate_finish_matches_reference() {
        let header = [0xCDu8; 84];
        let reference = sha256d(&header);

        let mid = midstate_of_first_chunk(&header);
        let mut tail = [0u8; 20];
        tail.copy_from_slice(&header[64..]);
        let viamid = finish_sha256d_from_midstate(&mid, &tail);

        assert_eq!(reference, viamid);
    }

    #[test]
    fn empty_sha256d_matches_known_vector() {
        // Sanity check: sha256d("") for cross-version stability.
        let h = sha256d(b"");
        assert_eq!(
            hex::encode(h),
            "5df6e0e2761359d30a8275058e299fcc0381534545f55cf43e41983f5d4c9456"
        );
    }

    #[test]
    fn sha2_fast_path_matches_hand_coded() {
        // Identical output across the two CPU implementations + the
        // canonical sha256d on the full 84-byte buffer.
        for seed in [0u8, 7, 0x55, 0xFF] {
            let header = [seed; 84];
            let mid_hand = midstate_of_first_chunk(&header);
            let mid_sha2 = midstate_of_first_chunk_fast(&header);
            assert_eq!(mid_hand, mid_sha2, "midstate mismatch seed={}", seed);

            let mut tail = [0u8; 20];
            tail.copy_from_slice(&header[64..]);

            let hand = finish_sha256d_from_midstate(&mid_hand, &tail);
            let fast = finish_sha256d_from_midstate_fast(&mid_sha2, &tail);
            let canonical = sha256d(&header);
            assert_eq!(hand, fast, "finisher mismatch seed={}", seed);
            assert_eq!(fast, canonical, "fast finisher vs canonical seed={}", seed);
        }
    }
}
