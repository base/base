//! Shared test utilities: reusable mock stubs for L1/L2 clients and a `test_prover` helper.

use std::sync::Arc;

use alloy_primitives::{Address, B256, Bytes, U256};
use async_trait::async_trait;
use op_enclave_core::types::config::{
    BlockId, Genesis, GenesisSystemConfig, PerChainConfig, RollupConfig,
};
use op_enclave_core::{AccountResult, executor::ExecutionWitness};

use crate::ProposerError;
use crate::contracts::{OnchainVerifierClient, OutputProposal};
use crate::enclave::EnclaveClientTrait;
use crate::prover::Prover;
use crate::rpc::{
    L1BlockId, L1BlockRef, L1Client, L2BlockRef, L2Client, OpBlock, RollupClient, RpcError,
    RpcResult, SyncStatus,
};

/// Mock L1 client with configurable `block_number()` return.
pub(crate) struct MockL1 {
    pub latest_block_number: u64,
}

#[async_trait]
impl L1Client for MockL1 {
    async fn block_number(&self) -> RpcResult<u64> {
        Ok(self.latest_block_number)
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

/// Mock L2 client with configurable `block_by_number()` behavior.
pub(crate) struct MockL2 {
    pub block_not_found: bool,
}

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
        if self.block_not_found {
            Err(RpcError::BlockNotFound("mock: no blocks".into()))
        } else {
            unimplemented!()
        }
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

/// Mock rollup client that returns a configurable `SyncStatus`.
#[allow(dead_code)]
pub(crate) struct MockRollupClient {
    pub sync_status: SyncStatus,
}

#[async_trait]
impl RollupClient for MockRollupClient {
    async fn rollup_config(&self) -> RpcResult<RollupConfig> {
        unimplemented!()
    }
    async fn sync_status(&self) -> RpcResult<SyncStatus> {
        Ok(self.sync_status.clone())
    }
}

/// Mock onchain verifier that returns a configurable `OutputProposal`.
#[allow(dead_code)]
pub(crate) struct MockOnchainVerifier {
    pub output_proposal: OutputProposal,
}

#[async_trait]
impl OnchainVerifierClient for MockOnchainVerifier {
    async fn latest_output_proposal(&self) -> Result<OutputProposal, ProposerError> {
        Ok(self.output_proposal.clone())
    }
}

/// Build a default `PerChainConfig` for tests.
pub(crate) fn test_per_chain_config() -> PerChainConfig {
    PerChainConfig {
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
    }
}

/// Build a `Prover` with mock L1/L2 clients and the given enclave mock.
pub(crate) fn test_prover<E: EnclaveClientTrait>(enclave: E) -> Prover<MockL1, MockL2, E> {
    Prover::new(
        test_per_chain_config(),
        RollupConfig::default(),
        Arc::new(MockL1 {
            latest_block_number: 0,
        }),
        Arc::new(MockL2 {
            block_not_found: false,
        }),
        enclave,
    )
}

pub(crate) fn test_l1_block_ref(number: u64) -> L1BlockRef {
    L1BlockRef {
        hash: B256::ZERO,
        number,
        parent_hash: B256::ZERO,
        timestamp: 1_000_000 + number,
    }
}

pub(crate) fn test_l2_block_ref(number: u64, hash: B256) -> L2BlockRef {
    L2BlockRef {
        hash,
        number,
        parent_hash: B256::ZERO,
        timestamp: 1_000_000 + number,
        l1origin: L1BlockId {
            hash: B256::ZERO,
            number: 100 + number,
        },
        sequence_number: 0,
    }
}

pub(crate) fn test_sync_status(safe_number: u64, safe_hash: B256) -> SyncStatus {
    let l1 = test_l1_block_ref(100);
    let l2 = test_l2_block_ref(safe_number, safe_hash);
    SyncStatus {
        current_l1: l1.clone(),
        current_l1_finalized: None,
        head_l1: l1.clone(),
        safe_l1: l1.clone(),
        finalized_l1: l1,
        unsafe_l2: l2.clone(),
        safe_l2: l2.clone(),
        finalized_l2: l2,
        pending_safe_l2: None,
    }
}

pub(crate) fn test_output_proposal(block_number: u64) -> OutputProposal {
    OutputProposal {
        outputRoot: B256::ZERO,
        timestamp: U256::from(1_000_000u64).try_into().unwrap(),
        l2BlockNumber: U256::from(block_number).try_into().unwrap(),
    }
}
