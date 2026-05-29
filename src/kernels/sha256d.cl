// csd GPU mining kernel — OpenCL, 2-queue pipelined design.
//
// Same shape as the CUDA kernel: each thread sweeps `nonces_per_thread`
// consecutive nonces in a tight inner loop. Two host-side command queues
// alternate launches so the GPU stays busy while one queue's readback
// runs.
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
// OpenCL's `rotate(x, c)` builtin compiles to the hardware rotate
// instruction on every vendor we care about. We use ROTR(x, n) = rotate
// left by (32 - n).

#define ROTR(x, n) rotate((uint)(x), (uint)(32u - (n)))

__constant uint K[64] = {
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

inline void sha256_compress(uint state[8], const uint w_in[16]) {
    uint w[64];
    for (int i = 0; i < 16; i++) w[i] = w_in[i];
    for (int i = 16; i < 64; i++) {
        uint s0 = ROTR(w[i-15], 7) ^ ROTR(w[i-15], 18) ^ (w[i-15] >> 3);
        uint s1 = ROTR(w[i-2], 17) ^ ROTR(w[i-2], 19) ^ (w[i-2] >> 10);
        w[i] = w[i-16] + s0 + w[i-7] + s1;
    }
    uint a = state[0], b = state[1], c = state[2], d = state[3];
    uint e = state[4], f = state[5], g = state[6], h = state[7];
    for (int i = 0; i < 64; i++) {
        uint S1 = ROTR(e, 6) ^ ROTR(e, 11) ^ ROTR(e, 25);
        uint ch = (e & f) ^ (~e & g);
        uint t1 = h + S1 + ch + K[i] + w[i];
        uint S0 = ROTR(a, 2) ^ ROTR(a, 13) ^ ROTR(a, 22);
        uint maj = (a & b) ^ (a & c) ^ (b & c);
        uint t2 = S0 + maj;
        h = g; g = f; f = e; e = d + t1;
        d = c; c = b; b = a; a = t1 + t2;
    }
    state[0] += a; state[1] += b; state[2] += c; state[3] += d;
    state[4] += e; state[5] += f; state[6] += g; state[7] += h;
}

inline bool hash_leq_target_words(const uint state[8], const uint target_words[8]) {
    for (int i = 0; i < 8; i++) {
        if (state[i] < target_words[i]) return true;
        if (state[i] > target_words[i]) return false;
    }
    return true;
}

inline bool try_one_nonce(
    const uint midstate[8],
    uint w0_merkle_tail,
    uint w1_time_lo,
    uint w2_time_hi,
    uint w3_bits,
    const uint target_words[8],
    uint nonce,
    uint out_hash[8]
) {
    uint w[16];
    w[0] = w0_merkle_tail;
    w[1] = w1_time_lo;
    w[2] = w2_time_hi;
    w[3] = w3_bits;
    w[4] = ((uint)((nonce >> 0) & 0xff) << 24)
         | ((uint)((nonce >> 8) & 0xff) << 16)
         | ((uint)((nonce >> 16) & 0xff) << 8)
         | ((uint)((nonce >> 24) & 0xff));
    w[5] = 0x80000000u;
    w[6] = 0u; w[7] = 0u; w[8] = 0u; w[9] = 0u;
    w[10] = 0u; w[11] = 0u; w[12] = 0u; w[13] = 0u;
    w[14] = 0u;
    w[15] = 672u;

    uint state[8];
    for (int i = 0; i < 8; i++) state[i] = midstate[i];
    sha256_compress(state, w);

    uint w2[16];
    for (int i = 0; i < 8; i++) w2[i] = state[i];
    w2[8] = 0x80000000u;
    w2[9] = 0u; w2[10] = 0u; w2[11] = 0u;
    w2[12] = 0u; w2[13] = 0u; w2[14] = 0u;
    w2[15] = 256u;

    uint state2[8] = {
        0x6a09e667u, 0xbb67ae85u, 0x3c6ef372u, 0xa54ff53au,
        0x510e527fu, 0x9b05688cu, 0x1f83d9abu, 0x5be0cd19u
    };
    sha256_compress(state2, w2);

    if (hash_leq_target_words(state2, target_words)) {
        for (int i = 0; i < 8; i++) out_hash[i] = state2[i];
        return true;
    }
    return false;
}

__kernel void mine_sha256d(
    __constant const uint *midstate_in,
    __constant const uchar *tail_16,
    __constant const uchar *target_be,
    const uint start_nonce,
    const uint nonce_end_excl,
    const uint nonces_per_thread,
    __global uint *found_nonce,
    __global uint *found_flag,
    __global uint *found_hash
) {
    // Copy constants into private memory so helper fns can take a plain
    // pointer (OpenCL is strict about address spaces).
    uint midstate[8];
    for (int i = 0; i < 8; i++) midstate[i] = midstate_in[i];

    uint target_words[8];
    for (int i = 0; i < 8; i++) {
        target_words[i] = ((uint)target_be[4*i]     << 24)
                        | ((uint)target_be[4*i + 1] << 16)
                        | ((uint)target_be[4*i + 2] << 8)
                        | ((uint)target_be[4*i + 3]);
    }
    // tail_16 = bytes 64..80 of the 84-byte header = merkle_tail(4) | time(8) | bits(4)
    uint w0_merkle_tail = ((uint)tail_16[0]  << 24) | ((uint)tail_16[1]  << 16)
                        | ((uint)tail_16[2]  << 8)  |  (uint)tail_16[3];
    uint w1_time_lo     = ((uint)tail_16[4]  << 24) | ((uint)tail_16[5]  << 16)
                        | ((uint)tail_16[6]  << 8)  |  (uint)tail_16[7];
    uint w2_time_hi     = ((uint)tail_16[8]  << 24) | ((uint)tail_16[9]  << 16)
                        | ((uint)tail_16[10] << 8)  |  (uint)tail_16[11];
    uint w3_bits        = ((uint)tail_16[12] << 24) | ((uint)tail_16[13] << 16)
                        | ((uint)tail_16[14] << 8)  |  (uint)tail_16[15];

    uint gid = (uint)get_global_id(0);
    uint base = start_nonce + gid * nonces_per_thread;

    uint out_hash[8];

    // Only poll `found_flag` every 256 iterations — checking on every
    // nonce burns one uncoalesced global read per hash (a big share of
    // memory bandwidth). Worst-case 256 wasted hashes after another
    // thread finds a winner; with nonces_per_thread=4096 that's a 16x
    // throughput improvement over per-iteration polling.
    for (uint k = 0; k < nonces_per_thread; k++) {
        if ((k & 255u) == 0u && *found_flag != 0u) return;
        uint nonce = base + k;
        if (nonce >= nonce_end_excl) return;
        if (try_one_nonce(midstate, w0_merkle_tail, w1_time_lo, w2_time_hi, w3_bits, target_words, nonce, out_hash)) {
            if (atomic_cmpxchg(found_flag, 0u, 1u) == 0u) {
                *found_nonce = nonce;
                for (int i = 0; i < 8; i++) found_hash[i] = out_hash[i];
            }
            return;
        }
    }
}
