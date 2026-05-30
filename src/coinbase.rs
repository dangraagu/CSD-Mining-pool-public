//! Coinbase tx encoding helpers.
//!
//! The work template splits the coinbase tx into:
//!     coinbase_prefix || extranonce_le[8] || coinbase_suffix
//!
//! The miner mutates only the 8 extranonce bytes, so:
//!     coinbase_bytes = coinbase_prefix || extranonce.to_le_bytes() || coinbase_suffix
//!     coinbase_txid  = sha256d(coinbase_bytes)
//!
//! `txid()` is `sha256d(consensus_bincode(stripped_tx))`. For coinbase
//! `stripped_tx == tx` (we do not clear coinbase script_sig), so the bytes
//! the node serves already are the txid preimage.

use crate::sha256d_cpu::sha256d;

#[inline]
pub fn build_coinbase_bytes(prefix: &[u8], extranonce: u64, suffix: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(prefix.len() + 8 + suffix.len());
    out.extend_from_slice(prefix);
    out.extend_from_slice(&extranonce.to_le_bytes());
    out.extend_from_slice(suffix);
    out
}

#[inline]
pub fn coinbase_txid(prefix: &[u8], extranonce: u64, suffix: &[u8]) -> [u8; 32] {
    let bytes = build_coinbase_bytes(prefix, extranonce, suffix);
    sha256d(&bytes)
}

/// Recompute the merkle root from a precomputed branch and a leaf hash.
/// Mirrors `csd_consensus::merkle_root_from_branch` so the miner does not
/// need to depend on the full consensus crate's serde machinery.
pub fn merkle_root_from_branch(leaf: [u8; 32], branch: &[[u8; 32]], leaf_index: usize) -> [u8; 32] {
    let mut h = leaf;
    let mut idx = leaf_index;
    for sib in branch {
        let mut buf = [0u8; 64];
        if idx % 2 == 0 {
            buf[..32].copy_from_slice(&h);
            buf[32..].copy_from_slice(sib);
        } else {
            buf[..32].copy_from_slice(sib);
            buf[32..].copy_from_slice(&h);
        }
        h = sha256d(&buf);
        idx /= 2;
    }
    h
}

/// Build the 84-byte block header wire form from a template's fixed fields
/// plus a candidate merkle root and nonce. (csd1 mainnet layout: `time` is u64.)
#[inline]
pub fn header_84(
    version: u32,
    prev: &[u8; 32],
    merkle: &[u8; 32],
    time: u64,
    bits: u32,
    nonce: u32,
) -> [u8; 84] {
    let mut out = [0u8; 84];
    out[0..4].copy_from_slice(&version.to_le_bytes());
    out[4..36].copy_from_slice(prev);
    out[36..68].copy_from_slice(merkle);
    out[68..76].copy_from_slice(&time.to_le_bytes());
    out[76..80].copy_from_slice(&bits.to_le_bytes());
    out[80..84].copy_from_slice(&nonce.to_le_bytes());
    out
}
