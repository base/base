//! Canonical off-hot-path signature verification: the native port of
//! `Keystore.validateSignature` / `envelopeDigest` / `replaySafeHash`.
//!
//! This is the signature-checking method that supersedes ERC-1271 for EIP-8130
//! accounts: given an account, an app digest, and a typed signature envelope, it
//! resolves and authenticates the signing actor and returns its identity and
//! scope so a consumer can decide authorization without re-deriving the actor.
//!
//! The transaction hot path ([`crate::ActorTxVerifier`]) does not use this — it
//! binds each transaction digest in-struct and calls
//! [`ActorAuthorizer::authenticate_actor`] directly. This surface exists for
//! callers that want the contract-parity envelope semantics (an off-8130
//! consumer reaching parity with the native hot path, or a general app digest
//! check) and is intentionally not wired into any execution flow today.

use alloy_primitives::{Address, B256, b256, keccak256};

use crate::{AccountConfigurationStorage, ActorAuthorizer, AuthorizeError, ResolvedActor};

/// Precomputed `keccak256` typehash of the replay-safe signed-message struct,
/// matching `Keystore.SIGNED_MESSAGE_TYPEHASH`:
/// `keccak256("EIP8130SignedMessage(address account,uint256 chainId,bytes32 hash)")`.
const SIGNED_MESSAGE_TYPEHASH: B256 =
    b256!("9d2bc80c29f8a3962243919d898fa1b566a99dc64dd59734f6a10e20be3a7e04");

/// The replay channel a typed signature envelope binds to. Mirrors
/// `Keystore.SignatureType`: the leading envelope byte is `Local` (`0x01`,
/// binds a specific chain id) or `Multichain` (`0x02`, binds `chainId == 0` and
/// is therefore replayable on every chain). `Invalid` (`0x00`) and any
/// out-of-range byte are rejected by [`SignatureVerifier::validate_signature`].
///
/// This is distinct from [`AccountChangeChannel`], which gates the
/// `SignedAccountChanges` batch and uses different byte values
/// (`Local = 0x00`, `Multichain = 0x01`).
///
/// [`AccountChangeChannel`]: base_common_consensus::AccountChangeChannel
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum SignatureType {
    /// Binds the caller-supplied chain id (single-chain).
    Local = 0x01,
    /// Binds `chainId == 0`; the signature is replayable on every chain.
    Multichain = 0x02,
}

impl SignatureType {
    /// Resolves the leading envelope byte to a [`SignatureType`], returning
    /// `None` for `Invalid` (`0x00`) and any out-of-range value. Mirrors the
    /// `sigTypeByte > uint8(type(SignatureType).max)` guard, extended to reject
    /// the reserved `Invalid` value the way `envelopeDigest` does.
    #[must_use]
    pub const fn from_byte(byte: u8) -> Option<Self> {
        match byte {
            0x01 => Some(Self::Local),
            0x02 => Some(Self::Multichain),
            _ => None,
        }
    }

    /// The chain id this envelope type binds, given the local chain id. `Local`
    /// binds `local_chain_id`; `Multichain` binds `0`.
    #[must_use]
    pub const fn bound_chain_id(self, local_chain_id: u64) -> u64 {
        match self {
            Self::Local => local_chain_id,
            Self::Multichain => 0,
        }
    }
}

/// Why a typed signature envelope failed to validate. Mirrors the revert
/// reasons of `Keystore.validateSignature`.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum SignatureError {
    /// The envelope was empty (missing its leading `sigType` byte). Mirrors
    /// `EmptySignatureEnvelope`.
    #[error("signature envelope is empty (missing its leading sigType byte)")]
    EmptySignatureEnvelope,

    /// The leading `sigType` byte is not a recognized [`SignatureType`] (it was
    /// `Invalid` or out of range). Mirrors `UnknownSignatureType`.
    #[error("unknown signature type byte {0:#04x}")]
    UnknownSignatureType(u8),

    /// The actor could not be authenticated against the envelope digest (bad
    /// authenticator length, authentication failure, mismatch, expiry, revoked
    /// default EOA, or a storage read). Mirrors the `authenticateActor` reverts.
    #[error("actor authentication failed: {0}")]
    Authenticate(#[from] AuthorizeError),
}

/// Contract-parity signature verification over an app digest.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub struct SignatureVerifier;

impl SignatureVerifier {
    /// The replay-safe digest binding `hash` to `account` and `chain_id`,
    /// matching `Keystore.replaySafeHash`:
    /// `keccak256(abi.encode(SIGNED_MESSAGE_TYPEHASH, account, chainId, hash))`.
    #[must_use]
    pub fn replay_safe_hash(account: Address, chain_id: u64, hash: B256) -> B256 {
        // abi.encode(bytes32, address, uint256, bytes32): four right-aligned words.
        let mut buf = [0u8; 128];
        buf[..32].copy_from_slice(SIGNED_MESSAGE_TYPEHASH.as_slice());
        buf[44..64].copy_from_slice(account.as_slice());
        buf[88..96].copy_from_slice(&chain_id.to_be_bytes());
        buf[96..128].copy_from_slice(hash.as_slice());
        keccak256(buf)
    }

    /// The digest a signer must sign for `hash` to be accepted for `account`
    /// under `sig_type`, matching `Keystore.envelopeDigest`. `local_chain_id`
    /// supplies the chain id bound by [`SignatureType::Local`]; `Multichain`
    /// binds `0`.
    #[must_use]
    pub fn envelope_digest(
        sig_type: SignatureType,
        account: Address,
        hash: B256,
        local_chain_id: u64,
    ) -> B256 {
        Self::replay_safe_hash(account, sig_type.bound_chain_id(local_chain_id), hash)
    }

    /// Validates a typed-envelope signature over app digest `hash` for
    /// `account`, returning the authenticated actor. Native mirror of
    /// `Keystore.validateSignature`.
    ///
    /// `auth` is `sigType(1) || authenticator(20) || authenticator-specific
    /// data`. The leading byte selects the [`SignatureType`], the envelope
    /// digest is resolved against `account`/`local_chain_id`, and the remainder
    /// is authenticated via [`ActorAuthorizer::authenticate_actor`]. `now` is
    /// the timestamp used for actor expiry.
    ///
    /// Operational gating (which scopes may sign) is left to the caller, which
    /// reads the returned [`ResolvedActor::scope`].
    pub fn validate_signature(
        storage: &AccountConfigurationStorage<'_>,
        account: Address,
        hash: B256,
        auth: &[u8],
        local_chain_id: u64,
        now: u64,
    ) -> Result<ResolvedActor, SignatureError> {
        let (&sig_type_byte, inner) = auth.split_first().ok_or(SignatureError::EmptySignatureEnvelope)?;
        let sig_type = SignatureType::from_byte(sig_type_byte)
            .ok_or(SignatureError::UnknownSignatureType(sig_type_byte))?;
        let digest = Self::envelope_digest(sig_type, account, hash, local_chain_id);
        Ok(ActorAuthorizer::authenticate_actor(storage, account, digest, inner, now)?)
    }
}

#[cfg(test)]
mod tests {
    use alloy_primitives::{Bytes, U256, address, keccak256};
    use base_common_consensus::Eip8130Constants;
    use base_precompile_storage::{Handler, HashMapStorageProvider, StorageCtx};
    use k256::ecdsa::SigningKey as K256SigningKey;

    use super::*;

    const NOW: u64 = 1_000;
    const CHAIN_ID: u64 = 8453;
    const K1: Address = Eip8130Constants::K1_AUTHENTICATOR;
    const ACCOUNT: Address = address!("0x00000000000000000000000000000000000000aa");
    const HASH: B256 = B256::repeat_byte(0x42);

    #[test]
    fn typehash_matches_string() {
        assert_eq!(
            SIGNED_MESSAGE_TYPEHASH,
            keccak256(b"EIP8130SignedMessage(address account,uint256 chainId,bytes32 hash)")
        );
    }

    #[test]
    fn signature_type_byte_roundtrip() {
        assert_eq!(SignatureType::from_byte(0x00), None);
        assert_eq!(SignatureType::from_byte(0x01), Some(SignatureType::Local));
        assert_eq!(SignatureType::from_byte(0x02), Some(SignatureType::Multichain));
        assert_eq!(SignatureType::from_byte(0x03), None);
        assert_eq!(SignatureType::Local as u8, 0x01);
        assert_eq!(SignatureType::Multichain as u8, 0x02);
    }

    #[test]
    fn multichain_binds_chain_id_zero() {
        assert_eq!(SignatureType::Multichain.bound_chain_id(CHAIN_ID), 0);
        assert_eq!(SignatureType::Local.bound_chain_id(CHAIN_ID), CHAIN_ID);
        assert_eq!(
            SignatureVerifier::envelope_digest(SignatureType::Multichain, ACCOUNT, HASH, CHAIN_ID),
            SignatureVerifier::replay_safe_hash(ACCOUNT, 0, HASH),
        );
    }

    fn key(byte: u8) -> K256SigningKey {
        K256SigningKey::from_slice(&[byte; 32]).unwrap()
    }

    fn addr(key: &K256SigningKey) -> Address {
        let point = key.verifying_key().to_encoded_point(false);
        Address::from_slice(&keccak256(&point.as_bytes()[1..])[12..])
    }

    /// 65-byte `r || s || v` signature over `hash`, `v` in `{27, 28}`, low-s.
    fn sig(key: &K256SigningKey, hash: B256) -> Vec<u8> {
        let (signature, recid) = key.sign_prehash_recoverable(hash.as_slice()).unwrap();
        let mut out = vec![0u8; 65];
        out[..64].copy_from_slice(&signature.to_bytes());
        out[64] = recid.to_byte() + 27;
        out
    }

    /// `sigType(1) || authenticator(20) || data`.
    fn envelope(sig_type: u8, authenticator: Address, data: &[u8]) -> Bytes {
        let mut out = Vec::with_capacity(1 + 20 + data.len());
        out.push(sig_type);
        out.extend_from_slice(authenticator.as_slice());
        out.extend_from_slice(data);
        Bytes::from(out)
    }

    /// Canonical Solidity packing of `ActorConfig` (authenticator 0..160, expiry
    /// 160..208, scope 208..224).
    fn pack(authenticator: Address, scope: u16, expiry: u64) -> U256 {
        U256::from_be_slice(authenticator.as_slice())
            | (U256::from(expiry) << 160)
            | (U256::from(scope) << 208)
    }

    fn with_storage<R>(body: impl FnOnce(&mut AccountConfigurationStorage<'_>) -> R) -> R {
        let mut storage = HashMapStorageProvider::new(1);
        StorageCtx::enter(&mut storage, |ctx| body(&mut AccountConfigurationStorage::new(ctx)))
    }

    #[test]
    fn empty_envelope_is_rejected() {
        with_storage(|acc| {
            assert_eq!(
                SignatureVerifier::validate_signature(acc, ACCOUNT, HASH, &[], CHAIN_ID, NOW),
                Err(SignatureError::EmptySignatureEnvelope),
            );
        });
    }

    #[test]
    fn invalid_and_out_of_range_sig_type_is_rejected() {
        with_storage(|acc| {
            for byte in [0x00u8, 0x03, 0xff] {
                let auth = envelope(byte, K1, &[]);
                assert_eq!(
                    SignatureVerifier::validate_signature(acc, ACCOUNT, HASH, &auth, CHAIN_ID, NOW,),
                    Err(SignatureError::UnknownSignatureType(byte)),
                );
            }
        });
    }

    #[test]
    fn local_envelope_authenticates_bound_actor() {
        let k = key(0x22);
        let id = AccountConfigurationStorage::self_actor_id(addr(&k));
        let scope = Eip8130Constants::SCOPE_SENDER;
        let digest =
            SignatureVerifier::envelope_digest(SignatureType::Local, ACCOUNT, HASH, CHAIN_ID);
        let auth = envelope(SignatureType::Local as u8, K1, &sig(&k, digest));
        with_storage(|acc| {
            acc.actor_config.at_mut(&id).at_mut(&ACCOUNT).write(pack(K1, scope, 0)).unwrap();
            let resolved =
                SignatureVerifier::validate_signature(acc, ACCOUNT, HASH, &auth, CHAIN_ID, NOW)
                    .unwrap();
            assert_eq!(resolved.scope, scope);
            assert_eq!(resolved.actor_id, id);
        });
    }

    #[test]
    fn multichain_signature_is_chain_independent() {
        // A Multichain envelope binds chainId == 0, so a signature produced for
        // one local chain authenticates unchanged under a different local chain.
        let k = key(0x33);
        let id = AccountConfigurationStorage::self_actor_id(addr(&k));
        let digest =
            SignatureVerifier::envelope_digest(SignatureType::Multichain, ACCOUNT, HASH, CHAIN_ID);
        let auth = envelope(SignatureType::Multichain as u8, K1, &sig(&k, digest));
        with_storage(|acc| {
            acc.actor_config.at_mut(&id).at_mut(&ACCOUNT).write(pack(K1, 0, 0)).unwrap();
            let other_chain = CHAIN_ID + 1;
            let resolved =
                SignatureVerifier::validate_signature(acc, ACCOUNT, HASH, &auth, other_chain, NOW)
                    .unwrap();
            assert_eq!(resolved.actor_id, id);
            assert!(resolved.is_admin());
        });
    }

    #[test]
    fn local_signature_bound_to_other_chain_does_not_authenticate() {
        // A Local envelope signed for CHAIN_ID must not validate under a
        // different local chain: the digest binds the chain id.
        let k = key(0x44);
        let id = AccountConfigurationStorage::self_actor_id(addr(&k));
        let digest =
            SignatureVerifier::envelope_digest(SignatureType::Local, ACCOUNT, HASH, CHAIN_ID);
        let auth = envelope(SignatureType::Local as u8, K1, &sig(&k, digest));
        with_storage(|acc| {
            acc.actor_config.at_mut(&id).at_mut(&ACCOUNT).write(pack(K1, 0, 0)).unwrap();
            // Recovers a different actor id than the one bound: AuthenticatorMismatch.
            assert!(matches!(
                SignatureVerifier::validate_signature(acc, ACCOUNT, HASH, &auth, CHAIN_ID + 1, NOW,),
                Err(SignatureError::Authenticate(AuthorizeError::AuthenticatorMismatch { .. })),
            ));
        });
    }
}
