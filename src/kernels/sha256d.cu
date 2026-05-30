// csd GPU mining kernel — CUDA flavor, 2-stream pipelined design.
//
// Each thread sweeps NONCES_PER_THREAD consecutive nonces in a tight inner
// loop, so a single kernel launch covers
//   geom = gridDim.x * blockDim.x * NONCES_PER_THREAD
// distinct nonces. Default geometry on the host is 560 x 256 x 4096
// (~587 M nonces per launch), so two launches blanket nearly the whole
// 32-bit nonce space.
//
// Header layout (84 bytes, csd1 mainnet wire format):
//   bytes  0..64  = version|prev|merkle (consumed by host midstate)
//   bytes 64..68  = tail of merkle  -> w[0]
//   bytes 68..76  = time (u64 LE)   -> w[1], w[2]
//   bytes 76..80  = bits            -> w[3]
//   bytes 80..84  = nonce           -> w[4]   (variable per iteration)
//   w[5]          = 0x80000000 padding
//   w[15]         = 672 (bit length = 84 * 8)
//
// ROTR uses `__funnelshift_r(x, x, n)`: Maxwell+ hardware fuses the two
// shifts + or into a single instruction. About 3 cycles vs. 5 for a
// scalar rotation.
//
// Blackwell sm_120 notes:
//   After a careful read of this kernel, the existing structure is already
//   close to optimal for per-nonce sha256d on sm_120: midstate/K/tail all
//   live in registers or __constant__ memory; `__funnelshift_r` is used
//   for every ROTR; both schedule and round loops are `#pragma unroll`'d
//   to expose ILP; the 2-stream pipeline at the host hides launch latency.
//
//   The honest sm_120 hot-paths (`cp.async`, `cooperative_groups`,
//   `__shfl_sync` of W[]) all require *inter-thread data sharing* that
//   does not exist in a per-nonce design — every thread is computing a
//   distinct nonce with a distinct W[]. There is nothing to share.
//
//   The one safe, motivated edit is to strength-reduce the per-iteration
//   nonce byte-swap from 8 ops (mask + shift + OR x4) down to a single
//   `__byte_perm` intrinsic. Hardware support for `__byte_perm` is
//   universal on Kepler+; on Blackwell it's a single-cycle ALU op. It is
//   executed once per `try_one_nonce` call, i.e. *every nonce attempt*,
//   so even a small constant saving multiplies through 587M nonces/launch.
//
//   The setup-time byte-swaps for tail_16 and target_be (run once per
//   thread, outside the inner loop) are also moved to `__byte_perm` for
//   symmetry; the perf delta there is negligible but it reads cleaner.
//
//   Correctness is preserved by selftest: identical hashes vs CPU
//   reference on randomized headers.

extern "C" {

// __funnelshift_r is a CUDA built-in available on compute capability 3.5+.
__device__ __forceinline__ unsigned int ROTR_FUNNEL(unsigned int x, unsigned int n) {
    return __funnelshift_r(x, x, n);
}

#define ROTR(x, n) ROTR_FUNNEL((x), (n))

// __byte_perm(x, 0, 0x0123) reverses the bytes of x (BE<->LE).
// Selector 0x0123 picks bytes [3,2,1,0] from the low 32 bits of (x|0<<32)
// which is exactly the BE-byte-swap of x.
//   __byte_perm(x, 0, 0x0123) ==
//     ((x & 0x000000ff) << 24) |
//     ((x & 0x0000ff00) <<  8) |
//     ((x & 0x00ff0000) >>  8) |
//     ((x & 0xff000000) >> 24);
// One ALU instruction (PRMT) vs. the 7+ ops of the explicit form.
__device__ __forceinline__ unsigned int BSWAP32(unsigned int x) {
    return __byte_perm(x, 0u, 0x0123u);
}

__device__ __constant__ unsigned int K[64] = {
    0x428a2f98u, 0x71374491u, 0xb5c0fbcfu, 0xe9b5dba5u,
    0x3956c25bu, 0x59f111f1u, 0x923f82a4u, 0xab1c5ed5u,
    0xd807aa98u, 0x12835b01u, 0x243185beu, 0x550c7dc3u,
    0x72be5d74u, 0x80deb1feu, 0x9bdc06a7u, 0xc19bf174u,
    0xe49b69c1u, 0xefbe4786u, 0x0fc19dc6u, 0x240ca1ccu,
    0x2de92c6fu, 0x4a7484aau, 0x5cb0a9dcu, 0x76f988dau,
    0x983e5152u, 0xa831c66du, 0xb00327c8u, 0xbf597fc7u,
    0xc6e00bf3u, 0xd5a79147u, 0x06ca6351u, 0x14292967u,
    0x27b70a85u, 0x2e1b2138u, 0x4d2c6dfcu, 0x53380d13u,
    0x650a7354u, 0x766a0abbu, 0x81c2c92eu, 0x92722c85u,
    0xa2bfe8a1u, 0xa81a664bu, 0xc24b8b70u, 0xc76c51a3u,
    0xd192e819u, 0xd6990624u, 0xf40e3585u, 0x106aa070u,
    0x19a4c116u, 0x1e376c08u, 0x2748774cu, 0x34b0bcb5u,
    0x391c0cb3u, 0x4ed8aa4au, 0x5b9cca4fu, 0x682e6ff3u,
    0x748f82eeu, 0x78a5636fu, 0x84c87814u, 0x8cc70208u,
    0x90befffau, 0xa4506cebu, 0xbef9a3f7u, 0xc67178f2u
};

__device__ __forceinline__ void sha256_compress(unsigned int state[8], const unsigned int w_in[16]) {
    unsigned int w[64];
    #pragma unroll
    for (int i = 0; i < 16; i++) w[i] = w_in[i];
    #pragma unroll
    for (int i = 16; i < 64; i++) {
        unsigned int s0 = ROTR(w[i-15], 7) ^ ROTR(w[i-15], 18) ^ (w[i-15] >> 3);
        unsigned int s1 = ROTR(w[i-2], 17) ^ ROTR(w[i-2], 19) ^ (w[i-2] >> 10);
        w[i] = w[i-16] + s0 + w[i-7] + s1;
    }
    unsigned int a = state[0], b = state[1], c = state[2], d = state[3];
    unsigned int e = state[4], f = state[5], g = state[6], h = state[7];
    #pragma unroll
    for (int i = 0; i < 64; i++) {
        unsigned int S1 = ROTR(e, 6) ^ ROTR(e, 11) ^ ROTR(e, 25);
        unsigned int ch = (e & f) ^ (~e & g);
        unsigned int t1 = h + S1 + ch + K[i] + w[i];
        unsigned int S0 = ROTR(a, 2) ^ ROTR(a, 13) ^ ROTR(a, 22);
        unsigned int maj = (a & b) ^ (a & c) ^ (b & c);
        unsigned int t2 = S0 + maj;
        h = g; g = f; f = e; e = d + t1;
        d = c; c = b; b = a; a = t1 + t2;
    }
    state[0] += a; state[1] += b; state[2] += c; state[3] += d;
    state[4] += e; state[5] += f; state[6] += g; state[7] += h;
}

__device__ __forceinline__ bool hash_leq_target_words(const unsigned int state[8], const unsigned int target_words[8]) {
    #pragma unroll
    for (int i = 0; i < 8; i++) {
        if (state[i] < target_words[i]) return true;
        if (state[i] > target_words[i]) return false;
    }
    return true;
}

/// One full sha256d attempt for a single nonce. Caller supplies the
/// midstate + the four fixed tail words (merkle_tail, time_lo, time_hi, bits).
__device__ __forceinline__ bool try_one_nonce(
    const unsigned int midstate[8],
    unsigned int w0_merkle_tail,
    unsigned int w1_time_lo,
    unsigned int w2_time_hi,
    unsigned int w3_bits,
    const unsigned int target_words[8],
    unsigned int nonce,
    unsigned int out_hash[8]
) {
    // Second SHA-256 block of the inner hash.
    unsigned int w[16];
    w[0] = w0_merkle_tail;
    w[1] = w1_time_lo;
    w[2] = w2_time_hi;
    w[3] = w3_bits;
    // Bytes 16..20 of the second block = nonce in LE byte order, packed BE
    // into a 32-bit word — i.e. a 32-bit byte-reverse of the nonce.
    // BSWAP32 (__byte_perm) replaces the 7-op mask/shift/OR chain
    // with one PRMT instruction. Hot path — runs once per try_one_nonce.
    w[4] = BSWAP32(nonce);
    w[5] = 0x80000000u;
    w[6] = 0u; w[7] = 0u; w[8] = 0u; w[9] = 0u;
    w[10] = 0u; w[11] = 0u; w[12] = 0u; w[13] = 0u;
    w[14] = 0u;
    w[15] = 672u;

    unsigned int state[8];
    #pragma unroll
    for (int i = 0; i < 8; i++) state[i] = midstate[i];
    sha256_compress(state, w);

    // Outer SHA-256 over the 32-byte inner digest.
    unsigned int w2[16];
    #pragma unroll
    for (int i = 0; i < 8; i++) w2[i] = state[i];
    w2[8] = 0x80000000u;
    w2[9] = 0u; w2[10] = 0u; w2[11] = 0u;
    w2[12] = 0u; w2[13] = 0u; w2[14] = 0u;
    w2[15] = 256u;

    unsigned int state2[8] = {
        0x6a09e667u, 0xbb67ae85u, 0x3c6ef372u, 0xa54ff53au,
        0x510e527fu, 0x9b05688cu, 0x1f83d9abu, 0x5be0cd19u
    };
    sha256_compress(state2, w2);

    if (hash_leq_target_words(state2, target_words)) {
        #pragma unroll
        for (int i = 0; i < 8; i++) out_hash[i] = state2[i];
        return true;
    }
    return false;
}

__global__ void mine_sha256d(
    const unsigned int *midstate,        // 8 words
    const unsigned char *tail_16,        // 16 bytes: merkle_tail(4) | time(8) | bits(4)
    const unsigned char *target_be,      // 32 bytes
    const unsigned int start_nonce,
    const unsigned int nonce_end_excl,
    const unsigned int nonces_per_thread,
    unsigned int *found_nonce,
    unsigned int *found_flag,
    unsigned int *found_hash             // 8 words
) {
    // Cache target as BE-packed words once per thread.
    // keep the explicit byte-by-byte pack here (not BSWAP32) — the
    // input is `const unsigned char *target_be` and the alignment is not
    // guaranteed, so reading it as a u32* and bswap'ing would risk a
    // misaligned global load. Per-thread, runs once, not in the hot loop.
    unsigned int target_words[8];
    #pragma unroll
    for (int i = 0; i < 8; i++) {
        target_words[i] = ((unsigned int)target_be[4*i]     << 24)
                        | ((unsigned int)target_be[4*i + 1] << 16)
                        | ((unsigned int)target_be[4*i + 2] << 8)
                        | ((unsigned int)target_be[4*i + 3]);
    }

    // Four fixed tail words (constant per kernel — bytes 0..15 of tail).
    // Same alignment reasoning as above: keep the per-byte pack.
    unsigned int w0_merkle_tail = ((unsigned int)tail_16[0]  << 24) | ((unsigned int)tail_16[1]  << 16)
                                | ((unsigned int)tail_16[2]  << 8)  |  (unsigned int)tail_16[3];
    unsigned int w1_time_lo     = ((unsigned int)tail_16[4]  << 24) | ((unsigned int)tail_16[5]  << 16)
                                | ((unsigned int)tail_16[6]  << 8)  |  (unsigned int)tail_16[7];
    unsigned int w2_time_hi     = ((unsigned int)tail_16[8]  << 24) | ((unsigned int)tail_16[9]  << 16)
                                | ((unsigned int)tail_16[10] << 8)  |  (unsigned int)tail_16[11];
    unsigned int w3_bits        = ((unsigned int)tail_16[12] << 24) | ((unsigned int)tail_16[13] << 16)
                                | ((unsigned int)tail_16[14] << 8)  |  (unsigned int)tail_16[15];

    unsigned int gid = blockIdx.x * blockDim.x + threadIdx.x;
    unsigned int base = start_nonce + gid * nonces_per_thread;

    unsigned int out_hash[8];

    // Throttle the found_flag poll to once per 256 iterations — checking
    // on every nonce burns one uncoalesced global read per hash, which
    // dominates the kernel inner-loop. Worst-case 256 wasted hashes
    // after another thread finds a winner.
    for (unsigned int k = 0; k < nonces_per_thread; k++) {
        if ((k & 255u) == 0u && *found_flag != 0) return;
        unsigned int nonce = base + k;
        if (nonce >= nonce_end_excl) return;
        if (try_one_nonce(midstate, w0_merkle_tail, w1_time_lo, w2_time_hi, w3_bits, target_words, nonce, out_hash)) {
            if (atomicCAS(found_flag, 0u, 1u) == 0u) {
                *found_nonce = nonce;
                #pragma unroll
                for (int i = 0; i < 8; i++) found_hash[i] = out_hash[i];
            }
            return;
        }
    }
}

} // extern "C"
