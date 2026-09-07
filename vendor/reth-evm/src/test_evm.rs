//! Alloy-backed EVM fixture for shared executor, storage, pool, and RPC tests.
//! Network nodes use their own execution configuration; this fixture adapts test blocks and
//! receipts to the generic execution interfaces.

use std::{borrow::Cow, convert::Infallible, sync::Arc};

use alloy_consensus::{Eip658Value, Header, Receipt};
use alloy_eips::Decodable2718;
use alloy_evm::{
    EthEvmFactory,
    eth::{
        EthBlockExecutionCtx, EthBlockExecutorFactory,
        receipt_builder::{ReceiptBuilder, ReceiptBuilderCtx},
    },
};
use alloy_primitives::{Bytes, U256};
use base_common_consensus::{
    BaseBlock as Block, BaseReceipt, BaseTxEnvelope, DepositReceipt, Eip8130Receipt, OpTxType,
};
use base_common_rpc_types_engine::ExecutionData;
use reth_chainspec::{ChainSpec, EthereumHardforks, MAINNET};
use reth_primitives_traits::{
    SealedBlock, SealedHeader, SignedTransaction, constants::MAX_TX_GAS_LIMIT_OSAKA,
};
use reth_storage_errors::any::AnyError;
use revm::{
    context::{BlockEnv, CfgEnv},
    context_interface::block::BlobExcessGasAndPrice,
    primitives::hardfork::SpecId,
};

use crate::{
    ConfigureEngineEvm, ConfigureEvm, Evm, EvmEnv, EvmEnvFor, ExecutableTxIterator,
    ExecutionCtxFor, NextBlockEnvAttributes, SenderRecoveryCache, TestBlockAssembler,
    eth::NextEvmEnvAttributes, noop::NoopEvmConfig,
};

/// Test EVM using Alloy's interpreter and a supplied test fork schedule.
#[derive(Debug, Clone)]
pub struct TestEvmConfig {
    /// Interpreter-backed executor factory.
    pub executor_factory:
        EthBlockExecutorFactory<TestReceiptBuilder, Arc<ChainSpec>, EthEvmFactory>,
    /// Assembler for generated test blocks.
    pub block_assembler: TestBlockAssembler,
    /// Optional recovered-sender cache.
    pub sender_recovery_cache: Option<SenderRecoveryCache>,
}

impl TestEvmConfig {
    /// Creates a test EVM for a supplied fork schedule.
    pub fn new(chain_spec: Arc<ChainSpec>) -> Self {
        Self {
            executor_factory: EthBlockExecutorFactory::new(
                TestReceiptBuilder,
                chain_spec.clone(),
                EthEvmFactory::default(),
            ),
            block_assembler: TestBlockAssembler { chain_spec },
            sender_recovery_cache: None,
        }
    }
}

impl Default for TestEvmConfig {
    fn default() -> Self {
        Self::new(MAINNET.clone())
    }
}

/// Scripted executor using the test EVM's type definitions.
pub type MockEvmConfig = NoopEvmConfig<TestEvmConfig>;

/// Builds compact receipts for shared execution tests.
#[derive(Debug, Clone, Copy, Default)]
pub struct TestReceiptBuilder;

impl ReceiptBuilder for TestReceiptBuilder {
    type Transaction = BaseTxEnvelope;
    type Receipt = BaseReceipt;

    fn build_receipt<E: Evm>(&self, ctx: ReceiptBuilderCtx<'_, OpTxType, E>) -> BaseReceipt {
        let receipt = Receipt {
            status: Eip658Value::Eip658(ctx.result.is_success()),
            cumulative_gas_used: ctx.cumulative_gas_used,
            logs: ctx.result.into_logs(),
        };
        match ctx.tx_type {
            OpTxType::Legacy => BaseReceipt::Legacy(receipt),
            OpTxType::Eip2930 => BaseReceipt::Eip2930(receipt),
            OpTxType::Eip1559 => BaseReceipt::Eip1559(receipt),
            OpTxType::Eip7702 => BaseReceipt::Eip7702(receipt),
            OpTxType::Deposit => BaseReceipt::Deposit(DepositReceipt {
                inner: receipt,
                deposit_nonce: None,
                deposit_receipt_version: None,
            }),
            OpTxType::Eip8130 => BaseReceipt::Eip8130(Eip8130Receipt::new(receipt, Vec::new())),
        }
    }
}

impl ConfigureEvm for TestEvmConfig {
    type Error = Infallible;
    type NextBlockEnvCtx = NextBlockEnvAttributes;
    type BlockExecutorFactory =
        EthBlockExecutorFactory<TestReceiptBuilder, Arc<ChainSpec>, EthEvmFactory>;
    type BlockAssembler = TestBlockAssembler;

    fn block_executor_factory(&self) -> &Self::BlockExecutorFactory {
        &self.executor_factory
    }

    fn block_assembler(&self) -> &Self::BlockAssembler {
        &self.block_assembler
    }

    fn evm_env(&self, header: &Header) -> Result<EvmEnv<SpecId>, Self::Error> {
        Ok(EvmEnv::for_eth_block(
            header,
            self.executor_factory.spec(),
            self.executor_factory.spec().chain().id(),
            self.executor_factory.spec().blob_params_at_timestamp(header.timestamp),
        ))
    }

    fn next_evm_env(
        &self,
        parent: &Header,
        attributes: &NextBlockEnvAttributes,
    ) -> Result<EvmEnv, Self::Error> {
        Ok(EvmEnv::for_eth_next_block(
            parent,
            NextEvmEnvAttributes {
                timestamp: attributes.timestamp,
                suggested_fee_recipient: attributes.suggested_fee_recipient,
                prev_randao: attributes.prev_randao,
                gas_limit: attributes.gas_limit,
                slot_number: attributes.slot_number,
            },
            self.executor_factory
                .spec()
                .next_block_base_fee(parent, attributes.timestamp)
                .unwrap_or_default(),
            self.executor_factory.spec(),
            self.executor_factory.spec().chain().id(),
            self.executor_factory.spec().blob_params_at_timestamp(attributes.timestamp),
        ))
    }

    fn context_for_block<'a>(
        &self,
        block: &'a SealedBlock<Block>,
    ) -> Result<EthBlockExecutionCtx<'a>, Self::Error> {
        Ok(EthBlockExecutionCtx {
            tx_count_hint: Some(block.transaction_count()),
            parent_hash: block.header().parent_hash,
            parent_beacon_block_root: block.header().parent_beacon_block_root,
            ommers: &block.body().ommers,
            withdrawals: block.body().withdrawals.as_ref().map(|w| Cow::Borrowed(w.as_slice())),
            extra_data: block.header().extra_data.clone(),
            slot_number: block.header().slot_number,
        })
    }

    fn context_for_next_block(
        &self,
        parent: &SealedHeader,
        attributes: Self::NextBlockEnvCtx,
    ) -> Result<EthBlockExecutionCtx<'_>, Self::Error> {
        Ok(EthBlockExecutionCtx {
            tx_count_hint: None,
            parent_hash: parent.hash(),
            parent_beacon_block_root: attributes.parent_beacon_block_root,
            ommers: &[],
            withdrawals: attributes.withdrawals.map(|w| Cow::Owned(w.into_inner())),
            extra_data: attributes.extra_data,
            slot_number: attributes.slot_number,
        })
    }
}

impl ConfigureEngineEvm<ExecutionData> for TestEvmConfig {
    fn evm_env_for_payload(&self, payload: &ExecutionData) -> Result<EvmEnvFor<Self>, Self::Error> {
        let timestamp = payload.payload.timestamp();
        let block_number = payload.payload.block_number();

        let blob_params = self.executor_factory.spec().blob_params_at_timestamp(timestamp);
        let spec = alloy_evm::spec_by_timestamp_and_block_number(
            self.executor_factory.spec(),
            timestamp,
            block_number,
        );

        // configure evm env based on parent block
        let mut cfg_env = CfgEnv::new()
            .with_chain_id(self.executor_factory.spec().chain().id())
            .with_spec_and_mainnet_gas_params(spec);

        if let Some(blob_params) = &blob_params {
            cfg_env.set_max_blobs_per_tx(blob_params.max_blobs_per_tx);
        }

        if self.executor_factory.spec().is_osaka_active_at_timestamp(timestamp) {
            cfg_env.tx_gas_limit_cap = Some(MAX_TX_GAS_LIMIT_OSAKA);
        }

        // derive the EIP-4844 blob fees from the header's `excess_blob_gas` and the current
        // blobparams
        let blob_excess_gas_and_price =
            payload.payload.excess_blob_gas().zip(blob_params).map(|(excess_blob_gas, params)| {
                let blob_gasprice = params.calc_blob_fee(excess_blob_gas);
                BlobExcessGasAndPrice { excess_blob_gas, blob_gasprice }
            });

        let block_env = BlockEnv {
            number: U256::from(block_number),
            beneficiary: payload.payload.fee_recipient(),
            timestamp: U256::from(timestamp),
            difficulty: if spec >= SpecId::MERGE {
                U256::ZERO
            } else {
                payload.payload.as_v1().prev_randao.into()
            },
            prevrandao: (spec >= SpecId::MERGE).then(|| payload.payload.as_v1().prev_randao),
            gas_limit: payload.payload.gas_limit(),
            basefee: payload.payload.saturated_base_fee_per_gas(),
            blob_excess_gas_and_price,
            slot_num: 0,
        };

        Ok(EvmEnv { cfg_env, block_env })
    }

    fn context_for_payload<'a>(
        &self,
        payload: &'a ExecutionData,
    ) -> Result<ExecutionCtxFor<'a, Self>, Self::Error> {
        Ok(EthBlockExecutionCtx {
            tx_count_hint: Some(payload.payload.transactions().len()),
            parent_hash: payload.parent_hash(),
            parent_beacon_block_root: payload.sidecar.parent_beacon_block_root(),
            ommers: &[],
            withdrawals: payload
                .payload
                .as_v2()
                .map(|payload| Cow::Borrowed(payload.withdrawals.as_slice())),
            extra_data: payload.payload.as_v1().extra_data.clone(),
            slot_number: None,
        })
    }

    fn tx_iterator_for_payload(
        &self,
        payload: &ExecutionData,
    ) -> Result<impl ExecutableTxIterator<Self>, Self::Error> {
        let txs = payload.payload.transactions().clone();
        let sender_recovery_cache = self.sender_recovery_cache.clone();
        let convert = move |tx: Bytes| {
            let tx = BaseTxEnvelope::decode_2718_exact(tx.as_ref()).map_err(AnyError::new)?;
            let signer = if let Some(cache) = &sender_recovery_cache {
                cache.recover(&tx)
            } else {
                tx.try_recover()
            }
            .map_err(AnyError::new)?;
            Ok::<_, AnyError>(tx.with_signer(signer))
        };

        Ok((txs, convert))
    }
}
