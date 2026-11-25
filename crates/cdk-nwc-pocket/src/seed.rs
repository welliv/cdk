//! Deterministic pocket seed derivation.

use bitcoin::hashes::{hmac, sha512, Hash, HashEngine};

/// Domain separation tag for pocket seeds.
const POCKET_SEED_TAG: &[u8] = b"cdk-nwc-pocket-v1";

/// Derive the 64-byte seed of pocket `index` from the main wallet seed.
///
/// `seed = HMAC-SHA512(key = main_seed, data = "cdk-nwc-pocket-v1" || index_be)`.
///
/// The main wallet can always recompute every pocket seed, so it can sweep or
/// restore (NUT-13) a pocket even if the pocket database is lost. The pocket
/// seed does not reveal the main seed, so a process that only holds the
/// pocket cannot reach the main wallet's proofs.
pub fn derive_pocket_seed(main_seed: &[u8; 64], index: u32) -> [u8; 64] {
    let mut engine = hmac::HmacEngine::<sha512::Hash>::new(main_seed);
    engine.input(POCKET_SEED_TAG);
    engine.input(&index.to_be_bytes());
    hmac::Hmac::<sha512::Hash>::from_engine(engine).to_byte_array()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pocket_seeds_are_deterministic_and_distinct() {
        let main = [7u8; 64];
        assert_eq!(derive_pocket_seed(&main, 0), derive_pocket_seed(&main, 0));
        assert_ne!(derive_pocket_seed(&main, 0), derive_pocket_seed(&main, 1));
        assert_ne!(derive_pocket_seed(&main, 0), main);
        assert_ne!(
            derive_pocket_seed(&main, 0),
            derive_pocket_seed(&[8u8; 64], 0)
        );
    }
}
