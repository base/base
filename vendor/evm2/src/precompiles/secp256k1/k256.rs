//! k256 implementation of `ecrecover`. More about it in [`crate::precompiles::secp256k1`].

#![allow(dead_code)]

use alloy_primitives::{B256, B512, keccak256};
use k256_0_14_0::ecdsa::{Error, RecoveryId, Signature, VerifyingKey};

/// Recover the public key from a signature and a message.
///
/// This function is using the `k256` crate.
pub(crate) fn ecrecover(sig: &B512, mut recid: u8, msg: &B256) -> Result<B256, Error> {
    // parse signature
    let sig = Signature::from_slice(sig.as_slice())?;

    // normalize signature and flip recovery id if needed.
    let sig_normalized = sig.normalize_s();
    if sig != sig_normalized {
        recid ^= 1;
    }
    let recid = RecoveryId::from_byte(recid).expect("recovery ID is valid");

    // recover key
    let recovered_key = VerifyingKey::recover_from_prehash(&msg[..], &sig_normalized, recid)?;
    // hash it
    let mut hash = keccak256(&recovered_key.to_sec1_point(/* compress = */ false).as_bytes()[1..]);

    // truncate to 20 bytes
    hash[..12].fill(0);
    Ok(hash)
}
