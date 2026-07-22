//! Contains an oracle-backed [`AltDaCommitmentResolver`] for the client program.

use alloc::{boxed::Box, string::ToString, sync::Arc};

use alloy_primitives::{Bytes, keccak256};
use async_trait::async_trait;
use base_consensus_derive::{AltDaCommitmentResolver, AltDaResolverError};
use base_proof_preimage::{CommsClient, PreimageKey};

use crate::HintType;

/// An oracle-backed alt-DA commitment resolver.
///
/// Resolves a generic alt-DA commitment to its off-chain batch bytes by hinting the host
/// (which fetches the object from the da-server) and reading the bytes back from the preimage
/// oracle, keyed by `keccak256(commitment)`. Mirrors [`OracleBlobProvider`](crate::OracleBlobProvider).
///
/// Generic commitments are random sentinels rather than content hashes, so the guest cannot
/// cryptographically verify the resolved bytes against the commitment — it trusts the
/// host-supplied mapping. This is inherent to alt-DA generic commitments and acceptable for the
/// TEE backend, where data availability/correctness is the alt-DA layer's responsibility.
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

        // The host stores the resolved bytes under keccak256(commitment). Objects are
        // variable-length (up to MAX_DA_OBJECT_BYTES), so use `get` rather than `get_exact`.
        let key = PreimageKey::new_keccak256(*keccak256(commitment));
        let bytes =
            self.oracle.get(key).await.map_err(|e| AltDaResolverError::Resolve(e.to_string()))?;

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

    #[tokio::test]
    async fn resolves_commitment_keyed_by_keccak() {
        let commitment = vec![0xabu8; 34];
        let stored = vec![0x00u8, 1, 2, 3, 4];
        let key = PreimageKey::new_keccak256(*keccak256(&commitment));
        let resolver =
            OracleAltDaResolver::new(Arc::new(MockOracle { key, value: stored.clone() }));

        let out = resolver.resolve(&commitment).await.expect("resolve should succeed");
        assert_eq!(out.as_ref(), stored.as_slice());
    }

    #[tokio::test]
    async fn missing_preimage_is_an_error() {
        let resolver = OracleAltDaResolver::new(Arc::new(MockOracle {
            key: PreimageKey::new_keccak256([0u8; 32]),
            value: vec![],
        }));
        let err = resolver.resolve(&[0xcd; 34]).await.unwrap_err();
        assert!(matches!(err, AltDaResolverError::Resolve(_)));
    }
}
