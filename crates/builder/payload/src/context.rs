/// Payload builder context and flashblock extra state.
use std::sync::Arc;

use alloy_consensus::Eip658Value;
use alloy_op_evm::block::receipt_builder::OpReceiptBuilder;
use alloy_primitives::{BlockHash, Bytes};
use alloy_rpc_types_eth::Withdrawals;
use op_alloy_consensus::OpDepositReceipt;
use op_revm::OpSpecId;
use reth_basic_payload_builder::PayloadConfig;
use reth_chainspec::{EthChainSpec, EthereumHardforks};
use reth_evm::{
    ConfigureEvm, Evm, EvmEnv,
    eth::receipt_builder::ReceiptBuilderCtx,
};
use reth_node_api::PayloadBuilderError;
use reth_optimism_chainspec::OpChainSpec;
use reth_optimism_evm::{OpEvmConfig, OpNextBlockEnvAttributes};
use reth_optimism_forks::OpHardforks;
use reth_optimism_node::OpPayloadBuilderAttributes;
use reth_optimism_payload_builder::config::{OpDAConfig, OpGasLimitConfig};
use reth_optimism_primitives::{OpReceipt, OpTransactionSigned};
use reth_payload_builder::PayloadId;
use reth_payload_primitives::PayloadBuilderAttributes;
use reth_primitives::SealedHeader;
use revm::{context_interface::Block, interpreter::as_u64_saturated};
use tokio_util::sync::CancellationToken;
use tracing::error;

use crate::{BuilderMetrics, ExecutionInfo, ExecutionMeteringMode, SharedMeteringProvider};

/// Extra context for flashblock payload building.
///
/// Contains per-flashblock configuration and cumulative limits that advance
/// with each flashblock iteration. Use [`advance`] to produce the context
/// for the next flashblock.
///
/// [`advance`]: FlashblocksExtraCtx::advance
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct FlashblocksExtraCtx {
    /// Current flashblock index
    pub flashblock_index: u64,
    /// Target flashblock count per block
    pub target_flashblock_count: u64,
    /// Cumulative gas target for the current flashblock
    pub target_gas_for_batch: u64,
    /// Cumulative DA bytes target for the current flashblock
    pub target_da_for_batch: Option<u64>,
    /// Cumulative DA footprint target for the current flashblock
    pub target_da_footprint_for_batch: Option<u64>,
    /// Target execution time for the current flashblock in microseconds
    pub target_execution_time_for_batch_us: Option<u128>,
    /// Target state root time for the current flashblock in microseconds
    pub target_state_root_time_for_batch_us: Option<u128>,
    /// Gas limit per flashblock
    pub gas_per_batch: u64,
    /// DA bytes limit per flashblock
    pub da_per_batch: Option<u64>,
    /// DA footprint limit per flashblock
    pub da_footprint_per_batch: Option<u64>,
    /// Execution time limit per flashblock in microseconds
    pub execution_time_per_batch_us: Option<u128>,
    /// State root time limit per flashblock in microseconds
    pub state_root_time_per_batch_us: Option<u128>,
    /// Whether to disable state root calculation for each flashblock
    pub disable_state_root: bool,
}

impl FlashblocksExtraCtx {
    /// Produces the [`FlashblocksExtraCtx`] for the next flashblock.
    ///
    /// Increments the flashblock index and advances all cumulative targets by
    /// their per-batch increments. Execution time is reset (use-it-or-lose-it);
    /// state root time accumulates across the block.
    pub fn advance(&self) -> Self {
        let target_da_for_batch = if let Some(da_limit) = self.da_per_batch {
            if let Some(t) = self.target_da_for_batch {
                Some(t + da_limit)
            } else {
                error!(
                    "Builder end up in faulty invariant, if da_per_batch is set then total_da_per_batch must be set"
                );
                None
            }
        } else {
            self.target_da_for_batch
        };

        let target_da_footprint_for_batch =
            match (self.target_da_footprint_for_batch, self.da_footprint_per_batch) {
                (Some(t), Some(p)) => Some(t + p),
                (t, _) => t,
            };

        let target_state_root_time_for_batch_us =
            match (self.target_state_root_time_for_batch_us, self.state_root_time_per_batch_us) {
                (Some(t), Some(p)) => Some(t + p),
                (t, _) => t,
            };

        Self {
            flashblock_index: self.flashblock_index + 1,
            target_gas_for_batch: self.target_gas_for_batch + self.gas_per_batch,
            target_da_for_batch,
            target_da_footprint_for_batch,
            // execution time is use-it-or-lose-it per flashblock
            target_execution_time_for_batch_us: self.execution_time_per_batch_us,
            target_state_root_time_for_batch_us,
            ..self.clone()
        }
    }
}

/// Container type that holds all the necessities to build a new payload.
#[derive(Debug)]
pub struct OpPayloadBuilderCtx {
    /// The type that knows how to perform system calls and configure the evm.
    pub evm_config: OpEvmConfig,
    /// The DA config for the payload builder
    pub da_config: OpDAConfig,
    /// Gas limit configuration for the payload builder
    pub gas_limit_config: OpGasLimitConfig,
    /// The chainspec
    pub chain_spec: Arc<OpChainSpec>,
    /// How to build the payload.
    pub config: PayloadConfig<OpPayloadBuilderAttributes<OpTransactionSigned>>,
    /// Evm Settings
    pub evm_env: EvmEnv<OpSpecId>,
    /// Block env attributes for the current block.
    pub block_env_attributes: OpNextBlockEnvAttributes,
    /// Marker to check whether the job has been cancelled.
    pub cancel: CancellationToken,
    /// The metrics for the builder
    pub metrics: Arc<BuilderMetrics>,
    /// Extra context for the payload builder
    pub extra: FlashblocksExtraCtx,
    /// Max gas that can be used by a transaction.
    pub max_gas_per_txn: Option<u64>,
    /// Max execution time per transaction in microseconds.
    pub max_execution_time_per_tx_us: Option<u128>,
    /// Max state root calculation time per transaction in microseconds.
    pub max_state_root_time_per_tx_us: Option<u128>,
    /// Flashblock-level execution time budget in microseconds.
    pub flashblock_execution_time_budget_us: Option<u128>,
    /// Block-level state root calculation time budget in microseconds.
    pub block_state_root_time_budget_us: Option<u128>,
    /// Execution metering mode: off, dry-run, or enforce.
    pub execution_metering_mode: ExecutionMeteringMode,
    /// Resource metering provider
    pub metering_provider: SharedMeteringProvider,
}

impl OpPayloadBuilderCtx {
    /// Returns a new context with the given cancellation token.
    pub fn with_cancel(self, cancel: CancellationToken) -> Self {
        Self { cancel, ..self }
    }

    /// Returns a new context with the given extra context.
    pub fn with_extra_ctx(self, extra: FlashblocksExtraCtx) -> Self {
        Self { extra, ..self }
    }

    /// Returns the current flashblock index.
    pub const fn flashblock_index(&self) -> u64 {
        self.extra.flashblock_index
    }

    /// Returns the target flashblock count.
    pub const fn target_flashblock_count(&self) -> u64 {
        self.extra.target_flashblock_count
    }

    /// Returns the parent block the payload will be built on.
    pub fn parent(&self) -> &SealedHeader {
        &self.config.parent_header
    }

    /// Returns the parent hash
    pub fn parent_hash(&self) -> BlockHash {
        self.parent().hash()
    }

    /// Returns the timestamp
    pub fn timestamp(&self) -> u64 {
        self.attributes().timestamp()
    }

    /// Returns the builder attributes.
    pub const fn attributes(&self) -> &OpPayloadBuilderAttributes<OpTransactionSigned> {
        &self.config.attributes
    }

    /// Returns the withdrawals if shanghai is active.
    pub fn withdrawals(&self) -> Option<&Withdrawals> {
        self.chain_spec
            .is_shanghai_active_at_timestamp(self.attributes().timestamp())
            .then(|| &self.attributes().payload_attributes.withdrawals)
    }

    /// Returns the block gas limit to target.
    pub fn block_gas_limit(&self) -> u64 {
        self.gas_limit_config.gas_limit().unwrap_or_else(|| {
            self.attributes().gas_limit.unwrap_or(self.evm_env.block_env.gas_limit)
        })
    }

    /// Returns the block number for the block.
    pub const fn block_number(&self) -> u64 {
        as_u64_saturated!(self.evm_env.block_env.number)
    }

    /// Returns the current base fee
    pub const fn base_fee(&self) -> u64 {
        self.evm_env.block_env.basefee
    }

    /// Returns the current blob gas price.
    pub fn get_blob_gasprice(&self) -> Option<u64> {
        self.evm_env.block_env.blob_gasprice().map(|gasprice| gasprice as u64)
    }

    /// Returns the blob fields for the header.
    ///
    /// After Jovian this will return the cumulative DA bytes * scalar.
    /// After Ecotone this will always return `Some(0)`.
    /// Pre-Ecotone, these fields aren't used.
    pub fn blob_fields(&self, info: &ExecutionInfo) -> (Option<u64>, Option<u64>) {
        if self.is_jovian_active() {
            let scalar =
                info.da_footprint_scalar.expect("Scalar must be defined for Jovian blocks");
            let result = info.cumulative_da_bytes_used * scalar as u64;
            (Some(0), Some(result))
        } else if self.is_ecotone_active() {
            (Some(0), Some(0))
        } else {
            (None, None)
        }
    }

    /// Returns the extra data for the block.
    pub fn extra_data(&self) -> Result<Bytes, PayloadBuilderError> {
        if self.is_jovian_active() {
            self.attributes()
                .get_jovian_extra_data(
                    self.chain_spec.base_fee_params_at_timestamp(
                        self.attributes().payload_attributes.timestamp,
                    ),
                )
                .map_err(PayloadBuilderError::other)
        } else if self.is_holocene_active() {
            self.attributes()
                .get_holocene_extra_data(
                    self.chain_spec.base_fee_params_at_timestamp(
                        self.attributes().payload_attributes.timestamp,
                    ),
                )
                .map_err(PayloadBuilderError::other)
        } else {
            Ok(Default::default())
        }
    }

    /// Returns the current fee settings for transactions from the mempool.
    pub fn best_transaction_attributes(&self) -> reth_transaction_pool::BestTransactionsAttributes {
        reth_transaction_pool::BestTransactionsAttributes::new(
            self.base_fee(),
            self.get_blob_gasprice(),
        )
    }

    /// Returns the unique id for this payload job.
    pub fn payload_id(&self) -> PayloadId {
        self.attributes().payload_id()
    }

    /// Returns true if regolith is active for the payload.
    pub fn is_regolith_active(&self) -> bool {
        self.chain_spec.is_regolith_active_at_timestamp(self.attributes().timestamp())
    }

    /// Returns true if ecotone is active for the payload.
    pub fn is_ecotone_active(&self) -> bool {
        self.chain_spec.is_ecotone_active_at_timestamp(self.attributes().timestamp())
    }

    /// Returns true if canyon is active for the payload.
    pub fn is_canyon_active(&self) -> bool {
        self.chain_spec.is_canyon_active_at_timestamp(self.attributes().timestamp())
    }

    /// Returns true if holocene is active for the payload.
    pub fn is_holocene_active(&self) -> bool {
        self.chain_spec.is_holocene_active_at_timestamp(self.attributes().timestamp())
    }

    /// Returns true if isthmus is active for the payload.
    pub fn is_isthmus_active(&self) -> bool {
        self.chain_spec.is_isthmus_active_at_timestamp(self.attributes().timestamp())
    }

    /// Returns true if jovian is active for the payload.
    pub fn is_jovian_active(&self) -> bool {
        self.chain_spec.is_jovian_active_at_timestamp(self.attributes().timestamp())
    }

    /// Returns the chain id
    pub fn chain_id(&self) -> u64 {
        self.chain_spec.chain_id()
    }

    /// Creates a test context with the given parent and block numbers.
    ///
    /// Builds an [`OpPayloadBuilderCtx`] from minimal defaults — optimism mainnet
    /// chain spec at timestamp 0 (all forks inactive), empty attributes, and
    /// no-op metering. Intended for unit tests that need a live context without
    /// a running node.
    #[cfg(any(test, feature = "test-utils"))]
    pub fn test_ctx_with(parent_number: u64, block_number: u64) -> Self {
        use alloy_consensus::Header as ConsensusHeader;
        use alloy_primitives::{Address, U256};

        let chain_spec = Arc::new(OpChainSpec::optimism_mainnet());
        let evm_config = OpEvmConfig::optimism(Arc::clone(&chain_spec));
        let parent_header = Arc::new(SealedHeader::new(
            ConsensusHeader { number: parent_number, ..Default::default() },
            BlockHash::ZERO,
        ));
        let mut evm_env = EvmEnv::<OpSpecId>::default();
        evm_env.block_env.number = U256::from(block_number);
        let block_env_attributes = OpNextBlockEnvAttributes {
            timestamp: 0,
            suggested_fee_recipient: Address::ZERO,
            prev_randao: BlockHash::ZERO,
            gas_limit: 30_000_000,
            parent_beacon_block_root: None,
            extra_data: Default::default(),
        };
        Self {
            evm_config,
            da_config: OpDAConfig::default(),
            gas_limit_config: OpGasLimitConfig::default(),
            chain_spec,
            config: PayloadConfig::new(parent_header, OpPayloadBuilderAttributes::default()),
            evm_env,
            block_env_attributes,
            cancel: CancellationToken::new(),
            metrics: Arc::new(BuilderMetrics::default()),
            extra: FlashblocksExtraCtx::default(),
            max_gas_per_txn: None,
            max_execution_time_per_tx_us: None,
            max_state_root_time_per_tx_us: None,
            flashblock_execution_time_budget_us: None,
            block_state_root_time_budget_us: None,
            execution_metering_mode: ExecutionMeteringMode::default(),
            metering_provider: Arc::new(crate::NoopMeteringProvider),
        }
    }

    /// Creates a test context with parent number 1 and block number 2.
    #[cfg(any(test, feature = "test-utils"))]
    pub fn test_ctx() -> Self {
        Self::test_ctx_with(1, 2)
    }

    /// Constructs a receipt for the given transaction.
    pub fn build_receipt<E: Evm>(
        &self,
        ctx: ReceiptBuilderCtx<'_, OpTransactionSigned, E>,
        deposit_nonce: Option<u64>,
    ) -> OpReceipt {
        let receipt_builder = self.evm_config.block_executor_factory().receipt_builder();
        match receipt_builder.build_receipt(ctx) {
            Ok(receipt) => receipt,
            Err(ctx) => {
                let receipt = alloy_consensus::Receipt {
                    status: Eip658Value::Eip658(ctx.result.is_success()),
                    cumulative_gas_used: ctx.cumulative_gas_used,
                    logs: ctx.result.into_logs(),
                };

                receipt_builder.build_deposit_receipt(OpDepositReceipt {
                    inner: receipt,
                    deposit_nonce,
                    deposit_receipt_version: self.is_canyon_active().then_some(1),
                })
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn base_extra() -> FlashblocksExtraCtx {
        FlashblocksExtraCtx {
            flashblock_index: 0,
            target_flashblock_count: 4,
            target_gas_for_batch: 1_000_000,
            gas_per_batch: 500_000,
            ..Default::default()
        }
    }

    #[test]
    fn test_advance_increments_index_and_gas() {
        let ctx = base_extra();
        let next = ctx.advance();
        assert_eq!(next.flashblock_index, 1);
        assert_eq!(next.target_gas_for_batch, 1_500_000);
    }

    #[test]
    fn test_advance_multiple_times() {
        let ctx = base_extra();
        let next = ctx.advance().advance().advance();
        assert_eq!(next.flashblock_index, 3);
        assert_eq!(next.target_gas_for_batch, 2_500_000);
    }

    #[test]
    fn test_advance_resets_execution_time() {
        let ctx = FlashblocksExtraCtx {
            execution_time_per_batch_us: Some(5_000),
            target_execution_time_for_batch_us: Some(999),
            ..base_extra()
        };
        let next = ctx.advance();
        // execution time is use-it-or-lose-it: next target = per_batch budget, not accumulated
        assert_eq!(next.target_execution_time_for_batch_us, Some(5_000));
    }

    #[test]
    fn test_advance_accumulates_state_root_time() {
        let ctx = FlashblocksExtraCtx {
            state_root_time_per_batch_us: Some(2_000),
            target_state_root_time_for_batch_us: Some(8_000),
            ..base_extra()
        };
        let next = ctx.advance();
        assert_eq!(next.target_state_root_time_for_batch_us, Some(10_000));
    }

    #[test]
    fn test_advance_state_root_time_none_passthrough() {
        let ctx = FlashblocksExtraCtx {
            state_root_time_per_batch_us: None,
            target_state_root_time_for_batch_us: None,
            ..base_extra()
        };
        let next = ctx.advance();
        assert_eq!(next.target_state_root_time_for_batch_us, None);
    }

    #[test]
    fn test_advance_da_advances() {
        let ctx = FlashblocksExtraCtx {
            da_per_batch: Some(100_000),
            target_da_for_batch: Some(200_000),
            ..base_extra()
        };
        let next = ctx.advance();
        assert_eq!(next.target_da_for_batch, Some(300_000));
    }

    #[test]
    fn test_advance_da_none_when_da_per_batch_none() {
        let ctx = FlashblocksExtraCtx {
            da_per_batch: None,
            target_da_for_batch: None,
            ..base_extra()
        };
        let next = ctx.advance();
        assert_eq!(next.target_da_for_batch, None);
    }

    #[test]
    fn test_advance_da_footprint_accumulates() {
        let ctx = FlashblocksExtraCtx {
            da_footprint_per_batch: Some(50),
            target_da_footprint_for_batch: Some(150),
            ..base_extra()
        };
        let next = ctx.advance();
        assert_eq!(next.target_da_footprint_for_batch, Some(200));
    }

    #[test]
    fn test_advance_da_footprint_none_passthrough() {
        let ctx = FlashblocksExtraCtx {
            da_footprint_per_batch: None,
            target_da_footprint_for_batch: None,
            ..base_extra()
        };
        let next = ctx.advance();
        assert_eq!(next.target_da_footprint_for_batch, None);
    }

    #[test]
    fn test_advance_preserves_static_fields() {
        let ctx = FlashblocksExtraCtx {
            target_flashblock_count: 8,
            disable_state_root: true,
            ..base_extra()
        };
        let next = ctx.advance();
        assert_eq!(next.target_flashblock_count, 8);
        assert!(next.disable_state_root);
    }
}
