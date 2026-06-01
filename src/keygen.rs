//! `newwallet` subcommand — generate a fresh CSD payout wallet locally.
//!
//! This derives an addr20 payout address **exactly** the way the Compute
//! Substrate (CSD) chain does, so an address created here is guaranteed to be
//! valid AND spendable on-chain:
//!
//! ```text
//!   sk      : 32-byte secp256k1 secret key (random)
//!   pk33    : compressed secp256k1 public key  = PublicKey::serialize()  (33 bytes)
//!   addr20  : RIPEMD160(SHA256(pk33))                                    (20 bytes)
//! ```
//!
//! Verified against the chain source (`compute-substrate`):
//!   * `src/crypto/mod.rs:28`  — `hash160(x) = RIPEMD160(SHA256(x))`
//!   * `src/cli/wallet.rs:106` — `pub_from_sk`: `addr20 = hash160(pk.serialize())`
//!   * `src/cli/wallet.rs:455` — `wallet_new`: same derivation the node prints
//!   * `src/cli/main.rs:619`   — `privkey_to_addr20_hex`: same derivation
//!   * `src/state/utxo.rs:441` — CONSENSUS spend check recomputes
//!                               `hash160(pubkey)` and requires it == the UTXO's
//!                               `script_pubkey`. So ONLY addresses derived this
//!                               way can ever be spent.
//!
//! The secret key is generated locally with the OS CSPRNG and is NEVER
//! transmitted anywhere. It is printed to the screen and written to
//! `./csd-wallet.txt`. Whoever holds the private key controls the coins —
//! back it up.

use std::fs::OpenOptions;
use std::io::Write as _;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use ripemd::Ripemd160;
use secp256k1::{PublicKey, Secp256k1, SecretKey};
use sha2::{Digest, Sha256};

/// A freshly generated CSD wallet: secret key, compressed public key, and the
/// derived 20-byte payout address.
pub struct Wallet {
    /// 32-byte secp256k1 secret key. THIS controls the coins — keep it secret.
    pub sk: [u8; 32],
    /// 33-byte compressed secp256k1 public key (`PublicKey::serialize()`).
    pub pk33: [u8; 33],
    /// 20-byte payout address = RIPEMD160(SHA256(pk33)).
    pub addr20: [u8; 20],
}

/// Derive the chain's `hash160` exactly: `RIPEMD160(SHA256(data))`.
///
/// Mirrors `compute-substrate/src/crypto/mod.rs:28-34` byte-for-byte
/// (`sha2::Sha256` then `ripemd::Ripemd160`).
fn hash160(data: &[u8]) -> [u8; 20] {
    let sha = Sha256::digest(data);
    let rip = Ripemd160::digest(sha);
    let mut out = [0u8; 20];
    out.copy_from_slice(&rip);
    out
}

/// Derive the compressed pubkey + addr20 from a 32-byte secret key, using the
/// same secp256k1 path as the node (`PublicKey::from_secret_key().serialize()`
/// → compressed 33 bytes → `hash160`).
///
/// Returns an error only if `sk32` is not a valid secp256k1 scalar (zero or
/// >= curve order), which never happens for keys produced by [`generate`].
pub fn wallet_from_secret(sk32: [u8; 32]) -> Result<Wallet> {
    let secp = Secp256k1::new();
    let sk = SecretKey::from_slice(&sk32).context("not a valid secp256k1 secret key")?;
    let pk = PublicKey::from_secret_key(&secp, &sk);
    let pk33 = pk.serialize(); // compressed (33 bytes), exactly as the chain uses
    let addr20 = hash160(&pk33);
    Ok(Wallet {
        sk: sk32,
        pk33,
        addr20,
    })
}

/// Generate a brand-new wallet using the operating system CSPRNG.
///
/// Uses `secp256k1`'s `rand`-backed key generation (`SecretKey::new(&mut
/// OsRng)`), the same constructor the node's `wallet new` uses
/// (`compute-substrate/src/cli/wallet.rs:457`).
pub fn generate() -> Result<Wallet> {
    use rand::rngs::OsRng;
    let secp = Secp256k1::new();
    let (sk, pk) = secp.generate_keypair(&mut OsRng);
    let pk33 = pk.serialize();
    let addr20 = hash160(&pk33);
    Ok(Wallet {
        sk: sk.secret_bytes(),
        pk33,
        addr20,
    })
}

/// addr20 as 40 lowercase hex chars (no `0x`), the canonical `--address` form.
pub fn addr20_hex(w: &Wallet) -> String {
    hex::encode(w.addr20)
}

/// Secret key as `0x`-prefixed 64-hex, the exact import format the node accepts
/// (`csd wallet recover --privkey 0x…`, `csd wallet init --privkey 0x…`, and the
/// `--privkey` flag on every `csd wallet` tx subcommand).
pub fn privkey_hex(w: &Wallet) -> String {
    format!("0x{}", hex::encode(w.sk))
}

/// Render the wallet file contents (also what we print to the screen, minus the
/// ANSI niceties). Kept separate so it is unit-testable.
fn wallet_file_contents(w: &Wallet) -> String {
    format!(
        "CSD pool-miner wallet\n\
         =====================\n\
         Generated locally by `csd-pool-miner newwallet`. The private key below\n\
         was created on THIS machine and was NOT sent anywhere.\n\
         \n\
         ############################################################\n\
         #  !!! BACK UP THE PRIVATE KEY BELOW — DO NOT LOSE IT !!!   #\n\
         #                                                          #\n\
         #  Anyone with this private key can spend your coins.      #\n\
         #  If you lose it, your mined coins are GONE FOREVER —     #\n\
         #  there is no recovery, no reset, no support line.        #\n\
         #                                                          #\n\
         #  - Store it offline (password manager / paper / USB).    #\n\
         #  - Never paste it into chat, email, or a website.        #\n\
         #  - The pool only ever needs the ADDRESS, never the key.  #\n\
         ############################################################\n\
         \n\
         ADDRESS (give this to the miner as --address; safe to share):\n\
         {addr}\n\
         \n\
         PRIVATE KEY (SECRET — back this up, never share):\n\
         {privkey}\n\
         \n\
         Public key (compressed, informational):\n\
         0x{pubkey}\n\
         \n\
         To mine to this wallet:\n\
         \n\
             csd-pool-miner --address {addr}\n\
         \n\
         To later import this key into the CSD node wallet and spend:\n\
         \n\
             csd wallet recover --privkey {privkey}\n\
         \n",
        addr = addr20_hex(w),
        privkey = privkey_hex(w),
        pubkey = hex::encode(w.pk33),
    )
}

/// Write the wallet to `path` with `0o600` perms where supported, refusing to
/// clobber an existing file (so a second run can't silently overwrite — and
/// destroy — a key the user may not have backed up yet).
fn write_wallet_file(path: &Path, w: &Wallet) -> Result<()> {
    let mut opts = OpenOptions::new();
    opts.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        opts.mode(0o600);
    }
    let mut f = opts.open(path).with_context(|| {
        format!(
            "could not create {} (does it already exist? refusing to overwrite an \
             existing wallet file — move it aside first)",
            path.display()
        )
    })?;
    f.write_all(wallet_file_contents(w).as_bytes())
        .with_context(|| format!("writing {}", path.display()))?;
    Ok(())
}

/// Entry point for the `newwallet` subcommand.
///
/// Generates a fresh wallet, writes it to `./csd-wallet.txt` (or
/// `csd-wallet-N.txt` if that exists), prints the address + key file location,
/// and reminds the user to back up the key. Never contacts the network.
pub fn run() -> Result<()> {
    let w = generate().context("generating keypair")?;

    // Pick a non-clobbering path: csd-wallet.txt, else csd-wallet-1.txt, …
    let path = pick_output_path("csd-wallet", "txt");
    write_wallet_file(&path, &w)?;

    let abs = std::fs::canonicalize(&path).unwrap_or_else(|_| path.clone());

    println!();
    println!("==============================================================");
    println!("  New CSD wallet created");
    println!("==============================================================");
    println!();
    println!("  Payout address (use this with the miner, safe to share):");
    println!();
    println!("      {}", addr20_hex(&w));
    println!();
    println!("  Private key (SECRET — back this up, NEVER share it):");
    println!();
    println!("      {}", privkey_hex(&w));
    println!();
    println!("  Saved to: {}", abs.display());
    println!();
    println!("  !!! BACK UP THE PRIVATE KEY. If you lose it, the coins this");
    println!("      address mines are GONE FOREVER. No recovery is possible.");
    println!("      The pool only needs the ADDRESS, never the private key.");
    println!();
    println!("  Start mining to it with:");
    println!("      csd-pool-miner --address {}", addr20_hex(&w));
    println!("==============================================================");
    println!();

    Ok(())
}

/// Return `<stem>.<ext>` if free, else `<stem>-1.<ext>`, `<stem>-2.<ext>`, …
/// so we never propose a path that already exists (the actual create is still
/// `create_new`, so this is purely to give a friendly default).
fn pick_output_path(stem: &str, ext: &str) -> PathBuf {
    let first = PathBuf::from(format!("{stem}.{ext}"));
    if !first.exists() {
        return first;
    }
    for n in 1u32.. {
        let p = PathBuf::from(format!("{stem}-{n}.{ext}"));
        if !p.exists() {
            return p;
        }
    }
    first // unreachable in practice
}

#[cfg(test)]
mod tests {
    use super::*;

    /// VERIFICATION OF DERIVATION CORRECTNESS.
    ///
    /// Each vector below was produced by the CSD chain's OWN binary:
    ///
    ///   target/release/csd wallet recover --privkey 0x<sk>
    ///
    /// (which runs `wallet_addr` → `pub_from_sk` → `crypto::hash160`, the exact
    /// consensus derivation in `compute-substrate/src/cli/wallet.rs:106` +
    /// `src/crypto/mod.rs:28`). Two of them are ALSO published, independent
    /// Bitcoin test vectors for HASH160(compressed pubkey), giving an external
    /// cross-check that the algorithm is standard secp256k1 + SHA256 + RIPEMD160:
    ///
    ///   * sk = 0x00..01 → pk = compressed generator point G
    ///     (0279be667ef9dcbbac55a06295ce870b07029bfcdb2dce28d959f2815b16f81798);
    ///     HASH160(G) = 751e76e8199196d454941c45d1b3a323f1433bd6  (the well-known
    ///     "address from G" vector, base58 1BgGZ9tcN4rm9KBzDn7KprQz87SZ26SAMH).
    ///
    ///   * sk = 0x18e14a7b…321725 → pk = 0250863ad6…5b2352; HASH160 =
    ///     f54a5851e9372b87810a8e60cdd2e7cfd80b6e31 — the canonical worked
    ///     example from the Bitcoin wiki "Technical background of v1 addresses".
    ///
    /// If this test passes, addresses from `newwallet` are byte-identical to
    /// what the CSD node would derive, hence valid and spendable on-chain.
    fn check_vector(sk_hex: &str, want_pk33_hex: &str, want_addr20_hex: &str) {
        let sk_bytes = hex::decode(sk_hex.trim_start_matches("0x")).unwrap();
        let mut sk = [0u8; 32];
        sk.copy_from_slice(&sk_bytes);

        let w = wallet_from_secret(sk).expect("valid secret key");

        assert_eq!(
            hex::encode(w.pk33),
            want_pk33_hex,
            "compressed pubkey mismatch for sk={sk_hex}"
        );
        assert_eq!(
            addr20_hex(&w),
            want_addr20_hex,
            "addr20 mismatch for sk={sk_hex} — derivation does NOT match the chain!"
        );
        // addr20 must be exactly 40 lowercase hex chars (the pool's --address form).
        assert_eq!(addr20_hex(&w).len(), 40);
        assert!(addr20_hex(&w)
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b)));
    }

    #[test]
    fn addr20_matches_chain_vector_bitcoin_wiki() {
        // Chain-verified AND published Bitcoin-wiki worked example.
        check_vector(
            "0x18e14a7b6a307f426a94f8114701e7c8e774e7f9a47e2c2035db29a206321725",
            "0250863ad64a87ae8a2fe83c1af1a8403cb53f53e486d8511dad8a04887e5b2352",
            "f54a5851e9372b87810a8e60cdd2e7cfd80b6e31",
        );
    }

    #[test]
    fn addr20_matches_chain_vector_generator_point() {
        // sk = 1 → pubkey is the secp256k1 generator G (compressed).
        check_vector(
            "0x0000000000000000000000000000000000000000000000000000000000000001",
            "0279be667ef9dcbbac55a06295ce870b07029bfcdb2dce28d959f2815b16f81798",
            "751e76e8199196d454941c45d1b3a323f1433bd6",
        );
    }

    #[test]
    fn addr20_matches_chain_vector_all_0x11() {
        // sk = 0x11 repeated; produced by the csd binary in this same session.
        check_vector(
            "0x1111111111111111111111111111111111111111111111111111111111111111",
            "034f355bdcb7cc0af728ef3cceb9615d90684bb5b2ca5f859ab0f0b704075871aa",
            "fc7250a211deddc70ee5a2738de5f07817351cef",
        );
    }

    #[test]
    fn generated_wallet_is_self_consistent() {
        // A freshly generated key must re-derive to the same addr20 when fed
        // back through wallet_from_secret (i.e. generate() and the pure
        // derivation path agree), and produce a well-formed 40-hex address.
        let w = generate().unwrap();
        let re = wallet_from_secret(w.sk).unwrap();
        assert_eq!(w.pk33, re.pk33);
        assert_eq!(w.addr20, re.addr20);
        assert_eq!(addr20_hex(&w).len(), 40);
        // Compressed pubkey always starts with 0x02 or 0x03.
        assert!(w.pk33[0] == 0x02 || w.pk33[0] == 0x03);
    }

    #[test]
    fn wallet_file_contains_address_and_key_and_warning() {
        let w = wallet_from_secret([0x11u8; 32]).unwrap();
        let body = wallet_file_contents(&w);
        assert!(body.contains(&addr20_hex(&w)));
        assert!(body.contains(&privkey_hex(&w)));
        assert!(body.to_uppercase().contains("BACK UP"));
        assert!(body.contains("GONE FOREVER"));
    }
}
