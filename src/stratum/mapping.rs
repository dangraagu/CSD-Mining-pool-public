//! Map a Stratum `mining.notify` into the miner's [`WorkTemplate`] so the
//! reconstructed 84-byte csd1 header is **byte-identical** to the one the pool
//! bridge verifies in its `verify_submit`. If a single byte differs, every
//! share the miner submits is rejected, so the mapping below is locked against
//! the bridge's `block::verify::assemble_header` + `stratum::server::verify_submit`.
//!
//! Three subtleties that are easy to get wrong (and which the equivalence test
//! pins down):
//!   1. **prev reversal.** Stratum sends `prev_hash` as *big-endian hex*. The
//!      csd1 header (and the miner's [`header_84`]) place the previous-block
//!      hash as *raw 32 bytes in the header's stored order*, which is the
//!      reverse of the Stratum hex. The bridge reverses it in `verify_submit`
//!      (`prev_le.reverse()`); we must reverse it here too.
//!   2. **extranonce split.** The bridge reassembles the coinbase as
//!      `coinb1 ‖ xn1[4] ‖ xn2[4] ‖ coinb2` with `xn1`/`xn2` as *raw 4-byte LE
//!      slices*. The miner builds `prefix ‖ extranonce.to_le_bytes() ‖ suffix`
//!      with a single `u64` extranonce. For the bytes to match, the u64 must be
//!      `extranonce = (xn1_low as u64) | ((xn2 as u64) << 32)` — low 32 bits are
//!      the pool-fixed xn1, high 32 bits are the miner-rolled xn2.
//!   3. **ntime → u64.** Stratum `ntime` is 4 hex bytes; the csd1 header widened
//!      `time` to a u64 at bytes [68..76]. We parse the 4-byte ntime and
//!      zero-extend to u64 (`ntime as u64`), exactly as the bridge does.

use crate::stratum::protocol::NotifyParams;
use crate::csd_consensus::{HexHash32, WorkTemplate};
use anyhow::{anyhow, Context, Result};

/// Result of mapping a `mining.notify` (+ session extranonce1 + share target)
/// into a miner work unit. Carries the [`WorkTemplate`] the mining loop drives,
/// plus the pieces the loop needs to build a matching `mining.submit`:
///   - `job_id`: echoed back verbatim on submit.
///   - `xn1_low`: the session's extranonce1 as a little-endian `u32`, i.e. the
///     **low 32 bits** of the 8-byte coinbase extranonce slot. The loop composes
///     the full extranonce as `(xn1_low as u64) | ((xn2 as u64) << 32)`.
#[derive(Clone, Debug)]
pub struct MappedJob {
    pub template: WorkTemplate,
    pub job_id: String,
    pub xn1_low: u32,
}

/// The three mutable fields a `mining.submit` carries for a found share, already
/// hex-encoded the way the bridge's `verify_submit` decodes them.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SubmitFields {
    /// `hex(xn2.to_le_bytes())` — the raw 4 LE bytes of the miner-rolled
    /// extranonce2. The bridge re-splits the coinbase using exactly these bytes.
    pub extranonce2_hex: String,
    /// `format!("{:08x}", time as u32)` — the 4-byte ntime echoed back.
    pub ntime_hex: String,
    /// `format!("{:08x}", nonce)` — the winning nonce.
    pub nonce_hex: String,
}

/// Compose the full 8-byte coinbase extranonce from the pool-fixed low half and
/// the miner-rolled high half. THIS is the rule the byte-equivalence test pins:
/// the resulting `u64.to_le_bytes()` is `xn1_low_le(4) ‖ xn2_le(4)`, which is
/// exactly what the bridge concatenates (`xn1[4] ‖ xn2[4]`) when reassembling
/// the coinbase.
#[inline]
pub fn compose_extranonce(xn1_low: u32, xn2: u32) -> u64 {
    (xn1_low as u64) | ((xn2 as u64) << 32)
}

/// Build the `mining.submit` field trio for a found share. `xn2` is the
/// miner-rolled high half of the extranonce; `time`/`nonce` are the header
/// values that produced the winning hash.
pub fn build_submit(xn2: u32, time: u64, nonce: u32) -> SubmitFields {
    SubmitFields {
        extranonce2_hex: hex::encode(xn2.to_le_bytes()),
        ntime_hex: format!("{:08x}", time as u32),
        nonce_hex: format!("{:08x}", nonce),
    }
}

/// Decode a hex string into a fixed `[u8; 32]`, erroring on bad hex or wrong
/// length (a mis-sized hash would silently corrupt the header).
fn decode_hash32(hex_str: &str, field: &str) -> Result<[u8; 32]> {
    let v = hex::decode(hex_str).with_context(|| format!("{field} is not valid hex"))?;
    if v.len() != 32 {
        return Err(anyhow!("{field} must be 32 bytes, got {}", v.len()));
    }
    let mut out = [0u8; 32];
    out.copy_from_slice(&v);
    Ok(out)
}

/// Map a parsed `mining.notify` into a [`MappedJob`].
///
/// `extranonce1` is the session extranonce1 from `mining.subscribe`; the bridge
/// advertises `extranonce2_size = 4`, so the 8-byte coinbase slot is
/// `xn1(4) ‖ xn2(4)` and `extranonce1` MUST be exactly 4 bytes. `share_target`
/// is the LE 32-byte target derived from the current `mining.set_difficulty`.
///
/// The produced [`WorkTemplate`] drives [`header_84`] (via the mining loop) to
/// the EXACT 84 bytes the bridge's `assemble_header` builds for the same job.
pub fn notify_to_template(
    n: &NotifyParams,
    extranonce1: &[u8],
    share_target: [u8; 32],
) -> Result<MappedJob> {
    // Fixed header scalars (hex → integer), same parses as verify_submit.
    let version = u32::from_str_radix(&n.version_hex, 16)
        .with_context(|| format!("version_hex {:?}", n.version_hex))?;
    let bits = u32::from_str_radix(&n.nbits_hex, 16)
        .with_context(|| format!("nbits_hex {:?}", n.nbits_hex))?;
    // 4-byte Stratum ntime, zero-extended to the csd1 header's u64 time field.
    let time = u32::from_str_radix(&n.ntime_hex, 16)
        .with_context(|| format!("ntime_hex {:?}", n.ntime_hex))? as u64;

    // prev: Stratum sends big-endian hex; the header stores it reversed.
    let mut prev = decode_hash32(&n.prev_hash_be_hex, "prev_hash_be_hex")?;
    prev.reverse();

    // Coinbase halves are placed verbatim around the 8-byte extranonce slot.
    let coinbase_prefix =
        hex::decode(&n.coinb1_hex).context("coinb1_hex is not valid hex")?;
    let coinbase_suffix =
        hex::decode(&n.coinb2_hex).context("coinb2_hex is not valid hex")?;

    // Merkle branch: each entry is a 32-byte sibling hash (raw, as the bridge
    // walks it). Stratum sends these in the header's stored byte order already
    // (the bridge feeds them straight into sha256d without reversal).
    let merkle_branch = n
        .merkle_branches_hex
        .iter()
        .enumerate()
        .map(|(i, h)| {
            decode_hash32(h, &format!("merkle_branch[{i}]")).map(HexHash32)
        })
        .collect::<Result<Vec<_>>>()?;

    // The bridge advertises extranonce2_size = 4; the coinbase slot is 8 bytes
    // (xn1(4) ‖ xn2(4)). extranonce1 must therefore be exactly 4 bytes.
    let xn1_arr: [u8; 4] = extranonce1
        .try_into()
        .map_err(|_| anyhow!("extranonce1 must be exactly 4 bytes, got {}", extranonce1.len()))?;
    let xn1_low = u32::from_le_bytes(xn1_arr);

    // `id` is local-only (the node-mode loop echoes it for staleness; in pool
    // mode the bridge tracks freshness by job_id). A stable hash of job_id keeps
    // it deterministic without colliding across jobs in logs.
    let id = fnv1a64(n.job_id.as_bytes());

    let template = WorkTemplate {
        id,
        version,
        prev,
        time,
        bits,
        target: share_target,
        coinbase_prefix,
        extranonce_size: 8,
        coinbase_suffix,
        merkle_branch,
        nonce_start: 0,
        nonce_end: u32::MAX,
        height: 0, // mining.notify carries no height
    };

    Ok(MappedJob {
        template,
        job_id: n.job_id.clone(),
        xn1_low,
    })
}

/// Tiny deterministic 64-bit hash for the local-only template `id` (FNV-1a).
/// Not security-sensitive — only used so logs can correlate a template back to
/// its `job_id`.
fn fnv1a64(bytes: &[u8]) -> u64 {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for &b in bytes {
        h ^= b as u64;
        h = h.wrapping_mul(0x0000_0100_0000_01b3);
    }
    h
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::coinbase::{coinbase_txid, header_84, merkle_root_from_branch};
    use crate::sha256d_cpu::sha256d;

    /// Build a fixture `mining.notify` with an asymmetric prev (so a missing
    /// reversal is detectable), a 2-entry merkle branch, and distinct coinbase
    /// halves. ntime is chosen so `as u64` zero-extension is exercised.
    fn fixture_notify() -> NotifyParams {
        // 32-byte prev in *big-endian hex* (Stratum convention): 00 01 .. 1f.
        // Asymmetric: byte 0 != byte 31, so reversing changes the bytes.
        let prev_be: String = (0u8..32).map(|i| format!("{:02x}", i)).collect();
        // Two arbitrary 32-byte merkle siblings.
        let br0: String = (0u8..32).map(|i| format!("{:02x}", 0x40 + i)).collect();
        let br1: String = (0u8..32).map(|i| format!("{:02x}", 0xa0u8.wrapping_add(i))).collect();
        NotifyParams {
            job_id: "deadbeefcafe".to_string(),
            prev_hash_be_hex: prev_be,
            coinb1_hex: "01000000aabbcc".to_string(), // arbitrary prefix bytes
            coinb2_hex: "ffeeddccbbaa99".to_string(), // arbitrary suffix bytes
            merkle_branches_hex: vec![br0, br1],
            version_hex: "20000000".to_string(),
            nbits_hex: "1d00ffff".to_string(),
            ntime_hex: "665544cc".to_string(), // high bit set in low byte; fits u32
            clean_jobs: true,
        }
    }

    /// Independently compute the golden 84-byte header the BRIDGE would verify,
    /// from raw notify fields + chosen extranonces/nonce. This deliberately does
    /// NOT call `notify_to_template`; it mirrors `verify_submit` step-for-step so
    /// the equivalence assertion proves the mapping, not a shared helper.
    fn bridge_golden_header(
        n: &NotifyParams,
        xn1: [u8; 4],
        xn2: u32,
        nonce: u32,
    ) -> [u8; 84] {
        // coinbase = coinb1 ‖ xn1[4] ‖ xn2_le[4] ‖ coinb2   (bridge `reassemble`)
        let mut coinbase = hex::decode(&n.coinb1_hex).unwrap();
        coinbase.extend_from_slice(&xn1);
        coinbase.extend_from_slice(&xn2.to_le_bytes());
        coinbase.extend_from_slice(&hex::decode(&n.coinb2_hex).unwrap());
        let cb = sha256d(&coinbase);

        // merkle = fold sha256d(h ‖ branch) over each branch (bridge walk).
        let mut merkle = cb;
        for h in &n.merkle_branches_hex {
            let sib = hex::decode(h).unwrap();
            let mut buf = [0u8; 64];
            buf[..32].copy_from_slice(&merkle);
            buf[32..].copy_from_slice(&sib);
            merkle = sha256d(&buf);
        }

        // prev = reverse(prev_be)   (bridge `prev_le.reverse()`)
        let mut prev = hex::decode(&n.prev_hash_be_hex).unwrap();
        prev.reverse();
        let mut prev_arr = [0u8; 32];
        prev_arr.copy_from_slice(&prev);

        let version = u32::from_str_radix(&n.version_hex, 16).unwrap();
        let bits = u32::from_str_radix(&n.nbits_hex, 16).unwrap();
        let time = u32::from_str_radix(&n.ntime_hex, 16).unwrap() as u64;

        // header = version_LE ‖ prev ‖ merkle ‖ time_u64_LE ‖ bits_LE ‖ nonce_LE
        let mut h = [0u8; 84];
        h[0..4].copy_from_slice(&version.to_le_bytes());
        h[4..36].copy_from_slice(&prev_arr);
        h[36..68].copy_from_slice(&merkle);
        h[68..76].copy_from_slice(&time.to_le_bytes());
        h[76..80].copy_from_slice(&bits.to_le_bytes());
        h[80..84].copy_from_slice(&nonce.to_le_bytes());
        h
    }

    /// Drive a `MappedJob` through the miner's real header path (coinbase_txid →
    /// merkle_root_from_branch → header_84) for a chosen xn2 + nonce.
    fn miner_header_from_mapped(job: &MappedJob, xn2: u32, nonce: u32) -> [u8; 84] {
        let t = &job.template;
        let extranonce = compose_extranonce(job.xn1_low, xn2);
        let cb = coinbase_txid(&t.coinbase_prefix, extranonce, &t.coinbase_suffix);
        let branch: Vec<[u8; 32]> = t.merkle_branch.iter().map(|b| b.0).collect();
        let merkle = merkle_root_from_branch(cb, &branch, 0);
        header_84(t.version, &t.prev, &merkle, t.time, t.bits, nonce)
    }

    /// THE byte-equivalence test. The miner's reconstructed header (driven by
    /// `notify_to_template`'s WorkTemplate) must equal the independently
    /// hand-computed bridge golden, byte for byte. Passing proves prev-reversal,
    /// the xn1/xn2 split, and ntime→u64 are all correct simultaneously.
    #[test]
    fn header_byte_equivalent_to_bridge_golden() {
        let n = fixture_notify();
        let xn1 = [0xaa, 0xbb, 0xcc, 0xdd];
        let xn2: u32 = 0x0000_0001;
        let nonce: u32 = 0x1234_5678;

        let target = [0u8; 32];
        let job = notify_to_template(&n, &xn1, target).unwrap();

        let miner = miner_header_from_mapped(&job, xn2, nonce);
        let golden = bridge_golden_header(&n, xn1, xn2, nonce);

        assert_eq!(
            hex::encode(miner),
            hex::encode(golden),
            "miner header must be byte-identical to the bridge golden"
        );
    }

    /// xn1_low is the LE-decoded extranonce1, and the composed extranonce's low
    /// 4 LE bytes equal xn1 while the high 4 equal xn2 — the exact split the
    /// bridge re-concatenates.
    #[test]
    fn extranonce_split_low_is_xn1_high_is_xn2() {
        let n = fixture_notify();
        let xn1 = [0xaa, 0xbb, 0xcc, 0xdd];
        let job = notify_to_template(&n, &xn1, [0u8; 32]).unwrap();
        assert_eq!(job.xn1_low, u32::from_le_bytes(xn1));

        let xn2: u32 = 0x0000_0001;
        let extranonce = compose_extranonce(job.xn1_low, xn2);
        let le = extranonce.to_le_bytes();
        assert_eq!(&le[0..4], &xn1, "low 4 bytes must be xn1 (raw LE)");
        assert_eq!(&le[4..8], &xn2.to_le_bytes(), "high 4 bytes must be xn2 LE");
    }

    /// Guard against accidentally NOT reversing prev being masked by symmetry:
    /// flipping one byte of the (asymmetric) prev_hash_be_hex must change the
    /// header. Uses an asymmetric prev so a no-op reversal can't hide the bug.
    #[test]
    fn flipping_prev_byte_changes_header() {
        let n = fixture_notify();
        let xn1 = [0xaa, 0xbb, 0xcc, 0xdd];
        let job_a = notify_to_template(&n, &xn1, [0u8; 32]).unwrap();
        let hdr_a = miner_header_from_mapped(&job_a, 0x0000_0001, 0x1234_5678);

        // Flip the FIRST hex byte of prev (lands at header[35] after reversal).
        let mut n2 = n.clone();
        let mut prev_bytes = hex::decode(&n2.prev_hash_be_hex).unwrap();
        prev_bytes[0] ^= 0xff;
        n2.prev_hash_be_hex = hex::encode(&prev_bytes);
        let job_b = notify_to_template(&n2, &xn1, [0u8; 32]).unwrap();
        let hdr_b = miner_header_from_mapped(&job_b, 0x0000_0001, 0x1234_5678);

        assert_ne!(
            hex::encode(hdr_a),
            hex::encode(hdr_b),
            "changing prev must change the header (proves prev is actually used + reversed)"
        );
        // And specifically the reversed first byte lands at header[35].
        assert_ne!(hdr_a[35], hdr_b[35], "flipped prev byte 0 must surface at header[35]");
    }

    /// `build_submit`'s extranonce2_hex must round-trip to the same 4 bytes the
    /// golden used, so the bridge's `reassemble` rebuilds the identical coinbase.
    #[test]
    fn submit_extranonce2_round_trips_to_bridge_coinbase() {
        let n = fixture_notify();
        let xn1 = [0xaa, 0xbb, 0xcc, 0xdd];
        let xn2: u32 = 0x0a0b_0c0d;
        let nonce: u32 = 0x1234_5678;
        let time = u32::from_str_radix(&n.ntime_hex, 16).unwrap() as u64;

        let fields = build_submit(xn2, time, nonce);
        // ntime/nonce are 8-hex-char lowercase.
        assert_eq!(fields.ntime_hex, format!("{:08x}", time as u32));
        assert_eq!(fields.nonce_hex, format!("{:08x}", nonce));

        // The bridge decodes extranonce2_hex and re-splits the coinbase. Rebuild
        // the coinbase both ways and confirm equality.
        let xn2_back = hex::decode(&fields.extranonce2_hex).unwrap();
        assert_eq!(xn2_back.len(), 4, "extranonce2 must be exactly 4 bytes");

        // miner-side coinbase via composed u64
        let job = notify_to_template(&n, &xn1, [0u8; 32]).unwrap();
        let extranonce = compose_extranonce(job.xn1_low, xn2);
        let miner_cb = crate::coinbase::build_coinbase_bytes(
            &job.template.coinbase_prefix,
            extranonce,
            &job.template.coinbase_suffix,
        );
        // bridge-side coinbase via coinb1 ‖ xn1 ‖ xn2_back ‖ coinb2
        let mut bridge_cb = hex::decode(&n.coinb1_hex).unwrap();
        bridge_cb.extend_from_slice(&xn1);
        bridge_cb.extend_from_slice(&xn2_back);
        bridge_cb.extend_from_slice(&hex::decode(&n.coinb2_hex).unwrap());

        assert_eq!(
            hex::encode(&miner_cb),
            hex::encode(&bridge_cb),
            "miner coinbase must equal the bridge's reassembled coinbase"
        );
    }

    /// extranonce1 that isn't exactly 4 bytes is a hard error (bridge advertises
    /// extranonce2_size = 4; the slot is xn1(4) ‖ xn2(4)).
    #[test]
    fn rejects_non_4_byte_extranonce1() {
        let n = fixture_notify();
        assert!(notify_to_template(&n, &[0xaa, 0xbb, 0xcc], [0u8; 32]).is_err());
        assert!(notify_to_template(&n, &[0xaa, 0xbb, 0xcc, 0xdd, 0xee], [0u8; 32]).is_err());
    }
}
