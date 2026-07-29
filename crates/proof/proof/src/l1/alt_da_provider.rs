//! Contains an oracle-backed [`AltDaCommitmentResolver`] for the client program.

use alloc::{boxed::Box, string::ToString, sync::Arc};

use alloy_primitives::{Bytes, keccak256};
use async_trait::async_trait;
use base_consensus_derive::{AltDaCommitmentResolver, AltDaResolverError};
use base_proof_preimage::{CommsClient, PreimageKey, PreimageKeyType, errors::PreimageOracleError};

use crate::HintType;

/// Keccak256 alt-DA commitment type byte: the commitment is `0x00 ++ keccak256(data)`.
const KECCAK256_COMMITMENT_TYPE: u8 = 0x00;
/// A keccak256 commitment is the type byte followed by a 32-byte content digest.
const KECCAK256_COMMITMENT_LEN: usize = 1 + 32;

/// Derives the preimage-oracle key under which a resolved alt-DA commitment's off-chain bytes are
/// stored, selecting the key *type* from the commitment *type*.
///
/// The commitment type dictates whether the resolved bytes are cryptographically verifiable, which
/// is exactly what a [`PreimageKeyType`] encodes:
///
/// - **Keccak256 commitment** (`0x00 ++ keccak256(data)`): the commitment *is* the content hash, so
///   the bytes are keyed under the embedded digest as a [`PreimageKeyType::Keccak256`] key. The
///   oracle re-hashes the served bytes and verifies them against the on-chain commitment.
/// - **Generic commitment** (anything else — e.g. `0x01 ++ 0xff ++ 32 random`): an opaque sentinel,
///   not a content hash, so the bytes cannot be verified. They are keyed under
///   `keccak256(commitment)` as a [`PreimageKeyType::GlobalGeneric`] key, which the oracle serves on
///   trust. This host trust is inherent to generic alt-DA and is the alt-DA layer's responsibility.
///
/// The host (which stores the bytes) and the guest (which reads them) MUST derive an identical key,
/// so both call this function — it is the single source of truth for alt-DA preimage keying.
pub fn preimage_key_for_commitment(commitment: &[u8]) -> PreimageKey {
    if commitment.len() == KECCAK256_COMMITMENT_LEN && commitment[0] == KECCAK256_COMMITMENT_TYPE {
        let mut digest = [0u8; 32];
        digest.copy_from_slice(&commitment[1..]);
        PreimageKey::new_keccak256(digest)
    } else {
        PreimageKey::new(*keccak256(commitment), PreimageKeyType::GlobalGeneric)
    }
}

/// An oracle-backed alt-DA commitment resolver.
///
/// Resolves an alt-DA commitment to its off-chain batch bytes by hinting the host (which fetches
/// the object from the da-server) and reading the bytes back from the preimage oracle. The oracle
/// key is derived by [`preimage_key_for_commitment`] — the same function the host uses to store
/// them — so its *type* follows the commitment type. Mirrors
/// [`OracleBlobProvider`](crate::OracleBlobProvider).
///
/// For generic commitments (random sentinels rather than content hashes) the guest cannot
/// cryptographically verify the resolved bytes against the commitment — it trusts the host-supplied
/// mapping. This is inherent to alt-DA generic commitments and acceptable for the TEE backend, where
/// data availability/correctness is the alt-DA layer's responsibility.
#[derive(Debug, Clone)]
pub struct OracleAltDaResolver<T: CommsClient> {
    oracle: Arc<T>,
}

impl<T: CommsClient> OracleAltDaResolver<T> {
    /// Constructs a new `OracleAltDaResolver`.
    pub const fn new(oracle: Arc<T>) -> Self {
        Self { oracle }
    }
}

#[async_trait]
impl<T: CommsClient + Send + Sync + core::fmt::Debug> AltDaCommitmentResolver
    for OracleAltDaResolver<T>
{
    async fn resolve(&self, commitment: &[u8]) -> Result<Bytes, AltDaResolverError> {
        // Hint the host to fetch the off-chain object for this commitment.
        HintType::AltDaCommitment
            .with_data(&[commitment])
            .send(self.oracle.as_ref())
            .await
            .map_err(|e| AltDaResolverError::Resolve(e.to_string()))?;

        // Read the bytes the host stored for this commitment under the same key (see
        // `preimage_key_for_commitment`). Objects are variable-length (up to MAX_DA_OBJECT_BYTES),
        // so use `get` rather than `get_exact`.
        let key = preimage_key_for_commitment(commitment);
        let bytes = self.oracle.get(key).await.map_err(|error| match error {
            PreimageOracleError::KeyNotFound => AltDaResolverError::NotFound,
            other => AltDaResolverError::Resolve(other.to_string()),
        })?;

        Ok(Bytes::from(bytes))
    }
}

#[cfg(test)]
mod tests {
    use alloc::{vec, vec::Vec};

    use base_proof_preimage::{
        HintWriterClient, PreimageKey, PreimageOracleClient,
        errors::{PreimageOracleError, PreimageOracleResult},
    };

    use super::*;

    /// Minimal oracle that serves a single preimage and accepts (ignores) hints.
    #[derive(Clone, Debug)]
    struct MockOracle {
        key: PreimageKey,
        value: Vec<u8>,
    }

    #[async_trait]
    impl PreimageOracleClient for MockOracle {
        async fn get(&self, key: PreimageKey) -> PreimageOracleResult<Vec<u8>> {
            if key == self.key {
                Ok(self.value.clone())
            } else {
                Err(PreimageOracleError::KeyNotFound)
            }
        }

        async fn get_exact(&self, key: PreimageKey, buf: &mut [u8]) -> PreimageOracleResult<()> {
            let value = self.get(key).await?;
            if value.len() != buf.len() {
                return Err(PreimageOracleError::BufferLengthMismatch(buf.len(), value.len()));
            }
            buf.copy_from_slice(&value);
            Ok(())
        }
    }

    #[async_trait]
    impl HintWriterClient for MockOracle {
        async fn write(&self, _hint: &str) -> PreimageOracleResult<()> {
            Ok(())
        }
    }

    /// Oracle that mirrors the nitro-enclave proving oracle's `check_preimage`: it re-hashes
    /// `Keccak256`-typed keys and rejects a value that doesn't match, while serving trusted key
    /// types (Local/`GlobalGeneric`/Blob/Precompile) as-is. This is exactly the verification the
    /// non-verifying [`MockOracle`] omits — the gap that let a keccak-keyed generic commitment pass
    /// unit tests yet fail `preimage hash mismatch` in the real prover.
    #[derive(Clone, Debug)]
    struct VerifyingOracle {
        key: PreimageKey,
        value: Vec<u8>,
    }

    #[async_trait]
    impl PreimageOracleClient for VerifyingOracle {
        async fn get(&self, key: PreimageKey) -> PreimageOracleResult<Vec<u8>> {
            if key != self.key {
                return Err(PreimageOracleError::KeyNotFound);
            }
            // Hash-typed keys must match their content; trusted types are served as-is.
            if key.key_type() == PreimageKeyType::Keccak256
                && key != PreimageKey::new_keccak256(*keccak256(&self.value))
            {
                return Err(PreimageOracleError::InvalidPreimageKey);
            }
            Ok(self.value.clone())
        }

        async fn get_exact(&self, key: PreimageKey, buf: &mut [u8]) -> PreimageOracleResult<()> {
            let value = self.get(key).await?;
            if value.len() != buf.len() {
                return Err(PreimageOracleError::BufferLengthMismatch(buf.len(), value.len()));
            }
            buf.copy_from_slice(&value);
            Ok(())
        }
    }

    #[async_trait]
    impl HintWriterClient for VerifyingOracle {
        async fn write(&self, _hint: &str) -> PreimageOracleResult<()> {
            Ok(())
        }
    }

    /// A well-formed generic commitment: `0x01` type byte, `0xff` sentinel, 32 arbitrary bytes.
    fn generic_commitment() -> Vec<u8> {
        let mut c = vec![0x01u8, 0xff];
        c.extend_from_slice(&[0x77u8; 32]);
        c
    }

    #[test]
    fn generic_commitment_uses_trusted_global_generic_key() {
        let c = generic_commitment();
        let key = preimage_key_for_commitment(&c);
        assert_eq!(key.key_type(), PreimageKeyType::GlobalGeneric);
        assert_eq!(key, PreimageKey::new(*keccak256(&c), PreimageKeyType::GlobalGeneric));
    }

    #[test]
    fn keccak_commitment_uses_verified_keccak_key() {
        let digest = [0x42u8; 32];
        let mut c = vec![KECCAK256_COMMITMENT_TYPE];
        c.extend_from_slice(&digest);
        let key = preimage_key_for_commitment(&c);
        assert_eq!(key.key_type(), PreimageKeyType::Keccak256);
        assert_eq!(key, PreimageKey::new_keccak256(digest));
    }

    /// Regression test for the original bug: a generic commitment's batch bytes must survive a
    /// *verifying* oracle. With the old keccak-typed key this failed `preimage hash mismatch`.
    #[tokio::test]
    async fn generic_commitment_resolves_through_verifying_oracle() {
        let commitment = generic_commitment();
        let stored = vec![0xde, 0xad, 0xbe, 0xef, 0x00, 0x11, 0x22];
        let key = preimage_key_for_commitment(&commitment);
        let resolver =
            OracleAltDaResolver::new(Arc::new(VerifyingOracle { key, value: stored.clone() }));

        let out = resolver
            .resolve(&commitment)
            .await
            .expect("generic commitment should resolve through a verifying oracle");
        assert_eq!(out.as_ref(), stored.as_slice());
    }

    /// A keccak256 commitment's bytes ARE verifiable, so a matching value resolves and a
    /// tampered value is rejected by the verifying oracle.
    #[tokio::test]
    async fn keccak_commitment_is_content_verified() {
        let data = vec![1u8, 2, 3, 4, 5];
        let mut commitment = vec![KECCAK256_COMMITMENT_TYPE];
        commitment.extend_from_slice(keccak256(&data).as_slice());
        let key = preimage_key_for_commitment(&commitment);

        let good = OracleAltDaResolver::new(Arc::new(VerifyingOracle { key, value: data.clone() }));
        assert_eq!(
            good.resolve(&commitment).await.expect("matching bytes resolve").as_ref(),
            data.as_slice()
        );

        let bad =
            OracleAltDaResolver::new(Arc::new(VerifyingOracle { key, value: vec![0xaau8; 5] }));
        assert!(matches!(
            bad.resolve(&commitment).await.unwrap_err(),
            AltDaResolverError::Resolve(_)
        ));
    }

    #[tokio::test]
    async fn missing_preimage_is_not_found() {
        let resolver = OracleAltDaResolver::new(Arc::new(MockOracle {
            key: PreimageKey::new_keccak256([0u8; 32]),
            value: vec![],
        }));
        let err = resolver.resolve(&generic_commitment()).await.unwrap_err();
        assert!(matches!(err, AltDaResolverError::NotFound));
    }
}
