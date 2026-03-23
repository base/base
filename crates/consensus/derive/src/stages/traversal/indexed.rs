//! Contains the [`IndexedTraversal`] stage of the derivation pipeline.

use alloc::{boxed::Box, sync::Arc};

use alloy_primitives::Address;
use async_trait::async_trait;
use base_consensus_genesis::{RollupConfig, SystemConfig};
use base_protocol::BlockInfo;

use crate::{
    ActivationSignal, ChainProvider, L1RetrievalProvider, OriginAdvancer, OriginProvider,
    PipelineError, PipelineResult, ResetError, ResetSignal, Signal, SignalReceiver,
};

/// The [`IndexedTraversal`] stage of the derivation pipeline.
///
/// This stage sits at the bottom of the pipeline, holding a handle to the data source
/// (a [`ChainProvider`] implementation) and the current L1 [`BlockInfo`] in the pipeline,
/// which are used to traverse the L1 chain. When the [`IndexedTraversal`] stage is advanced,
/// it fetches the next L1 [`BlockInfo`] from the data source and updates the [`SystemConfig`]
/// with the receipts from the block.
#[derive(Debug, Clone)]
pub struct IndexedTraversal<Provider: ChainProvider> {
    /// The current block in the traversal stage.
    pub block: Option<BlockInfo>,
    /// The data source for the traversal stage.
    pub data_source: Provider,
    /// Indicates whether the block has been consumed by other stages.
    pub done: bool,
    /// The system config.
    pub system_config: SystemConfig,
    /// A reference to the rollup config.
    pub rollup_config: Arc<RollupConfig>,
}

#[async_trait]
impl<F: ChainProvider + Send> L1RetrievalProvider for IndexedTraversal<F> {
    fn batcher_addr(&self) -> Address {
        self.system_config.batcher_address
    }

    async fn next_l1_block(&mut self) -> PipelineResult<Option<BlockInfo>> {
        if !self.done {
            self.done = true;
            Ok(self.block)
        } else {
            Err(PipelineError::Eof.temp())
        }
    }
}

impl<F: ChainProvider> IndexedTraversal<F> {
    /// Creates a new [`IndexedTraversal`] instance.
    pub fn new(data_source: F, cfg: Arc<RollupConfig>) -> Self {
        Self {
            block: Some(BlockInfo::default()),
            data_source,
            done: false,
            system_config: SystemConfig::default(),
            rollup_config: cfg,
        }
    }

    /// Update the origin block in the traversal stage.
    #[cfg(feature = "metrics")]
    fn update_origin(&mut self, block: BlockInfo) {
        self.done = false;
        self.block = Some(block);
        base_macros::set!(gauge, crate::metrics::Metrics::PIPELINE_ORIGIN, block.number as f64);
    }

    /// Update the origin block in the traversal stage.
    #[cfg(not(feature = "metrics"))]
    const fn update_origin(&mut self, block: BlockInfo) {
        self.done = false;
        self.block = Some(block);
    }

    /// Provide the next block to the traversal stage.
    async fn provide_next_block(&mut self, block_info: BlockInfo) -> PipelineResult<()> {
        if !self.done {
            debug!(target: "traversal", "Not finished consuming block, ignoring provided block.");
            return Ok(());
        }
        let Some(block) = self.block else {
            return Err(PipelineError::MissingOrigin.temp());
        };
        if block.number + 1 != block_info.number {
            // Safe to ignore.
            // The next step will exhaust l1 and get the correct next l1 block.
            return Ok(());
        }
        if block.hash != block_info.parent_hash {
            return Err(
                ResetError::NextL1BlockHashMismatch(block.hash, block_info.parent_hash).reset()
            );
        }

        // Fetch receipts for the next l1 block and update the system config.
        let receipts =
            self.data_source.receipts_by_hash(block_info.hash).await.map_err(Into::into)?;

        super::update_system_config_with_receipts(
            &mut self.system_config,
            &receipts,
            self.rollup_config.l1_system_config_address,
            self.rollup_config.is_ecotone_active(block_info.timestamp),
            block_info.number,
        );

        // Update the origin block.
        self.update_origin(block_info);

        Ok(())
    }
}

#[async_trait]
impl<F: ChainProvider + Send> OriginAdvancer for IndexedTraversal<F> {
    async fn advance_origin(&mut self) -> PipelineResult<()> {
        if !self.done {
            debug!(target: "traversal", "Not finished consuming block, ignoring advance call.");
            return Ok(());
        }
        return Err(PipelineError::Eof.temp());
    }
}

impl<F: ChainProvider> OriginProvider for IndexedTraversal<F> {
    fn origin(&self) -> Option<BlockInfo> {
        self.block
    }
}

#[async_trait]
impl<F: ChainProvider + Send> SignalReceiver for IndexedTraversal<F> {
    async fn signal(&mut self, signal: Signal) -> PipelineResult<()> {
        match signal {
            Signal::Reset(ResetSignal { l1_origin, system_config, .. })
            | Signal::Activation(ActivationSignal { l1_origin, system_config, .. }) => {
                self.update_origin(l1_origin);
                self.system_config = system_config.expect("System config must be provided.");
            }
            Signal::ProvideBlock(block_info) => self.provide_next_block(block_info).await?,
            _ => {}
        }

        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use alloc::vec;

    use alloy_consensus::Receipt;
    use alloy_primitives::{B256, Bytes, Log, LogData, U256, address, b256};
    use base_consensus_genesis::{CONFIG_UPDATE_EVENT_VERSION_0, CONFIG_UPDATE_TOPIC};

    use super::*;
    use crate::{errors::PipelineErrorKind, test_utils::TestChainProvider};

    const L1_SYS_CONFIG_ADDR: Address = address!("1337000000000000000000000000000000000000");

    fn new_update_batcher_log_with_addr(addr: Address) -> Log {
        let mut addr_word = [0u8; 32];
        addr_word[12..32].copy_from_slice(addr.as_slice());
        let mut data = alloc::vec::Vec::with_capacity(96);
        data.extend_from_slice(U256::from(0x20).to_be_bytes::<32>().as_slice());
        data.extend_from_slice(U256::from(0x20).to_be_bytes::<32>().as_slice());
        data.extend_from_slice(&addr_word);
        Log {
            address: L1_SYS_CONFIG_ADDR,
            data: LogData::new_unchecked(
                vec![CONFIG_UPDATE_TOPIC, CONFIG_UPDATE_EVENT_VERSION_0, B256::ZERO],
                data.into(),
            ),
        }
    }

    fn new_update_batcher_log() -> Log {
        new_update_batcher_log_with_addr(address!("000000000000000000000000000000000000beef"))
    }

    fn new_receipts() -> alloc::vec::Vec<Receipt> {
        let mut receipt =
            Receipt { status: alloy_consensus::Eip658Value::Eip658(true), ..Receipt::default() };
        let bad = Log::new(
            Address::from([2; 20]),
            vec![CONFIG_UPDATE_TOPIC, B256::default()],
            Bytes::default(),
        )
        .unwrap();
        receipt.logs = vec![new_update_batcher_log(), bad, new_update_batcher_log()];
        vec![receipt.clone(), Receipt::default(), receipt]
    }

    fn new_test_managed(
        blocks: alloc::vec::Vec<BlockInfo>,
        receipts: alloc::vec::Vec<Receipt>,
    ) -> IndexedTraversal<TestChainProvider> {
        let mut provider = TestChainProvider::default();
        let rollup_config = RollupConfig {
            l1_system_config_address: L1_SYS_CONFIG_ADDR,
            ..RollupConfig::default()
        };
        for (i, block) in blocks.iter().enumerate() {
            provider.insert_block(i as u64, *block);
        }
        for (i, receipt) in receipts.iter().enumerate() {
            let hash = blocks.get(i).map(|b| b.hash).unwrap_or_default();
            provider.insert_receipts(hash, vec![receipt.clone()]);
        }
        IndexedTraversal::new(provider, Arc::new(rollup_config))
    }

    fn new_populated_test_managed() -> IndexedTraversal<TestChainProvider> {
        let blocks = vec![BlockInfo::default(), BlockInfo::default()];
        let receipts = new_receipts();
        new_test_managed(blocks, receipts)
    }

    #[test]
    fn test_managed_traversal_batcher_address() {
        let mut traversal = new_populated_test_managed();
        traversal.system_config.batcher_address = L1_SYS_CONFIG_ADDR;
        assert_eq!(traversal.batcher_addr(), L1_SYS_CONFIG_ADDR);
    }

    #[tokio::test]
    async fn test_managed_traversal_activation_signal() {
        let blocks = vec![BlockInfo::default(), BlockInfo::default()];
        let receipts = new_receipts();
        let mut traversal = new_test_managed(blocks, receipts);
        let cfg = SystemConfig::default();
        traversal.done = true;
        assert!(
            traversal
                .signal(Signal::Activation(ActivationSignal {
                    system_config: Some(cfg),
                    ..Default::default()
                }))
                .await
                .is_ok()
        );
        assert_eq!(traversal.origin(), Some(BlockInfo::default()));
        assert_eq!(traversal.system_config, cfg);
        assert!(!traversal.done);
    }

    #[tokio::test]
    async fn test_managed_traversal_reset_signal() {
        let blocks = vec![BlockInfo::default(), BlockInfo::default()];
        let receipts = new_receipts();
        let mut traversal = new_test_managed(blocks, receipts);
        let cfg = SystemConfig::default();
        traversal.done = true;
        assert!(
            traversal
                .signal(Signal::Reset(ResetSignal {
                    system_config: Some(cfg),
                    ..Default::default()
                }))
                .await
                .is_ok()
        );
        assert_eq!(traversal.origin(), Some(BlockInfo::default()));
        assert_eq!(traversal.system_config, cfg);
        assert!(!traversal.done);
    }

    #[tokio::test]
    async fn test_managed_traversal_next_l1_block() {
        let blocks = vec![BlockInfo::default(), BlockInfo::default()];
        let receipts = new_receipts();
        let mut traversal = new_test_managed(blocks, receipts);
        assert_eq!(traversal.next_l1_block().await.unwrap(), Some(BlockInfo::default()));
        assert_eq!(traversal.next_l1_block().await.unwrap_err(), PipelineError::Eof.temp());
    }

    #[tokio::test]
    async fn test_managed_traversal_missing_receipts() {
        let blocks = vec![BlockInfo::default(), BlockInfo::default()];
        let mut traversal = new_test_managed(blocks, vec![]);
        assert_eq!(traversal.next_l1_block().await.unwrap(), Some(BlockInfo::default()));
        assert_eq!(traversal.next_l1_block().await.unwrap_err(), PipelineError::Eof.temp());
        // provide_next_block will fail due to missing receipts
        let next_block = BlockInfo { number: 1, ..BlockInfo::default() };
        let err = traversal.provide_next_block(next_block).await.unwrap_err();
        matches!(err, PipelineErrorKind::Temporary(PipelineError::Provider(_)));
    }

    #[tokio::test]
    async fn test_managed_traversal_reorgs() {
        let hash = b256!("3333333333333333333333333333333333333333333333333333333333333333");
        let block = BlockInfo { hash, number: 0, ..BlockInfo::default() };
        let blocks = vec![block];
        let receipts = new_receipts();
        let mut traversal = new_test_managed(blocks, receipts);
        traversal.block = Some(block);
        assert_eq!(traversal.next_l1_block().await.unwrap(), Some(block));
        // provide_next_block will fail due to hash mismatch (simulate reorg)
        let next_block = BlockInfo { number: 1, ..BlockInfo::default() };
        let err = traversal.provide_next_block(next_block).await.unwrap_err();
        assert_eq!(err, ResetError::NextL1BlockHashMismatch(hash, next_block.parent_hash).reset());
    }

    #[tokio::test]
    async fn test_managed_traversal_missing_blocks() {
        let mut traversal = new_test_managed(vec![], vec![]);
        assert_eq!(traversal.next_l1_block().await.unwrap(), Some(BlockInfo::default()));
        assert_eq!(traversal.next_l1_block().await.unwrap_err(), PipelineError::Eof.temp());
        // provide_next_block will fail due to missing origin
        let next_block = BlockInfo { number: 1, ..BlockInfo::default() };
        let err = traversal.provide_next_block(next_block).await.unwrap_err();
        matches!(err, PipelineErrorKind::Temporary(PipelineError::MissingOrigin));
    }

    #[tokio::test]
    async fn test_managed_traversal_system_config_update_fails() {
        let zero = b256!("0000000000000000000000000000000000000000000000000000000000000000");
        let first = b256!("3333333333333333333333333333333333333333333333333333333333333333");
        let second = b256!("4444444444444444444444444444444444444444444444444444444444444444");
        let block1 = BlockInfo { hash: first, parent_hash: zero, ..BlockInfo::default() };
        let block2 =
            BlockInfo { number: 1, hash: second, parent_hash: first, ..BlockInfo::default() };
        let blocks = vec![block1, block2];
        let receipts = new_receipts();
        let mut traversal = new_test_managed(blocks, receipts);
        traversal.block = Some(block1);
        assert_eq!(traversal.next_l1_block().await.unwrap(), Some(block1));
        // System config update failure is non-fatal — pipeline continues.
        assert!(traversal.provide_next_block(block2).await.is_ok());
    }

    #[tokio::test]
    async fn test_managed_traversal_system_config_updated() {
        let blocks = vec![BlockInfo::default(), BlockInfo::default()];
        let receipts = new_receipts();
        let mut traversal = new_test_managed(blocks, receipts);
        assert_eq!(traversal.next_l1_block().await.unwrap(), Some(BlockInfo::default()));
        assert_eq!(traversal.next_l1_block().await.unwrap_err(), PipelineError::Eof.temp());
        // provide_next_block should update system config
        let next_block = BlockInfo { number: 1, ..BlockInfo::default() };
        assert!(traversal.provide_next_block(next_block).await.is_ok());
        let expected = address!("000000000000000000000000000000000000bEEF");
        assert_eq!(traversal.system_config.batcher_address, expected);
        assert_eq!(traversal.batcher_addr(), expected);
    }

    /// Helper to create a `ConfigUpdate` log for `TYPE_GAS_LIMIT` (update type 0x02).
    fn new_update_gas_limit_log(gas_limit: u64) -> Log {
        let mut update_type = B256::ZERO;
        update_type.0[31] = 0x02;

        // ABI-encode the gas limit: pointer(0x20) + length(0x20) + value(u64 as U256)
        let value = U256::from(gas_limit);
        let mut data = alloc::vec::Vec::with_capacity(96);
        data.extend_from_slice(U256::from(0x20).to_be_bytes::<32>().as_slice());
        data.extend_from_slice(U256::from(0x20).to_be_bytes::<32>().as_slice());
        data.extend_from_slice(value.to_be_bytes::<32>().as_slice());

        Log {
            address: L1_SYS_CONFIG_ADDR,
            data: LogData::new_unchecked(
                vec![CONFIG_UPDATE_TOPIC, CONFIG_UPDATE_EVENT_VERSION_0, update_type],
                data.into(),
            ),
        }
    }

    /// A gas limit `ConfigUpdate` log is processed without error and the pipeline
    /// continues normally.
    #[tokio::test]
    async fn test_gas_limit_update_does_not_disrupt_derivation() {
        let block0 = BlockInfo::default();
        let block1 = BlockInfo { number: 1, ..BlockInfo::default() };

        let gas_limit_log = new_update_gas_limit_log(60_000_000);
        let receipt = Receipt {
            status: alloy_consensus::Eip658Value::Eip658(true),
            logs: vec![gas_limit_log],
            ..Receipt::default()
        };

        let mut provider = TestChainProvider::default();
        let rollup_config = RollupConfig {
            l1_system_config_address: L1_SYS_CONFIG_ADDR,
            ..RollupConfig::default()
        };
        provider.insert_block(0, block0);
        provider.insert_block(1, block1);
        provider.insert_receipts(block1.hash, vec![receipt]);

        let mut traversal = IndexedTraversal::new(provider, Arc::new(rollup_config));
        traversal.block = Some(block0);

        // Consume the current block.
        assert!(traversal.next_l1_block().await.is_ok());
        assert!(traversal.next_l1_block().await.is_err());

        // Provide the next block with the gas limit update — should succeed.
        assert!(traversal.provide_next_block(block1).await.is_ok());

        // Verify gas_limit was updated.
        assert_eq!(traversal.system_config.gas_limit, 60_000_000);

        // Pipeline state is valid: the origin advanced and the block is ready.
        assert_eq!(traversal.origin(), Some(block1));
        assert!(!traversal.done);
    }

    /// The `batcher_address` field on `SystemConfig` is mutated after a `CONFIG_UPDATE`
    /// log is processed during an L1 epoch change (`provide_next_block`).
    #[tokio::test]
    async fn test_batcher_address_update_applied_on_l1_epoch_change() {
        let new_batcher = address!("00000000000000000000000000000000DeaDBeef");

        let receipt = Receipt {
            status: alloy_consensus::Eip658Value::Eip658(true),
            logs: vec![new_update_batcher_log_with_addr(new_batcher)],
            ..Receipt::default()
        };

        let epoch0_hash = b256!("1111111111111111111111111111111111111111111111111111111111111111");
        let epoch1_hash = b256!("2222222222222222222222222222222222222222222222222222222222222222");
        let block0 = BlockInfo { number: 10, hash: epoch0_hash, ..BlockInfo::default() };
        let block1 =
            BlockInfo { number: 11, hash: epoch1_hash, parent_hash: epoch0_hash, timestamp: 100 };

        let mut provider = TestChainProvider::default();
        let rollup_config = RollupConfig {
            l1_system_config_address: L1_SYS_CONFIG_ADDR,
            ..RollupConfig::default()
        };
        provider.insert_block(10, block0);
        provider.insert_block(11, block1);
        provider.insert_receipts(epoch1_hash, vec![receipt]);

        let mut traversal = IndexedTraversal::new(provider, Arc::new(rollup_config));
        traversal.block = Some(block0);

        // Verify initial batcher_address is the default.
        assert_eq!(traversal.system_config.batcher_address, Address::ZERO);

        // Consume block0.
        assert!(traversal.next_l1_block().await.is_ok());
        assert!(traversal.next_l1_block().await.is_err());

        // Advance to block1 (epoch change) — triggers receipt processing.
        assert!(traversal.provide_next_block(block1).await.is_ok());

        // The system config's batcher_address must now reflect the update.
        assert_eq!(traversal.system_config.batcher_address, new_batcher);
    }

    /// After a `ConfigUpdate` log changes `batcher_address` from A → B, sending a
    /// `Signal::Reset` with a `SystemConfig` containing address A restores the
    /// batcher address back to A. This models L1 reorg rollback behavior.
    #[tokio::test]
    async fn test_reorg_signal_restores_batcher_address() {
        let addr_a = address!("aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa");

        let blocks = vec![BlockInfo::default(), BlockInfo::default()];
        let receipts = new_receipts();
        let mut traversal = new_test_managed(blocks, receipts);

        // Start with batcher_address = ADDR_A.
        traversal.system_config.batcher_address = addr_a;
        assert_eq!(traversal.batcher_addr(), addr_a);

        // Consume the current block so provide_next_block will process.
        assert!(traversal.next_l1_block().await.is_ok());
        assert!(traversal.next_l1_block().await.is_err());

        // Provide the next L1 block carrying a ConfigUpdate that sets batcher to 0xBEEF.
        let next_block = BlockInfo { number: 1, ..BlockInfo::default() };
        assert!(traversal.provide_next_block(next_block).await.is_ok());

        let addr_b = address!("000000000000000000000000000000000000bEEF");
        assert_eq!(traversal.batcher_addr(), addr_b);

        // Simulate L1 reorg: send a Reset signal with a SystemConfig that has ADDR_A.
        let reset_config = SystemConfig { batcher_address: addr_a, ..SystemConfig::default() };
        let signal =
            Signal::Reset(ResetSignal { system_config: Some(reset_config), ..Default::default() });
        assert!(traversal.signal(signal).await.is_ok());

        // After reset, batcher_addr() must be restored to ADDR_A.
        assert_eq!(traversal.batcher_addr(), addr_a);
    }
}
