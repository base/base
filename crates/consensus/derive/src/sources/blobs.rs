//! Blob Data Source

use alloc::{boxed::Box, vec::Vec};

use alloy_consensus::{
    Transaction, TxEip4844Variant, TxEnvelope, TxType, transaction::SignerRecoverable,
};
use alloy_primitives::{Address, B256, Bytes};
use async_trait::async_trait;
use base_protocol::BlockInfo;

use crate::{
    BlobData, BlobProvider, ChainProvider, DataAvailabilityProvider, Metrics, PipelineError,
    PipelineErrorKind, PipelineResult, ResetError,
};

/// A data iterator that reads from a blob.
#[derive(Debug, Clone)]
pub struct BlobSource<F, B>
where
    F: ChainProvider + Send,
    B: BlobProvider + Send,
{
    /// Chain provider.
    pub chain_provider: F,
    /// Fetches blobs.
    pub blob_fetcher: B,
    /// The address of the batcher contract.
    pub batcher_address: Address,
    /// Data.
    pub data: Vec<BlobData>,
    /// Whether the source is open.
    pub open: bool,
}

impl<F, B> BlobSource<F, B>
where
    F: ChainProvider + Send,
    B: BlobProvider + Send,
{
    /// Creates a new blob source.
    pub const fn new(chain_provider: F, blob_fetcher: B, batcher_address: Address) -> Self {
        Self { chain_provider, blob_fetcher, batcher_address, data: Vec::new(), open: false }
    }

    fn extract_blob_data(
        &self,
        txs: Vec<TxEnvelope>,
        batcher_address: Address,
    ) -> (Vec<BlobData>, Vec<B256>) {
        let mut data = Vec::new();
        let mut hashes = Vec::new();
        for tx in txs {
            let (tx_kind, calldata, blob_hashes) = match &tx {
                TxEnvelope::Legacy(tx) => (tx.tx().to(), tx.tx().input.clone(), None),
                TxEnvelope::Eip2930(tx) => (tx.tx().to(), tx.tx().input.clone(), None),
                TxEnvelope::Eip1559(tx) => (tx.tx().to(), tx.tx().input.clone(), None),
                TxEnvelope::Eip4844(blob_tx_wrapper) => match blob_tx_wrapper.tx() {
                    TxEip4844Variant::TxEip4844(tx) => {
                        (tx.to(), tx.input.clone(), Some(tx.blob_versioned_hashes.clone()))
                    }
                    TxEip4844Variant::TxEip4844WithSidecar(tx) => {
                        let tx = tx.tx();
                        (tx.to(), tx.input.clone(), Some(tx.blob_versioned_hashes.clone()))
                    }
                },
                _ => continue,
            };
            let Some(to) = tx_kind else { continue };

            if to != self.batcher_address {
                continue;
            }
            if tx.recover_signer().unwrap_or_default() != batcher_address {
                continue;
            }
            if tx.tx_type() != TxType::Eip4844 {
                let blob_data = BlobData { data: None, calldata: Some(calldata.to_vec().into()) };
                data.push(blob_data);
                continue;
            }
            if !calldata.is_empty() {
                let hash = match &tx {
                    TxEnvelope::Legacy(tx) => Some(tx.hash()),
                    TxEnvelope::Eip2930(tx) => Some(tx.hash()),
                    TxEnvelope::Eip1559(tx) => Some(tx.hash()),
                    TxEnvelope::Eip4844(blob_tx_wrapper) => Some(blob_tx_wrapper.hash()),
                    _ => None,
                };
                warn!(target: "blob_source", hash = ?hash, "Blob tx has calldata, which will be ignored");
            }
            let blob_hashes = if let Some(b) = blob_hashes {
                b
            } else {
                continue;
            };
            for hash in blob_hashes {
                hashes.push(hash);
                data.push(BlobData::default());
            }
        }
        Metrics::pipeline_data_availability_provider("blobs").increment(data.len() as f64);
        (data, hashes)
    }

    /// Loads blob data into the source if it is not open.
    async fn load_blobs(
        &mut self,
        block_ref: &BlockInfo,
        batcher_address: Address,
    ) -> PipelineResult<()> {
        if self.open {
            return Ok(());
        }

        let info = self
            .chain_provider
            .block_info_and_transactions_by_hash(block_ref.hash)
            .await
            .map_err(Into::into)?;

        let (mut data, blob_hashes) = self.extract_blob_data(info.1, batcher_address);

        // If there are no hashes, set the calldata and return.
        if blob_hashes.is_empty() {
            self.open = true;
            self.data = data;
            return Ok(());
        }

        // Convert via Into<PipelineErrorKind> which routes:
        //   BlobNotFound  -> PipelineErrorKind::Reset   (missed/orphaned slot)
        //   Backend       -> PipelineErrorKind::Temporary (transient, retry)
        //   others        -> PipelineErrorKind::Critical
        let blobs = self
            .blob_fetcher
            .get_and_validate_blobs(block_ref, &blob_hashes)
            .await
            .map_err(Into::<PipelineErrorKind>::into)
            .inspect_err(|kind| match kind {
                PipelineErrorKind::Reset(_) => {
                    warn!(
                        target: "blob_source",
                        block_hash = %block_ref.hash,
                        block_number = block_ref.number,
                        timestamp = block_ref.timestamp,
                        "Blobs permanently unavailable (missed/orphaned beacon slot); \
                         triggering pipeline reset"
                    );
                }
                _ => {
                    warn!(
                        target: "blob_source",
                        block_hash = %block_ref.hash,
                        block_number = block_ref.number,
                        timestamp = block_ref.timestamp,
                        "Failed to fetch blobs: {kind}"
                    );
                }
            })?;

        // Fill the blob pointers.
        let mut blob_index = 0;
        for blob in &mut data {
            match blob.fill(&blobs, blob_index) {
                Ok(should_increment) => {
                    if should_increment {
                        blob_index += 1;
                    }
                }
                Err(e) => {
                    return Err(e.into());
                }
            }
        }

        // Check for over-fill: ensure all blobs were consumed.
        if blob_index < blobs.len() {
            return Err(ResetError::BlobsOverFill(blob_index, blobs.len()).into());
        }

        self.open = true;
        self.data = data;
        Ok(())
    }

    /// Extracts the next data from the source.
    fn next_data(&mut self) -> PipelineResult<BlobData> {
        if self.data.is_empty() {
            return Err(PipelineError::Eof.temp());
        }

        Ok(self.data.remove(0))
    }
}

#[async_trait]
impl<F, B> DataAvailabilityProvider for BlobSource<F, B>
where
    F: ChainProvider + Sync + Send,
    B: BlobProvider + Sync + Send,
{
    type Item = Bytes;

    async fn next(
        &mut self,
        block_ref: &BlockInfo,
        batcher_address: Address,
    ) -> PipelineResult<Self::Item> {
        self.load_blobs(block_ref, batcher_address).await?;

        let next_data = self.next_data()?;
        if let Some(c) = next_data.calldata {
            return Ok(c);
        }

        // Decode the blob data to raw bytes.
        // Otherwise, ignore blob and recurse next.
        match next_data.decode() {
            Ok(d) => Ok(d),
            Err(_) => {
                warn!(target: "blob_source", "Failed to decode blob data, skipping");
                self.next(block_ref, batcher_address).await
            }
        }
    }

    fn clear(&mut self) {
        self.data.clear();
        self.open = false;
    }
}

#[cfg(test)]
pub(super) mod tests {
    use alloc::vec;

    use alloy_consensus::{Blob, Signed, TxEip4844, TxEip4844Variant};
    use alloy_primitives::Signature;
    use base_common_chains::Registry;

    use super::*;
    use crate::{
        errors::PipelineErrorKind,
        test_utils::{TestBlobProvider, TestChainProvider},
    };

    pub(crate) fn default_test_blob_source() -> BlobSource<TestChainProvider, TestBlobProvider> {
        let chain_provider = TestChainProvider::default();
        let blob_fetcher = TestBlobProvider::default();
        let batch_inbox_address = Address::default();
        BlobSource::new(chain_provider, blob_fetcher, batch_inbox_address)
    }

    fn valid_blob_batcher_address() -> Address {
        valid_blob_txs().into_iter().next().unwrap().recover_signer().unwrap()
    }

    pub(crate) fn valid_blob_txs() -> Vec<TxEnvelope> {
        let batch_inbox_address = Registry::rollup_config(8453).unwrap().batch_inbox_address;
        let sig = Signature::test_signature();
        vec![TxEnvelope::Eip4844(Signed::new_unchecked(
            TxEip4844Variant::TxEip4844(TxEip4844 {
                to: batch_inbox_address,
                blob_versioned_hashes: vec![
                    alloy_primitives::b256!(
                        "012ec3d6f66766bedb002a190126b3549fce0047de0d4c25cffce0dc1c57921a"
                    ),
                    alloy_primitives::b256!(
                        "0152d8e24762ff22b1cfd9f8c0683786a7ca63ba49973818b3d1e9512cd2cec4"
                    ),
                    alloy_primitives::b256!(
                        "013b98c6c83e066d5b14af2b85199e3d4fc7d1e778dd53130d180f5077e2d1c7"
                    ),
                    alloy_primitives::b256!(
                        "01148b495d6e859114e670ca54fb6e2657f0cbae5b08063605093a4b3dc9f8f1"
                    ),
                    alloy_primitives::b256!(
                        "011ac212f13c5dff2b2c6b600a79635103d6f580a4221079951181b25c7e6549"
                    ),
                ],
                ..Default::default()
            }),
            sig,
            Default::default(),
        ))]
    }

    #[tokio::test]
    async fn test_load_blobs_open() {
        let mut source = default_test_blob_source();
        source.open = true;
        assert!(source.load_blobs(&BlockInfo::default(), Address::ZERO).await.is_ok());
    }

    #[tokio::test]
    async fn test_load_blobs_chain_provider_err() {
        let mut source = default_test_blob_source();
        assert!(matches!(
            source.load_blobs(&BlockInfo::default(), Address::ZERO).await,
            Err(PipelineErrorKind::Temporary(_))
        ));
    }

    #[tokio::test]
    async fn test_load_blobs_chain_provider_empty_txs() {
        let mut source = default_test_blob_source();
        let block_info = BlockInfo::default();
        source.chain_provider.insert_block_with_transactions(0, block_info, Vec::new());
        assert!(!source.open); // Source is not open by default.
        assert!(source.load_blobs(&BlockInfo::default(), Address::ZERO).await.is_ok());
        assert!(source.data.is_empty());
        assert!(source.open);
    }

    #[tokio::test]
    async fn test_load_blobs_chain_provider_4844_txs_blob_fetch_error() {
        let mut source = default_test_blob_source();
        let block_info = BlockInfo::default();
        let batcher_address = valid_blob_batcher_address();
        let batch_inbox_address = Registry::rollup_config(8453).unwrap().batch_inbox_address;
        source.batcher_address = batch_inbox_address;
        let txs = valid_blob_txs();
        source.blob_fetcher.should_error = true;
        source.chain_provider.insert_block_with_transactions(1, block_info, txs);
        assert!(matches!(
            source.load_blobs(&BlockInfo::default(), batcher_address).await,
            Err(PipelineErrorKind::Critical(_))
        ));
    }

    #[tokio::test]
    async fn test_load_blobs_chain_provider_4844_txs_succeeds() {
        let mut source = default_test_blob_source();
        let block_info = BlockInfo::default();
        let batcher_address = valid_blob_batcher_address();
        let batch_inbox_address = Registry::rollup_config(8453).unwrap().batch_inbox_address;
        source.batcher_address = batch_inbox_address;
        let txs = valid_blob_txs();
        source.chain_provider.insert_block_with_transactions(1, block_info, txs);
        let hashes = [
            alloy_primitives::b256!(
                "012ec3d6f66766bedb002a190126b3549fce0047de0d4c25cffce0dc1c57921a"
            ),
            alloy_primitives::b256!(
                "0152d8e24762ff22b1cfd9f8c0683786a7ca63ba49973818b3d1e9512cd2cec4"
            ),
            alloy_primitives::b256!(
                "013b98c6c83e066d5b14af2b85199e3d4fc7d1e778dd53130d180f5077e2d1c7"
            ),
            alloy_primitives::b256!(
                "01148b495d6e859114e670ca54fb6e2657f0cbae5b08063605093a4b3dc9f8f1"
            ),
            alloy_primitives::b256!(
                "011ac212f13c5dff2b2c6b600a79635103d6f580a4221079951181b25c7e6549"
            ),
        ];
        for hash in hashes {
            source.blob_fetcher.insert_blob(hash, Blob::with_last_byte(1u8));
        }
        source.load_blobs(&BlockInfo::default(), batcher_address).await.unwrap();
        assert!(source.open);
        assert!(!source.data.is_empty());
    }

    #[tokio::test]
    async fn test_open_empty_data_eof() {
        let mut source = default_test_blob_source();
        source.open = true;

        let err = source.next(&BlockInfo::default(), Address::ZERO).await.unwrap_err();
        assert!(matches!(err, PipelineErrorKind::Temporary(PipelineError::Eof)));
    }

    #[tokio::test]
    async fn test_open_calldata() {
        let mut source = default_test_blob_source();
        source.open = true;
        source.data.push(BlobData { data: None, calldata: Some(Bytes::default()) });

        let data = source.next(&BlockInfo::default(), Address::ZERO).await.unwrap();
        assert_eq!(data, Bytes::default());
    }

    #[tokio::test]
    async fn test_open_blob_data_decode_missing_data() {
        let mut source = default_test_blob_source();
        source.open = true;
        source.data.push(BlobData { data: Some(Bytes::from(&[1; 32])), calldata: None });

        let err = source.next(&BlockInfo::default(), Address::ZERO).await.unwrap_err();
        assert!(matches!(err, PipelineErrorKind::Temporary(PipelineError::Eof)));
    }

    #[tokio::test]
    async fn test_blob_source_pipeline_error() {
        let mut source = default_test_blob_source();
        let err = source.next(&BlockInfo::default(), Address::ZERO).await.unwrap_err();
        assert!(matches!(err, PipelineErrorKind::Temporary(PipelineError::Provider(_))));
    }

    #[tokio::test]
    async fn test_load_blobs_overfill_triggers_reset() {
        let mut source = default_test_blob_source();
        let block_info = BlockInfo::default();
        let batcher_address = valid_blob_batcher_address();
        let batch_inbox_address = Registry::rollup_config(8453).unwrap().batch_inbox_address;
        source.batcher_address = batch_inbox_address;
        let txs = valid_blob_txs();
        source.blob_fetcher.should_return_extra_blob = true;
        source.chain_provider.insert_block_with_transactions(1, block_info, txs);
        let hashes = [
            alloy_primitives::b256!(
                "012ec3d6f66766bedb002a190126b3549fce0047de0d4c25cffce0dc1c57921a"
            ),
            alloy_primitives::b256!(
                "0152d8e24762ff22b1cfd9f8c0683786a7ca63ba49973818b3d1e9512cd2cec4"
            ),
            alloy_primitives::b256!(
                "013b98c6c83e066d5b14af2b85199e3d4fc7d1e778dd53130d180f5077e2d1c7"
            ),
            alloy_primitives::b256!(
                "01148b495d6e859114e670ca54fb6e2657f0cbae5b08063605093a4b3dc9f8f1"
            ),
            alloy_primitives::b256!(
                "011ac212f13c5dff2b2c6b600a79635103d6f580a4221079951181b25c7e6549"
            ),
        ];
        for hash in hashes {
            source.blob_fetcher.insert_blob(hash, Blob::with_last_byte(1u8));
        }
        let result = source.load_blobs(&BlockInfo::default(), batcher_address).await;
        assert!(matches!(result, Err(PipelineErrorKind::Reset(ResetError::BlobsOverFill(5, 6)))));
    }

    /// A minimal [`ChainProvider`] whose errors map to [`PipelineErrorKind::Reset`].
    /// Used to verify that [`BlobSource::load_blobs`] preserves the `Reset` kind when the
    /// underlying chain provider signals a reset condition (e.g. an L1 reorg).
    #[derive(Debug, Clone, Default)]
    struct ResetChainProvider;

    #[derive(Debug)]
    struct ResetProviderError;

    impl core::fmt::Display for ResetProviderError {
        fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
            write!(f, "reorg detected")
        }
    }

    impl From<ResetProviderError> for PipelineErrorKind {
        fn from(_: ResetProviderError) -> Self {
            ResetError::ReorgDetected(Default::default(), Default::default()).reset()
        }
    }

    #[async_trait::async_trait]
    impl ChainProvider for ResetChainProvider {
        type Error = ResetProviderError;

        async fn header_by_hash(
            &mut self,
            _: alloy_primitives::B256,
        ) -> Result<alloy_consensus::Header, Self::Error> {
            Err(ResetProviderError)
        }

        async fn block_info_by_number(&mut self, _: u64) -> Result<BlockInfo, Self::Error> {
            Err(ResetProviderError)
        }

        async fn receipts_by_hash(
            &mut self,
            _: alloy_primitives::B256,
        ) -> Result<alloc::vec::Vec<alloy_consensus::Receipt>, Self::Error> {
            Err(ResetProviderError)
        }

        async fn block_info_and_transactions_by_hash(
            &mut self,
            _: alloy_primitives::B256,
        ) -> Result<(BlockInfo, alloc::vec::Vec<TxEnvelope>), Self::Error> {
            Err(ResetProviderError)
        }
    }

    /// Regression test: a beacon node 404 (missed/orphaned slot) must propagate through
    /// `load_blobs` as `PipelineErrorKind::Reset`, not as a temporary retryable error.
    #[tokio::test]
    async fn test_load_blobs_not_found_triggers_reset() {
        let mut source = default_test_blob_source();
        let block_info = BlockInfo::default();
        let batcher_address = valid_blob_batcher_address();
        let batch_inbox_address = Registry::rollup_config(8453).unwrap().batch_inbox_address;
        source.batcher_address = batch_inbox_address;
        source.chain_provider.insert_block_with_transactions(1, block_info, valid_blob_txs());
        source.blob_fetcher.should_return_not_found = true;
        let err = source.load_blobs(&BlockInfo::default(), batcher_address).await.unwrap_err();
        assert!(
            matches!(err, PipelineErrorKind::Reset(_)),
            "expected Reset for missed beacon slot, got {err:?}"
        );
    }

    /// Regression test: `BlobProviderError::BlobNotFound` from the blob fetcher must surface
    /// through `next()` as `PipelineErrorKind::Reset`, triggering a pipeline reset.
    /// Without this, a missed beacon slot causes an infinite retry loop and safe head stall.
    #[tokio::test]
    async fn test_missed_beacon_slot_triggers_pipeline_reset() {
        let mut source = default_test_blob_source();
        let block_info = BlockInfo::default();
        let batcher_address = valid_blob_batcher_address();
        let batch_inbox_address = Registry::rollup_config(8453).unwrap().batch_inbox_address;
        source.batcher_address = batch_inbox_address;
        source.chain_provider.insert_block_with_transactions(1, block_info, valid_blob_txs());
        source.blob_fetcher.should_return_not_found = true;
        let err = source.next(&BlockInfo::default(), batcher_address).await.unwrap_err();
        assert!(
            matches!(err, PipelineErrorKind::Reset(_)),
            "expected Reset for missed beacon slot, got {err:?}"
        );
    }

    /// Regression test: when `block_info_and_transactions_by_hash` returns an error that maps to
    /// `PipelineErrorKind::Reset`, `load_blobs` must propagate the `Reset` kind unchanged.
    ///
    /// Before the fix, `BlobSource` wrapped every chain-provider error as
    /// `BlobProviderError::Backend(e.to_string()).into()`, which unconditionally produced
    /// `PipelineErrorKind::Temporary`. The fix uses `map_err(Into::into)` so the `Reset` kind
    /// is preserved, allowing the pipeline to recover via reset rather than spinning in a retry loop.
    #[tokio::test]
    async fn test_load_blobs_reset_error_preserved() {
        let mut source =
            BlobSource::new(ResetChainProvider, TestBlobProvider::default(), Address::ZERO);
        let err = source.load_blobs(&BlockInfo::default(), Address::ZERO).await.unwrap_err();
        assert!(
            matches!(err, PipelineErrorKind::Reset(_)),
            "expected Reset kind to be preserved, got {err:?}"
        );
    }

    /// Verifies that EIP-4844 blobs from non-batcher transactions are excluded from the pipeline.
    ///
    /// When the configured batch inbox address (`source.batcher_address`) does not match the
    /// transaction's `to` field, the entire transaction is skipped — no blob hashes are collected
    /// and `data` remains empty. This tests the exclusion path that previously incremented an
    /// index counter regardless of the sender.
    ///
    /// Two cases are exercised back-to-back to make the contrast explicit:
    /// 1. Wrong batch inbox address → no blobs captured.
    /// 2. Correct batch inbox address → all 5 blobs from the batcher transaction are captured.
    #[test]
    fn test_extract_blob_data_non_batcher_blobs_excluded() {
        // Case 1: source.batcher_address = Address::ZERO does not match the tx's batch inbox
        // address from `Registry::rollup_config(8453)`, so the transaction is skipped and no
        // blobs are captured.
        let source = default_test_blob_source(); // batch_inbox_address = Address::ZERO
        let batcher_address = valid_blob_batcher_address();
        let (data, hashes) = source.extract_blob_data(valid_blob_txs(), batcher_address);
        assert!(data.is_empty(), "non-batcher blobs must not be captured (data)");
        assert!(hashes.is_empty(), "non-batcher blob hashes must not be captured");

        // Case 2: correct batch inbox address → all 5 blobs from the batcher transaction captured.
        let mut source2 = default_test_blob_source();
        let batch_inbox_address = Registry::rollup_config(8453).unwrap().batch_inbox_address;
        source2.batcher_address = batch_inbox_address;
        let batcher_address = valid_blob_batcher_address();
        let (data, hashes) = source2.extract_blob_data(valid_blob_txs(), batcher_address);
        assert_eq!(data.len(), 5, "all 5 batcher blobs must be captured");
        assert_eq!(hashes.len(), 5, "all 5 batcher blob hashes must be captured");
    }
}
