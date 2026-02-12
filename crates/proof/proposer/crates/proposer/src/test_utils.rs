//! Shared test utilities: reusable mock stubs for L1/L2 clients and a `test_prover` helper.

use std::sync::Arc;

use alloy_primitives::{Address, B256, Bytes, U256};
use async_trait::async_trait;
use op_enclave_core::types::config::{
    BlockId, Genesis, GenesisSystemConfig, PerChainConfig, RollupConfig,
};
use op_enclave_core::{AccountResult, executor::ExecutionWitness};

use crate::enclave::EnclaveClientTrait;
use crate::prover::Prover;
use crate::rpc::{L1Client, L2Client, OpBlock, RpcResult};

/// Minimal mock L1 client (all methods `unimplemented!()`).
pub(crate) struct MockL1;

#[async_trait]
impl L1Client for MockL1 {
    async fn block_number(&self) -> RpcResult<u64> {
        unimplemented!()
    }
    async fn header_by_number(&self, _: Option<u64>) -> RpcResult<alloy_rpc_types_eth::Header> {
        unimplemented!()
    }
    async fn header_by_hash(&self, _: B256) -> RpcResult<alloy_rpc_types_eth::Header> {
        unimplemented!()
    }
    async fn block_receipts(
        &self,
        _: B256,
    ) -> RpcResult<Vec<alloy_rpc_types_eth::TransactionReceipt>> {
        unimplemented!()
    }
    async fn code_at(&self, _: Address, _: Option<u64>) -> RpcResult<Bytes> {
        unimplemented!()
    }
    async fn call_contract(&self, _: Address, _: Bytes, _: Option<u64>) -> RpcResult<Bytes> {
        unimplemented!()
    }
}

/// Minimal mock L2 client (all methods `unimplemented!()`).
pub(crate) struct MockL2;

#[async_trait]
impl L2Client for MockL2 {
    async fn chain_config(&self) -> RpcResult<serde_json::Value> {
        unimplemented!()
    }
    async fn get_proof(&self, _: Address, _: B256) -> RpcResult<AccountResult> {
        unimplemented!()
    }
    async fn header_by_number(&self, _: Option<u64>) -> RpcResult<alloy_rpc_types_eth::Header> {
        unimplemented!()
    }
    async fn block_by_number(&self, _: Option<u64>) -> RpcResult<OpBlock> {
        unimplemented!()
    }
    async fn block_by_hash(&self, _: B256) -> RpcResult<OpBlock> {
        unimplemented!()
    }
    async fn execution_witness(&self, _: u64) -> RpcResult<ExecutionWitness> {
        unimplemented!()
    }
    async fn db_get(&self, _: B256) -> RpcResult<Bytes> {
        unimplemented!()
    }
}

/// Build a `Prover` with mock L1/L2 clients and the given enclave mock.
pub(crate) fn test_prover<E: EnclaveClientTrait>(enclave: E) -> Prover<MockL1, MockL2, E> {
    let config = PerChainConfig {
        chain_id: U256::from(1),
        genesis: Genesis {
            l1: BlockId {
                hash: B256::ZERO,
                number: 0,
            },
            l2: BlockId {
                hash: B256::ZERO,
                number: 0,
            },
            l2_time: 0,
            system_config: GenesisSystemConfig {
                batcher_addr: Address::ZERO,
                overhead: B256::ZERO,
                scalar: B256::ZERO,
                gas_limit: 30_000_000,
            },
        },
        block_time: 2,
        deposit_contract_address: Address::ZERO,
        l1_system_config_address: Address::ZERO,
    };

    Prover::new(
        config,
        RollupConfig::default(),
        Arc::new(MockL1),
        Arc::new(MockL2),
        enclave,
    )
}
