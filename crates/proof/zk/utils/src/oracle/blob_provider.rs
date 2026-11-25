use alloc::{boxed::Box, vec::Vec};
use core::fmt::Display;

use alloy_consensus::Blob;
use alloy_eips::eip4844::kzg_to_versioned_hash;
use alloy_primitives::B256;
use async_trait::async_trait;
use base_consensus_derive::{BlobProvider, BlobProviderError};
use base_protocol::BlockInfo;
use revm::precompile::Crypto;

use crate::witness::BlobData;

/// KZG verification a range program needs: blob batch verification during derivation, and the
/// EIP-4844 point-evaluation precompile through [`Crypto::verify_kzg_proof`].
///
/// Each zkVM supplies its own implementation. [`WitnessExecutor`](crate::witness::WitnessExecutor)
/// installs it as the revm crypto provider.
pub trait KzgVerifier: Crypto + Default + 'static {
    /// Error for malformed blobs, commitments, or proofs.
    type Error: Display;

    /// Verifies every blob against its commitment and proof (EIP-4844
    /// `verify_blob_kzg_proof_batch`).
    fn verify_blob_kzg_proof_batch(&self, blob_data: BlobData) -> Result<bool, Self::Error>;
}

/// KZG-verified blob storage for the zkVM oracle.
#[derive(Clone, Debug, Default)]
pub struct BlobStore {
    versioned_blobs: Vec<(B256, Blob)>,
}

impl BlobStore {
    /// Verifies the witness blobs with `kzg` and stores them for the derivation pipeline.
    ///
    /// # Panics
    ///
    /// Panics if any blob fails KZG verification.
    pub fn new<K: KzgVerifier>(value: BlobData, kzg: &K) -> Self {
        let versioned_blobs = value
            .commitments
            .iter()
            .map(|c| kzg_to_versioned_hash(c.as_slice()))
            .zip(value.blobs.iter().map(|b| Blob::from(b.0)))
            .rev()
            .collect();

        match kzg.verify_blob_kzg_proof_batch(value) {
            Ok(true) => {} // Verification passed
            Ok(false) => panic!("KZG proof verification failed: invalid proofs"),
            Err(e) => panic!("KZG proof verification error: {e}"),
        }

        Self { versioned_blobs }
    }
}

#[async_trait]
impl BlobProvider for BlobStore {
    type Error = BlobProviderError;

    async fn get_and_validate_blobs(
        &mut self,
        _: &BlockInfo,
        blob_hashes: &[B256],
    ) -> Result<Vec<Box<Blob>>, Self::Error> {
        Ok(blob_hashes
            .iter()
            .filter_map(|hash| {
                let (blob_hash, blob) = self.versioned_blobs.pop().unwrap();
                (*hash == blob_hash).then(|| Box::new(blob))
            })
            .collect())
    }
}
