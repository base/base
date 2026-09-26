//! Pure secp256k1 actor authentication.
//!
//! With the Keystore removed, every EIP-8130 identity is a full-authority
//! secp256k1 owner. Authentication is therefore a signature recovery with no
//! account-config storage read, no scope/expiry/revocation checks, and no
//! non-k1 authenticators. This type provides the two wire forms the sender and
//! payer paths use:
//!
//! - a **bare** 65-byte `r || s || v` signature (the empty-`sender` EOA path and
//!   open payer mode), and
//! - a **named** `K1_AUTHENTICATOR(20) || r || s || v(65)` blob (a configured
//!   sender and an explicit/bound payer), which must carry the canonical native
//!   secp256k1 authenticator selector.

use alloy_primitives::{Address, B256};
use base_common_consensus::Eip8130Constants;

use crate::{AuthError, RecoveredActorId};

/// Recovers the secp256k1 signer of an EIP-8130 authentication blob.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub struct ActorAuthorizer;

impl ActorAuthorizer {
    /// Recovers the signer of a **named** `K1_AUTHENTICATOR(20) || r||s||v(65)`
    /// blob over `hash`.
    ///
    /// The 20-byte selector must be the canonical native secp256k1 sentinel
    /// ([`Eip8130Constants::K1_AUTHENTICATOR`]); any other selector is rejected
    /// as non-canonical, since it is a removed Keystore authenticator that no
    /// account can hold.
    pub fn recover_named_k1(auth: &[u8], hash: B256) -> Result<Address, AuthError> {
        if auth.len() < 20 {
            return Err(AuthError::MalformedAuth);
        }
        let authenticator = Address::from_slice(&auth[..20]);
        if authenticator != Eip8130Constants::K1_AUTHENTICATOR {
            return Err(AuthError::NotCanonical(authenticator));
        }
        Ok(RecoveredActorId::recover_k1(hash, &auth[20..])?.address())
    }

    /// Recovers the signer of a **bare** 65-byte `r||s||v` signature over `hash`
    /// (the EOA sender path and open payer mode).
    pub fn recover_bare_k1(auth: &[u8], hash: B256) -> Result<Address, AuthError> {
        Ok(RecoveredActorId::recover_k1(hash, auth)?.address())
    }
}

#[cfg(test)]
mod tests {
    use alloy_primitives::keccak256;
    use k256::ecdsa::SigningKey as K256SigningKey;

    use super::*;

    const HASH: B256 = B256::repeat_byte(0x42);

    fn key(byte: u8) -> K256SigningKey {
        K256SigningKey::from_slice(&[byte; 32]).unwrap()
    }

    fn addr(key: &K256SigningKey) -> Address {
        let point = key.verifying_key().to_encoded_point(false);
        Address::from_slice(&keccak256(&point.as_bytes()[1..])[12..])
    }

    fn sig(key: &K256SigningKey, hash: B256) -> [u8; 65] {
        let (signature, recid) = key.sign_prehash_recoverable(hash.as_slice()).unwrap();
        let mut out = [0u8; 65];
        out[..64].copy_from_slice(&signature.to_bytes());
        out[64] = recid.to_byte() + 27;
        out
    }

    fn named(authenticator: Address, data: &[u8]) -> Vec<u8> {
        let mut out = Vec::with_capacity(20 + data.len());
        out.extend_from_slice(authenticator.as_slice());
        out.extend_from_slice(data);
        out
    }

    #[test]
    fn named_k1_recovers_the_signer() {
        let k = key(0x11);
        let blob = named(Eip8130Constants::K1_AUTHENTICATOR, &sig(&k, HASH));
        assert_eq!(ActorAuthorizer::recover_named_k1(&blob, HASH), Ok(addr(&k)));
    }

    #[test]
    fn bare_k1_recovers_the_signer() {
        let k = key(0x22);
        assert_eq!(ActorAuthorizer::recover_bare_k1(&sig(&k, HASH), HASH), Ok(addr(&k)));
    }

    #[test]
    fn named_non_canonical_selector_is_rejected() {
        let k = key(0x33);
        let bogus = Address::repeat_byte(0x99);
        let blob = named(bogus, &sig(&k, HASH));
        assert_eq!(
            ActorAuthorizer::recover_named_k1(&blob, HASH),
            Err(AuthError::NotCanonical(bogus))
        );
    }

    #[test]
    fn named_blob_shorter_than_selector_is_malformed() {
        assert_eq!(
            ActorAuthorizer::recover_named_k1(&[0u8; 10], HASH),
            Err(AuthError::MalformedAuth)
        );
    }

    #[test]
    fn bare_wrong_length_is_malformed() {
        assert_eq!(
            ActorAuthorizer::recover_bare_k1(&[0u8; 64], HASH),
            Err(AuthError::MalformedAuth)
        );
    }
}
