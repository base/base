//! Proof-of-recovery actor id: a recovered secp256k1 signer that can only be
//! produced by a verified signature recovery.

use alloy_primitives::{Address, B256, keccak256};
use base_common_consensus::Eip8130Signed;
use k256::ecdsa::{RecoveryId, Signature as K256Signature, VerifyingKey as K256VerifyingKey};

use crate::AuthError;

/// A recovered secp256k1 signer, carried as its address and resolved `actorId`
/// (`bytes32(bytes20(address))`).
///
/// The wrapped address is private and every constructor performs a real
/// signature recovery, so a value is *evidence* that the signer authenticated
/// over the relevant hash. [`ActorAuthorizer::authorize_k1`] consumes this
/// token instead of a bare `B256`, lifting its "the caller must have recovered
/// the signer first" precondition into the type system: a caller cannot
/// fabricate an arbitrary recovered signer and obtain owner access on an
/// account, because the only ways to obtain a `RecoveredActorId` are the
/// recovery constructors below.
///
/// [`ActorAuthorizer::authorize_k1`]: crate::ActorAuthorizer::authorize_k1
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RecoveredActorId {
    address: Address,
}

impl RecoveredActorId {
    /// Recovers the signer of `hash` from a 65-byte `r || s || v` secp256k1
    /// signature — the native `K1_AUTHENTICATOR` wire form — requiring
    /// `v in {27, 28}` and enforcing **EIP-2 low-`s`** (malleable upper-half-`s`
    /// signatures are rejected, not canonicalized). This is the recovery the
    /// enshrined k1 authenticator performs; [`AuthenticatorDispatch`] routes its
    /// k1 path through here, so the two stay byte-parity with the deployed
    /// `AccountConfiguration` reference by construction.
    ///
    /// [`AuthenticatorDispatch`]: crate::AuthenticatorDispatch
    pub fn recover_k1(hash: B256, signature: &[u8]) -> Result<Self, AuthError> {
        if signature.len() != 65 {
            return Err(AuthError::MalformedAuth);
        }
        let recovery = match signature[64] {
            27 | 28 => signature[64] - 27,
            _ => return Err(AuthError::InvalidSignature),
        };
        let sig =
            K256Signature::from_slice(&signature[..64]).map_err(|_| AuthError::InvalidSignature)?;
        // `normalize_s` returns `Some` only when `s` is in the upper half, i.e. a
        // malleable high-`s` signature: reject it rather than canonicalizing.
        if sig.normalize_s().is_some() {
            return Err(AuthError::InvalidSignature);
        }
        let recovery_id = RecoveryId::from_byte(recovery).ok_or(AuthError::InvalidSignature)?;
        let key = K256VerifyingKey::recover_from_prehash(hash.as_slice(), &sig, recovery_id)
            .map_err(|_| AuthError::InvalidSignature)?;
        let encoded = key.to_encoded_point(false);
        // encoded = 0x04 || x(32) || y(32); address = keccak256(x || y)[12..].
        let address = Address::from_slice(&keccak256(&encoded.as_bytes()[1..])[12..]);
        Ok(Self { address })
    }

    /// Recovers the EOA sender of a signed transaction — the empty-`sender` wire
    /// path — via the checked (EIP-2 low-`s`) recovery over its sender signing
    /// hash. Returns `Ok(None)` on the configured-actor path
    /// (`tx.sender == Some`), where there is no EOA signature to recover, and
    /// `Err` when the empty-`sender` payload is malformed or the signature is
    /// invalid.
    pub fn recover_eoa_sender(signed: &Eip8130Signed) -> Result<Option<Self>, AuthError> {
        Ok(signed
            .recover_eoa_sender()
            .map_err(|_| AuthError::InvalidSignature)?
            .map(|address| Self { address }))
    }

    /// The recovered signer address.
    #[must_use]
    pub const fn address(self) -> Address {
        self.address
    }

    /// The recovered signer's actor id, `bytes32(bytes20(address))` — the 20
    /// address bytes left-aligned, right-padded with zeros.
    #[must_use]
    pub fn actor_id(self) -> B256 {
        let mut id = [0u8; 32];
        id[..20].copy_from_slice(self.address.as_slice());
        B256::from(id)
    }
}
