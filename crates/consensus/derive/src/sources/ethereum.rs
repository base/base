//! Contains the [`EthereumDataSource`], which is a concrete implementation of the
//! [`DataAvailabilityProvider`] trait for the Ethereum protocol.

use alloc::{boxed::Box, fmt::Debug};

use alloy_primitives::{Address, Bytes};
use async_trait::async_trait;
use base_common_genesis::{BaseFeature, RollupConfig};
use base_protocol::BlockInfo;

use crate::{
    BlobProvider, BlobSource, CalldataSource, ChainProvider, DataAvailabilityProvider,
    PipelineResult,
};

/// A factory for creating an Ethereum data source provider.
#[derive(Debug, Clone)]
pub struct EthereumDataSource<C, B>
where
    C: ChainProvider + Send + Clone,
    B: BlobProvider + Send + Clone,
{
    /// The explicitly configured blob data-source boundary, captured when the source is built.
    pub blob_activation_timestamp: Option<u64>,
    /// The blob source.
    pub blob_source: BlobSource<C, B>,
    /// The calldata source.
    pub calldata_source: CalldataSource<C>,
}

impl<C, B> EthereumDataSource<C, B>
where
    C: ChainProvider + Send + Clone + Debug,
    B: BlobProvider + Send + Clone + Debug,
{
    /// Instantiates a new [`EthereumDataSource`].
    pub const fn new(
        blob_source: BlobSource<C, B>,
        calldata_source: CalldataSource<C>,
        cfg: &RollupConfig,
    ) -> Self {
        Self {
            blob_activation_timestamp: cfg
                .upgrades
                .activation_timestamp(BaseFeature::BlobDataSource.activation_upgrade()),
            blob_source,
            calldata_source,
        }
    }

    /// Instantiates a new [`EthereumDataSource`] from parts.
    pub fn new_from_parts(provider: C, blobs: B, cfg: &RollupConfig) -> Self {
        Self::new(
            BlobSource::new(provider.clone(), blobs, cfg.batch_inbox_address),
            CalldataSource::new(provider, cfg.batch_inbox_address),
            cfg,
        )
    }
}

#[async_trait]
impl<C, B> DataAvailabilityProvider for EthereumDataSource<C, B>
where
    C: ChainProvider + Send + Sync + Clone + Debug,
    B: BlobProvider + Send + Sync + Clone + Debug,
{
    type Item = Bytes;

    async fn next(
        &mut self,
        block_ref: &BlockInfo,
        batcher_address: Address,
    ) -> PipelineResult<Self::Item> {
        // Unlike L2 format gates, blob sourcing uses the configured L1 boundary captured at
        // construction, not a runtime override or an implied activation in a sparse schedule.
        let blobs_enabled = self
            .blob_activation_timestamp
            .is_some_and(|activation| block_ref.timestamp >= activation);
        if blobs_enabled {
            self.blob_source.next(block_ref, batcher_address).await
        } else {
            self.calldata_source.next(block_ref, batcher_address).await
        }
    }

    fn clear(&mut self) {
        self.blob_source.clear();
        self.calldata_source.clear();
    }
}

#[cfg(test)]
mod tests {
    use alloc::vec;

    use alloy_consensus::TxEnvelope;
    use alloy_eips::eip2718::Decodable2718;
    use alloy_primitives::{Address, address};
    use base_common_genesis::{
        BaseUpgrade, RollupConfig, RuntimeUpgradeRegistry, SystemConfig, UpgradeActivation,
        UpgradeConfig,
    };
    use base_protocol::BlockInfo;

    use super::*;
    use crate::{
        BlobData,
        test_utils::{TestBlobProvider, TestChainProvider},
    };

    fn default_test_blob_source() -> BlobSource<TestChainProvider, TestBlobProvider> {
        let chain_provider = TestChainProvider::default();
        let blob_fetcher = TestBlobProvider::default();
        let batcher_address = Address::default();
        BlobSource::new(chain_provider, blob_fetcher, batcher_address)
    }

    #[tokio::test]
    async fn test_clear_ethereum_data_source() {
        let chain = TestChainProvider::default();
        let blob = TestBlobProvider::default();
        let cfg = RollupConfig::default();
        let mut calldata = CalldataSource::new(chain.clone(), Address::ZERO);
        calldata.calldata.insert(0, Default::default());
        calldata.open = true;
        let mut blob = BlobSource::new(chain, blob, Address::ZERO);
        blob.data = vec![Default::default()];
        blob.open = true;
        let mut data_source = EthereumDataSource::new(blob, calldata, &cfg);

        data_source.clear();
        assert!(data_source.blob_source.data.is_empty());
        assert!(!data_source.blob_source.open);
        assert!(data_source.calldata_source.calldata.is_empty());
        assert!(!data_source.calldata_source.open);
    }

    #[tokio::test]
    async fn test_open_blob_source() {
        let chain = TestChainProvider::default();
        let mut blob = default_test_blob_source();
        blob.open = true;
        blob.data.push(BlobData { data: None, calldata: Some(Bytes::default()) });
        let calldata = CalldataSource::new(chain.clone(), Address::ZERO);
        let cfg = RollupConfig {
            upgrades: UpgradeConfig { ecotone_time: Some(0), ..Default::default() },
            ..Default::default()
        };

        // Should successfully retrieve a blob batch from the block
        let mut data_source = EthereumDataSource::new(blob, calldata, &cfg);
        let data = data_source.next(&BlockInfo::default(), Address::ZERO).await.unwrap();
        assert_eq!(data, Bytes::default());
    }

    #[tokio::test]
    async fn blob_source_uses_explicit_activation_boundary() {
        for (ecotone_time, timestamp, expected) in [
            (Some(10), 9, Bytes::from_static(b"calldata")),
            (Some(10), 10, Bytes::from_static(b"blob")),
            (Some(0), 0, Bytes::from_static(b"blob")),
            (None, 10, Bytes::from_static(b"calldata")),
        ] {
            // A later scheduled fork must not imply this L1 data-source transition.
            let cfg = RollupConfig {
                upgrades: UpgradeConfig { ecotone_time, fjord_time: Some(0), ..Default::default() },
                ..Default::default()
            };
            let mut blob = default_test_blob_source();
            blob.open = true;
            blob.data.push(BlobData { data: None, calldata: Some(Bytes::from_static(b"blob")) });
            let mut calldata = CalldataSource::new(TestChainProvider::default(), Address::ZERO);
            calldata.open = true;
            calldata.calldata.push_back(Bytes::from_static(b"calldata"));
            let mut source = EthereumDataSource::new(blob, calldata, &cfg);

            let block = BlockInfo { timestamp, ..Default::default() };
            assert_eq!(source.next(&block, Address::ZERO).await.unwrap(), expected);
        }
    }

    #[tokio::test]
    async fn blob_source_preserves_configured_schedule_snapshot() {
        let chain_id = 4_294_962_813;
        let cfg = RollupConfig { l2_chain_id: chain_id.into(), ..Default::default() };
        let mut blob = default_test_blob_source();
        blob.open = true;
        blob.data.push(BlobData { data: None, calldata: Some(Bytes::from_static(b"blob")) });
        let mut calldata = CalldataSource::new(TestChainProvider::default(), Address::ZERO);
        calldata.open = true;
        calldata.calldata.extend([
            Bytes::from_static(b"before"),
            Bytes::from_static(b"after"),
            Bytes::from_static(b"disabled"),
        ]);
        let mut source = EthereumDataSource::new(blob, calldata, &cfg);
        let block = BlockInfo::default();

        let before = source.next(&block, Address::ZERO).await;
        RuntimeUpgradeRegistry::set_activation_timestamp(chain_id, BaseUpgrade::Ecotone, 0);
        let enabled = source.next(&block, Address::ZERO).await;
        RuntimeUpgradeRegistry::set_activation(
            chain_id,
            BaseUpgrade::Ecotone,
            UpgradeActivation::Never,
        );
        let disabled = source.next(&block, Address::ZERO).await;
        RuntimeUpgradeRegistry::clear_chain(chain_id);

        assert_eq!(before.unwrap(), Bytes::from_static(b"before"));
        assert_eq!(enabled.unwrap(), Bytes::from_static(b"after"));
        assert_eq!(disabled.unwrap(), Bytes::from_static(b"disabled"));
    }

    #[tokio::test]
    async fn test_open_ethereum_calldata_source_pre_ecotone() {
        let mut chain = TestChainProvider::default();
        let blob = TestBlobProvider::default();
        let batcher_address = address!("6887246668a3b87F54DeB3b94Ba47a6f63F32985");
        let batch_inbox = address!("FF00000000000000000000000000000000000010");
        let block_ref = BlockInfo { number: 10, ..Default::default() };

        let mut cfg = RollupConfig::default();
        cfg.genesis.system_config = Some(SystemConfig { batcher_address, ..Default::default() });
        cfg.batch_inbox_address = batch_inbox;

        // load a test batcher transaction
        let raw_batcher_tx = include_bytes!("../../testdata/raw_batcher_tx.hex");
        let tx = TxEnvelope::decode_2718(&mut raw_batcher_tx.as_ref()).unwrap();
        chain.insert_block_with_transactions(10, block_ref, vec![tx]);

        // Should successfully retrieve a calldata batch from the block
        let mut data_source = EthereumDataSource::new_from_parts(chain, blob, &cfg);
        let calldata_batch = data_source.next(&block_ref, batcher_address).await.unwrap();
        assert_eq!(calldata_batch.len(), 119823);
    }
}
