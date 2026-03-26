//! Contains an online implementation of the `BlobProvider` trait.

use std::{boxed::Box, string::ToString, vec::Vec};

use alloy_eips::eip4844::{Blob, Bytes48, env_settings::EnvKzgSettings};
use alloy_primitives::{B256, FixedBytes};
use async_trait::async_trait;
use base_consensus_derive::{BlobProvider, BlobProviderError};
use base_protocol::BlockInfo;
use tracing::warn;

use crate::BeaconClient;
#[cfg(feature = "metrics")]
use crate::Metrics;

/// A boxed blob.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BoxedBlob {
    /// The blob data.
    pub blob: Box<Blob>,
}

/// A blob with its KZG commitment and proof.
///
/// This is used by the host to store blob preimages. Unlike `BlobTransactionSidecarItem`,
/// this does not include an index field since blobs are fetched by hash, not by index.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BlobWithCommitmentAndProof {
    /// The blob data.
    pub blob: Box<Blob>,
    /// The KZG commitment for the blob.
    pub kzg_commitment: Bytes48,
    /// The KZG proof for the blob.
    pub kzg_proof: Bytes48,
}

/// An online implementation of the [`BlobProvider`] trait.
#[derive(Debug, Clone)]
pub struct OnlineBlobProvider<B: BeaconClient> {
    /// The Beacon API client.
    pub beacon_client: B,
    /// Beacon Genesis time used for the time to slot conversion.
    pub genesis_time: u64,
    /// Slot interval used for the time to slot conversion.
    pub slot_interval: u64,
}

impl<B: BeaconClient> OnlineBlobProvider<B> {
    /// Creates a new instance of the [`OnlineBlobProvider`].
    ///
    /// The `genesis_time` and `slot_interval` arguments are _optional_ and the
    /// [`OnlineBlobProvider`] will attempt to load them dynamically at runtime if they are not
    /// provided.
    ///
    /// ## Panics
    /// Panics if the genesis time or slot interval cannot be loaded from the beacon client.
    pub async fn init(beacon_client: B) -> Self {
        let genesis_time = beacon_client
            .genesis_time()
            .await
            .map(|r| r.data.genesis_time)
            .map_err(|e| BlobProviderError::Backend(e.to_string()))
            .expect("Failed to load genesis time from beacon client");
        let slot_interval = beacon_client
            .slot_interval()
            .await
            .map(|r| r.data.seconds_per_slot)
            .map_err(|e| BlobProviderError::Backend(e.to_string()))
            .expect("Failed to load slot interval from beacon client");
        Self { beacon_client, genesis_time, slot_interval }
    }

    /// Computes the slot for the given timestamp.
    pub const fn slot(
        genesis: u64,
        slot_time: u64,
        timestamp: u64,
    ) -> Result<u64, BlobProviderError> {
        if timestamp < genesis {
            return Err(BlobProviderError::SlotDerivation);
        }
        Ok((timestamp - genesis) / slot_time)
    }

    /// Fetches blobs for the given slot.
    async fn fetch_filtered_blobs(
        &self,
        slot: u64,
        blob_hashes: &[B256],
    ) -> Result<Vec<BoxedBlob>, BlobProviderError> {
        base_metrics::inc!(gauge, Metrics::BLOB_FETCHES);

        let result =
            self.beacon_client.filtered_beacon_blobs(slot, blob_hashes).await.map_err(|e| {
                // The beacon node returned 404 for this slot. The slot was missed or
                // orphaned; its blobs will never be available. Map to BlobNotFound so
                // the pipeline issues a reset rather than retrying indefinitely.
                let Some(missing_slot) = B::slot_not_found(&e) else {
                    return BlobProviderError::Backend(e.to_string());
                };
                warn!(
                    target: "blob_provider",
                    slot = missing_slot,
                    "Beacon slot not found (404); slot may be missed or orphaned, \
                     triggering pipeline reset"
                );
                BlobProviderError::BlobNotFound { slot: missing_slot, reason: e.to_string() }
            });

        #[cfg(feature = "metrics")]
        if result.is_err() {
            base_metrics::inc!(gauge, Metrics::BLOB_FETCH_ERRORS);
        }

        result
    }

    /// Converts a vector of boxed blobs to a vector of blobs with their KZG commitments and proofs.
    ///
    /// Note: for performance reasons, we need to transmute the blobs to the `c_kzg::Blob` type to
    /// avoid the overhead of moving the blobs around or reallocating the memory.
    fn blobs_with_proofs(
        blobs: Vec<BoxedBlob>,
    ) -> Result<Vec<BlobWithCommitmentAndProof>, c_kzg::Error> {
        blobs
            .into_iter()
            .map(|blob| {
                let kzg_settings = EnvKzgSettings::Default;

                // SAFETY: all types have the same size and alignment
                let kzg_blob =
                    unsafe { Box::from_raw(Box::<Blob>::into_raw(blob.blob) as *mut c_kzg::Blob) };

                let commitment = kzg_settings
                    .get()
                    .blob_to_kzg_commitment(&kzg_blob)
                    .map(|blob| blob.to_bytes())?;
                let proof = kzg_settings
                    .get()
                    .compute_blob_kzg_proof(&kzg_blob, &commitment)
                    .map(|proof| proof.to_bytes())?;

                // SAFETY: all types have the same size and alignment
                let alloy_blob =
                    unsafe { Box::from_raw(Box::<c_kzg::Blob>::into_raw(kzg_blob) as *mut Blob) };

                Ok(BlobWithCommitmentAndProof {
                    blob: alloy_blob,
                    kzg_commitment: FixedBytes::from(*commitment),
                    kzg_proof: FixedBytes::from(*proof),
                })
            })
            .collect()
    }

    /// Fetches and validates blobs for the given block reference and blob hashes.
    /// Recomputes the kzg proofs associated with the blobs and returns as a vector of
    /// [`BlobWithCommitmentAndProof`]s.
    ///
    /// Use [`Self::beacon_client`] to fetch the blobs without recomputing the kzg
    /// proofs/commitments.
    pub async fn fetch_blobs_with_proofs(
        &self,
        block_ref: &BlockInfo,
        blob_hashes: &[B256],
    ) -> Result<Vec<BlobWithCommitmentAndProof>, BlobProviderError> {
        if blob_hashes.is_empty() {
            return Ok(Default::default());
        }

        // Calculate the slot for the given timestamp.
        let slot = Self::slot(self.genesis_time, self.slot_interval, block_ref.timestamp)?;

        // Fetch blobs for the slot.
        let blobs = self.fetch_filtered_blobs(slot, blob_hashes).await?;

        Self::blobs_with_proofs(blobs)
            .map_err(|e| BlobProviderError::Backend(format!("KZG commitment error: {e}")))
    }
}

#[async_trait]
impl<B> BlobProvider for OnlineBlobProvider<B>
where
    B: BeaconClient + Send + Sync,
{
    type Error = BlobProviderError;

    /// Fetches blobs that were confirmed in the specified L1 block with the given versioned
    /// hashes. The blobs are already validated by the beacon client by recomputing their
    /// commitments and checking against the expected hashes.
    async fn get_and_validate_blobs(
        &mut self,
        block_ref: &BlockInfo,
        blob_hashes: &[B256],
    ) -> Result<Vec<Box<Blob>>, Self::Error> {
        if blob_hashes.is_empty() {
            return Ok(Default::default());
        }

        // Calculate the slot for the given timestamp.
        let slot = Self::slot(self.genesis_time, self.slot_interval, block_ref.timestamp)?;

        // Fetch and validate blobs from the beacon client.
        // The beacon client already validates each blob by recomputing its commitment
        // and checking it against the expected hash.
        let blobs = self.fetch_filtered_blobs(slot, blob_hashes).await?;

        Ok(blobs.into_iter().map(|boxed_blob| boxed_blob.blob).collect())
    }
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;

    use alloy_eips::eip4844::{Blob, env_settings::EnvKzgSettings, kzg_to_versioned_hash};
    use alloy_primitives::{B256, FixedBytes};
    use async_trait::async_trait;
    use base_consensus_derive::{BlobProvider, BlobProviderError};
    use base_protocol::BlockInfo;

    use super::{BlobWithCommitmentAndProof, BoxedBlob, OnlineBlobProvider};
    use crate::{APIConfigResponse, APIGenesisResponse, BeaconClient};

    /// Local error type for [`MockBeaconClient`].
    #[derive(Debug)]
    enum MockBeaconError {
        SlotNotFound,
        BlobNotFound(B256),
    }

    impl std::fmt::Display for MockBeaconError {
        fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            match self {
                Self::SlotNotFound => write!(f, "slot not found"),
                Self::BlobNotFound(h) => write!(f, "blob not found: {h}"),
            }
        }
    }

    /// A minimal [`BeaconClient`] for unit-testing [`OnlineBlobProvider`].
    ///
    /// `filtered_beacon_blobs` iterates the requested hashes in order and looks each up in the
    /// internal map, returning `BlobNotFound` for any miss — matching the ordering contract of
    /// [`crate::OnlineBeaconClient`].
    #[derive(Debug, Default)]
    struct MockBeaconClient {
        blobs: HashMap<B256, Blob>,
        fail_with_slot_not_found: bool,
    }

    #[async_trait]
    impl BeaconClient for MockBeaconClient {
        type Error = MockBeaconError;

        fn slot_not_found(err: &Self::Error) -> Option<u64> {
            if matches!(err, MockBeaconError::SlotNotFound) { Some(0) } else { None }
        }

        async fn slot_interval(&self) -> Result<APIConfigResponse, Self::Error> {
            Ok(APIConfigResponse::new(12))
        }

        async fn genesis_time(&self) -> Result<APIGenesisResponse, Self::Error> {
            Ok(APIGenesisResponse::new(0))
        }

        async fn filtered_beacon_blobs(
            &self,
            _slot: u64,
            blob_hashes: &[B256],
        ) -> Result<Vec<BoxedBlob>, Self::Error> {
            if self.fail_with_slot_not_found {
                return Err(MockBeaconError::SlotNotFound);
            }
            blob_hashes
                .iter()
                .map(|hash| {
                    self.blobs
                        .get(hash)
                        .map(|blob| BoxedBlob { blob: Box::new(*blob) })
                        .ok_or(MockBeaconError::BlobNotFound(*hash))
                })
                .collect()
        }
    }

    /// Computes the versioned hash for a blob using the KZG settings.
    fn versioned_hash_for(blob: &Blob) -> B256 {
        let kzg_settings = EnvKzgSettings::Default;
        let kzg_blob = c_kzg::Blob::new(blob.0);
        let commitment = kzg_settings.get().blob_to_kzg_commitment(&kzg_blob).unwrap();
        kzg_to_versioned_hash(commitment.as_slice())
    }

    /// An empty `blob_hashes` slice must return `Ok(vec![])` without touching the beacon client.
    #[tokio::test]
    async fn test_get_and_validate_blobs_empty() {
        let mut provider = OnlineBlobProvider {
            beacon_client: MockBeaconClient::default(),
            genesis_time: 0,
            slot_interval: 12,
        };
        let block_ref = BlockInfo::default();
        let result = provider.get_and_validate_blobs(&block_ref, &[]).await.unwrap();
        assert!(result.is_empty(), "empty hashes must return empty blob vec");
    }

    /// Verifies that `get_and_validate_blobs` preserves the ordering of the input hashes.
    ///
    /// The mock stores `{hash_a: blob_a, hash_b: blob_b}` but the request is `[hash_b, hash_a]`.
    /// The result must be `[blob_b, blob_a]`, following the request order end-to-end through
    /// [`OnlineBlobProvider`].
    #[tokio::test]
    async fn test_get_and_validate_blobs_ordering() {
        let blob_a: Blob = FixedBytes::repeat_byte(1);
        let blob_b: Blob = FixedBytes::repeat_byte(2);
        let hash_a = versioned_hash_for(&blob_a);
        let hash_b = versioned_hash_for(&blob_b);

        let mut mock = MockBeaconClient::default();
        mock.blobs.insert(hash_a, blob_a);
        mock.blobs.insert(hash_b, blob_b);

        let mut provider =
            OnlineBlobProvider { beacon_client: mock, genesis_time: 0, slot_interval: 12 };
        let block_ref = BlockInfo { timestamp: 12, ..Default::default() };

        let result = provider.get_and_validate_blobs(&block_ref, &[hash_b, hash_a]).await.unwrap();

        assert_eq!(result.len(), 2);
        assert_eq!(*result[0], blob_b, "first result must be blob_b (requested first)");
        assert_eq!(*result[1], blob_a, "second result must be blob_a (requested second)");
    }

    /// Verifies that a slot-not-found signal from the beacon client propagates through
    /// [`OnlineBlobProvider`] as `BlobProviderError::BlobNotFound`.
    ///
    /// This is the path taken when a beacon node returns HTTP 404 for a missed/orphaned slot.
    /// The provider must surface `BlobNotFound` so the pipeline triggers a reset instead of
    /// retrying indefinitely.
    #[tokio::test]
    async fn test_get_and_validate_blobs_slot_not_found() {
        let hash = versioned_hash_for(&FixedBytes::repeat_byte(1));

        let mock = MockBeaconClient { fail_with_slot_not_found: true, ..Default::default() };

        let mut provider =
            OnlineBlobProvider { beacon_client: mock, genesis_time: 0, slot_interval: 12 };
        let block_ref = BlockInfo { timestamp: 12, ..Default::default() };

        let result = provider.get_and_validate_blobs(&block_ref, &[hash]).await;

        assert!(
            matches!(result, Err(BlobProviderError::BlobNotFound { .. })),
            "slot-not-found must propagate as BlobProviderError::BlobNotFound, got {result:?}"
        );
    }

    /// Verifies that `fetch_blobs_with_proofs` produces cryptographically correct KZG commitments.
    ///
    /// Stores a real blob in the mock, calls `fetch_blobs_with_proofs`, then recomputes the
    /// versioned hash from the returned commitment and asserts it matches the requested hash.
    #[tokio::test]
    async fn test_fetch_blobs_with_proofs_kzg_valid() {
        let blob: Blob = FixedBytes::repeat_byte(1);
        let hash = versioned_hash_for(&blob);

        let mut mock = MockBeaconClient::default();
        mock.blobs.insert(hash, blob);

        let provider =
            OnlineBlobProvider { beacon_client: mock, genesis_time: 0, slot_interval: 12 };
        let block_ref = BlockInfo { timestamp: 12, ..Default::default() };

        let result: Vec<BlobWithCommitmentAndProof> =
            provider.fetch_blobs_with_proofs(&block_ref, &[hash]).await.unwrap();

        assert_eq!(result.len(), 1);
        let commitment = result[0].kzg_commitment;
        assert_ne!(commitment, FixedBytes::ZERO, "KZG commitment must be non-zero");

        // Recompute the versioned hash from the returned commitment and verify it
        // matches the hash we originally requested.
        let recomputed_hash = kzg_to_versioned_hash(commitment.as_slice());
        assert_eq!(
            recomputed_hash, hash,
            "versioned hash derived from returned commitment must equal the requested hash"
        );
    }
}
