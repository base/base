//! A process-wide memo of the `ecrecover` precompile's signature recoveries.
//!
//! The prewarm workers execute a block's transactions speculatively ahead of its validation, so
//! the recoveries the validating thread needs (about 25 µs each) were mostly computed moments
//! earlier on another thread. [`install_ecrecover_cache`] installs an evm2 [`Crypto`] provider
//! that keeps them: a recovery is a pure function of its inputs, so a remembered result is the
//! one the computation would give. Gas is charged by the precompile before the recovery, so it is
//! unchanged.

use std::{
    collections::HashMap,
    sync::{Mutex, PoisonError},
};

use evm2::{Crypto, PrecompileHalt, install_crypto, precompiles::DefaultCrypto};

const SHARDS: usize = 16;
/// Recoveries kept per shard; a full shard starts over.
const SHARD_ENTRIES: usize = 4096;

/// The message (32 bytes), the signature (64) and the recovery id.
type Key = [u8; 97];

#[derive(Debug)]
struct EcrecoverCache {
    shards: [Mutex<HashMap<Key, Option<[u8; 32]>>>; SHARDS],
}

impl Crypto for EcrecoverCache {
    fn secp256k1_ecrecover(
        &self,
        sig: &[u8; 64],
        recid: u8,
        msg: &[u8; 32],
    ) -> Result<[u8; 32], PrecompileHalt> {
        let mut key = [0; 97];
        key[..32].copy_from_slice(msg);
        key[32..96].copy_from_slice(sig);
        key[96] = recid;
        // The signature's last byte is uniformly distributed.
        let shard = &self.shards[usize::from(sig[63]) % SHARDS];
        let cached = shard.lock().unwrap_or_else(PoisonError::into_inner).get(&key).copied();
        let recovered = match cached {
            Some(recovered) => recovered,
            None => {
                let recovered = DefaultCrypto::new().secp256k1_ecrecover(sig, recid, msg).ok();
                let mut shard = shard.lock().unwrap_or_else(PoisonError::into_inner);
                if shard.len() >= SHARD_ENTRIES {
                    shard.clear();
                }
                shard.insert(key, recovered);
                recovered
            }
        };
        recovered.ok_or(PrecompileHalt::Secp256k1RecoverFailed)
    }
}

/// Installs the memo as evm2's crypto provider. Call it before any EVM runs; returns whether it
/// was installed (another provider may already be).
pub fn install_ecrecover_cache() -> bool {
    install_crypto(EcrecoverCache { shards: std::array::from_fn(|_| Mutex::default()) })
}

#[cfg(test)]
mod tests {
    use k256::ecdsa::SigningKey;

    use super::*;

    /// Remembered recoveries are keyed by the whole input: the same signature over another
    /// message, or with the other recovery id, recovers what an uncached recovery does.
    #[test]
    fn remembered_recoveries_match_the_computed_ones() {
        let cache = EcrecoverCache { shards: std::array::from_fn(|_| Mutex::default()) };
        let key = SigningKey::from_slice(&[7; 32]).unwrap();
        let msg = [0x11; 32];
        let (signature, recid) = key.sign_prehash_recoverable(&msg).unwrap();
        let sig: [u8; 64] = signature.to_bytes().into();
        let other_msg = [0x22; 32];
        let inputs = [(recid.to_byte(), msg), (recid.to_byte() ^ 1, msg), (recid.to_byte(), other_msg)];
        for _ in 0..2 {
            for (recid, msg) in &inputs {
                assert_eq!(
                    cache.secp256k1_ecrecover(&sig, *recid, msg),
                    DefaultCrypto::new().secp256k1_ecrecover(&sig, *recid, msg),
                );
            }
        }
        // The signer, not just some address, is recovered.
        let point = key.verifying_key().to_encoded_point(false);
        let signer = alloy_primitives::keccak256(&point.as_bytes()[1..]);
        let mut expected = [0; 32];
        expected[12..].copy_from_slice(&signer[12..]);
        assert_eq!(cache.secp256k1_ecrecover(&sig, recid.to_byte(), &msg), Ok(expected));
        // An invalid signature fails every time.
        assert!(cache.secp256k1_ecrecover(&[0; 64], 0, &msg).is_err());
        assert!(cache.secp256k1_ecrecover(&[0; 64], 0, &msg).is_err());
    }
}
