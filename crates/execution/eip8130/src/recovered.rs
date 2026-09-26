//! Proof-of-recovery actor id: a recovered secp256k1 signer that can only be
//! produced by a verified signature recovery.

use alloy_primitives::{Address, B256};
use base_common_consensus::Eip8130Signed;

use crate::AuthError;

/// A recovered secp256k1 signer, carried as its address and resolved `actorId`
/// (`bytes32(uint256(uint160(address)))`, address right-aligned).
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
    /// signatures are rejected, not canonicalized). Recovery goes through
    /// [`Eip8130Signed::recover_raw_k1`], the same checked path open-payer
    /// resolution uses. [`AuthenticatorDispatch`] routes its k1 path through
    /// here.
    ///
    /// [`AuthenticatorDispatch`]: crate::AuthenticatorDispatch
    pub fn recover_k1(hash: B256, signature: &[u8]) -> Result<Self, AuthError> {
        if signature.len() != 65 {
            return Err(AuthError::MalformedAuth);
        }
        let address = Eip8130Signed::recover_raw_k1(hash, signature)
            .map_err(|_| AuthError::InvalidSignature)?;
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

    /// The recovered signer's actor id, `bytes32(uint256(uint160(address)))` —
    /// the 20 address bytes right-aligned in the low bytes, high 12 bytes zero
    /// (matches the finalized `Keystore.ActorId.fromAddress`).
    #[must_use]
    pub fn actor_id(self) -> B256 {
        let mut id = [0u8; 32];
        id[12..].copy_from_slice(self.address.as_slice());
        B256::from(id)
    }
}
