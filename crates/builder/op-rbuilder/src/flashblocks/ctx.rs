use std::sync::Arc;

use base_builder_cli::GasLimiterArgs;
use op_revm::OpSpecId;
use reth_basic_payload_builder::PayloadConfig;
use reth_evm::EvmEnv;
use reth_optimism_chainspec::OpChainSpec;
use reth_optimism_evm::{OpEvmConfig, OpNextBlockEnvAttributes};
use reth_optimism_payload_builder::{
    OpPayloadBuilderAttributes,
    config::{OpDAConfig, OpGasLimitConfig},
};
use reth_optimism_primitives::OpTransactionSigned;
use tokio_util::sync::CancellationToken;

use crate::{
    flashblocks::{BuilderConfig, OpPayloadBuilderCtx},
    gas_limiter::{AddressGasLimiter},
    metrics::OpRBuilderMetrics,
    traits::ClientBounds,
    tx_data_store::TxDataStore,
};

#[allow(dead_code)]
#[derive(Debug, Clone)]
pub(super) struct OpPayloadSyncerCtx {
    /// The type that knows how to perform system calls and configure the evm.
    evm_config: OpEvmConfig,
    /// The DA config for the payload builder
    da_config: OpDAConfig,
    /// The chainspec
    chain_spec: Arc<OpChainSpec>,
    /// Max gas that can be used by a transaction.
    max_gas_per_txn: Option<u64>,
    /// The metrics for the builder
    metrics: Arc<OpRBuilderMetrics>,
    /// Unified transaction data store (backrun bundles + resource metering)
    tx_data_store: TxDataStore,
}

#[allow(dead_code)]
impl OpPayloadSyncerCtx {
    pub(super) fn new<Client>(
        client: &Client,
        builder_config: BuilderConfig,
        evm_config: OpEvmConfig,
        metrics: Arc<OpRBuilderMetrics>,
    ) -> eyre::Result<Self>
    where
        Client: ClientBounds,
    {
        let chain_spec = client.chain_spec();
        Ok(Self {
            evm_config,
            da_config: builder_config.da_config.clone(),
            chain_spec,
            max_gas_per_txn: builder_config.max_gas_per_txn,
            metrics,
            tx_data_store: builder_config.tx_data_store,
        })
    }

    pub(super) const fn evm_config(&self) -> &OpEvmConfig {
        &self.evm_config
    }

    pub(super) const fn max_gas_per_txn(&self) -> Option<u64> {
        self.max_gas_per_txn
    }

    pub(super) fn into_op_payload_builder_ctx(
        self,
        payload_config: PayloadConfig<OpPayloadBuilderAttributes<OpTransactionSigned>>,
        evm_env: EvmEnv<OpSpecId>,
        block_env_attributes: OpNextBlockEnvAttributes,
        cancel: CancellationToken,
    ) -> OpPayloadBuilderCtx {
        OpPayloadBuilderCtx {
            evm_config: self.evm_config,
            da_config: self.da_config,
            gas_limit_config: OpGasLimitConfig::default(),
            chain_spec: self.chain_spec,
            config: payload_config,
            evm_env,
            block_env_attributes,
            cancel,
            metrics: self.metrics,
            flashblock_index: 0,
            target_flashblock_count: 0,
            target_gas_for_batch: 0,
            target_da_for_batch: None,
            target_da_footprint_for_batch: None,
            gas_per_batch: 0,
            da_per_batch: None,
            da_footprint_per_batch: None,
            disable_state_root: false,
            max_gas_per_txn: self.max_gas_per_txn,
            address_gas_limiter: AddressGasLimiter::new(GasLimiterArgs::default()),
            tx_data_store: self.tx_data_store,
        }
    }
}
