//! Vendored consensus types.
//!
//! These types model the CSD consensus wire format. They are vendored here so
//! the miner builds as a fully standalone single crate.
//!
//! CONSENSUS WARNING: every byte layout here is wire-visible and must match
//! the network exactly. Do not reorder fields, change integer widths, or touch
//! encoded forms — doing so changes the consensus wire protocol and would
//! break compatibility.
//!
//! Scope: only the symbols the miner actually consumes are vendored —
//! `Hash32`, `WorkTemplate`, `WorkSubmission`, `HexHash32`, and the two
//! helper functions. The miner already carries its own copies of
//! `sha256d`, `merkle_root_from_branch`, and header packing (see
//! `sha256d_cpu.rs` and `coinbase.rs`), so none of the consensus crate's
//! crypto/merkle machinery is needed here.

use serde::{Deserialize, Serialize};

/// 32-byte hash, big-endian on the wire (matches header serialization).
pub type Hash32 = [u8; 32];

/// A single work unit served to a miner.
///
/// Fields are JSON-friendly so the same struct is sent over the wire and
/// consumed here directly.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct WorkTemplate {
    /// Monotonic id; the miner echoes it on submit so the node can detect
    /// stale work.
    pub id: u64,

    /// Block-template construction inputs (fixed once chosen by the node):
    pub version: u32,
    /// 32-byte big-endian previous-block hash, matching how header
    /// serialization stores the raw 32 bytes.
    #[serde(with = "hex_array_32")]
    pub prev: Hash32,
    pub time: u64,
    pub bits: u32,

    /// 32-byte big-endian PoW target derived from `bits`. The miner uses
    /// the wire form directly so it doesn't have to re-derive it.
    #[serde(with = "hex_array_32")]
    pub target: Hash32,

    /// Coinbase tx split into prefix||extranonce||suffix.
    /// The miner builds `coinbase_tx = prefix || extranonce.to_le_bytes() || suffix`.
    #[serde(with = "serde_hex")]
    pub coinbase_prefix: Vec<u8>,
    pub extranonce_size: u8, // bytes, usually 8
    #[serde(with = "serde_hex")]
    pub coinbase_suffix: Vec<u8>,

    /// Merkle branch from the coinbase (leaf 0) up to the root. Each entry
    /// is a 32-byte sibling hash. `merkle_root_from_branch` consumes this.
    pub merkle_branch: Vec<HexHash32>,

    /// Suggested nonce-space partitioning. The node returns the full 32-bit
    /// range here; multi-miner orchestration can subdivide. The kernel is
    /// responsible for sweeping its assigned subrange.
    pub nonce_start: u32,
    pub nonce_end: u32,

    /// Block height for human-readable logging / per-template freshness.
    pub height: u64,
}

/// Same-shape submission payload sent by miners.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct WorkSubmission {
    pub id: u64,
    pub nonce: u32,
    pub extranonce: u64,
    pub time: u64,
}

/// Helper: convert a `Vec<Hash32>` merkle branch into the JSON-friendly form.
pub fn merkle_branch_hex(branch: &[Hash32]) -> Vec<HexHash32> {
    branch.iter().map(|h| HexHash32(*h)).collect()
}

/// Convenience: rebuild the raw 84-byte work header for kernel inputs.
/// Coinbase txid is recomputed by the caller; we only own the header bytes.
pub fn work_template_encode(template: &WorkTemplate, merkle_root: Hash32, nonce: u32) -> [u8; 84] {
    let mut out = [0u8; 84];
    out[0..4].copy_from_slice(&template.version.to_le_bytes());
    out[4..36].copy_from_slice(&template.prev);
    out[36..68].copy_from_slice(&merkle_root);
    out[68..76].copy_from_slice(&template.time.to_le_bytes());
    out[76..80].copy_from_slice(&template.bits.to_le_bytes());
    out[80..84].copy_from_slice(&nonce.to_le_bytes());
    out
}

// -----------------------------------------------------------------------------
// JSON hex helpers
// -----------------------------------------------------------------------------

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(transparent)]
pub struct HexHash32(#[serde(with = "hex_array_32")] pub Hash32);

mod hex_array_32 {
    use super::*;
    use serde::{Deserializer, Serializer};

    pub fn serialize<S: Serializer>(b: &Hash32, s: S) -> Result<S::Ok, S::Error> {
        s.serialize_str(&hex::encode(b))
    }
    pub fn deserialize<'de, D: Deserializer<'de>>(d: D) -> Result<Hash32, D::Error> {
        use serde::Deserialize;
        let s: String = String::deserialize(d)?;
        let v = hex::decode(s).map_err(serde::de::Error::custom)?;
        if v.len() != 32 {
            return Err(serde::de::Error::custom("hash must be 32 bytes"));
        }
        let mut out = [0u8; 32];
        out.copy_from_slice(&v);
        Ok(out)
    }
}

mod serde_hex {
    use serde::{Deserializer, Serializer};

    pub fn serialize<S: Serializer>(b: &Vec<u8>, s: S) -> Result<S::Ok, S::Error> {
        s.serialize_str(&hex::encode(b))
    }
    pub fn deserialize<'de, D: Deserializer<'de>>(d: D) -> Result<Vec<u8>, D::Error> {
        use serde::Deserialize;
        let s: String = String::deserialize(d)?;
        hex::decode(s).map_err(serde::de::Error::custom)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn encode_header_matches_layout() {
        let t = WorkTemplate {
            id: 1,
            version: 1,
            prev: [0xAAu8; 32],
            time: 0x0807060504030201u64,
            bits: 0x1e00ffff,
            target: [0u8; 32],
            coinbase_prefix: vec![],
            extranonce_size: 8,
            coinbase_suffix: vec![],
            merkle_branch: vec![],
            nonce_start: 0,
            nonce_end: u32::MAX,
            height: 0,
        };
        let mr = [0xBBu8; 32];
        let nonce = 0xDEADBEEF;
        let buf = work_template_encode(&t, mr, nonce);

        assert_eq!(&buf[0..4], &[1, 0, 0, 0]);
        assert_eq!(&buf[4..36], &[0xAAu8; 32]);
        assert_eq!(&buf[36..68], &[0xBBu8; 32]);
        assert_eq!(&buf[68..76], &0x0807060504030201u64.to_le_bytes());
        assert_eq!(&buf[76..80], &0x1e00ffffu32.to_le_bytes());
        assert_eq!(&buf[80..84], &0xDEADBEEFu32.to_le_bytes());
    }
}
