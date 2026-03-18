//! Shared test utilities: reusable mock stubs for L1/L2 clients and a `test_prover` helper.

use alloy_primitives::{Address, B256, Bytes, U256};
use async_trait::async_trait;
use base_enclave::{
    AccountResult, BlockId, Genesis, GenesisSystemConfig, PerChainConfig, RollupConfig,
};
use base_proof_contracts::{
    AggregateVerifierClient, AnchorRoot, AnchorStateRegistryClient, ContractError,
    DisputeGameFactoryClient, GameAtIndex, GameInfo,
};
use base_proof_primitives::Proposal;
use base_proof_rpc::{
    L1BlockId, L1BlockRef, L1Provider, L2BlockRef, L2Provider, OpBlock, OutputAtBlock,
    RollupProvider, RpcError, RpcResult, SyncStatus,
};

use crate::{error::ProposerError, output_proposer::OutputProposer};

/// Mock L1 client with configurable `block_number()` return.
pub(crate) struct MockL1 {
    pub latest_block_number: u64,
}

#[async_trait]
impl L1Provider for MockL1 {
    async fn block_number(&self) -> RpcResult<u64> {
        Ok(self.latest_block_number)
    }
    async fn header_by_number(&self, _: Option<u64>) -> RpcResult<alloy_rpc_types_eth::Header> {
        Ok(alloy_rpc_types_eth::Header { hash: B256::repeat_byte(0x11), ..Default::default() })
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
    async fn get_balance(&self, _: Address) -> RpcResult<U256> {
        Ok(U256::ZERO)
    }
}

/// Mock L2 client with configurable `block_by_number()` behavior.
pub(crate) struct MockL2 {
    pub block_not_found: bool,
    /// If set, `header_by_number` returns a header with this hash.
    /// Used for reorg detection tests.
    pub canonical_hash: Option<B256>,
}

#[async_trait]
impl L2Provider for MockL2 {
    async fn chain_config(&self) -> RpcResult<serde_json::Value> {
        unimplemented!()
    }
    async fn get_proof(&self, _: Address, _: B256) -> RpcResult<AccountResult> {
        unimplemented!()
    }
    async fn header_by_number(&self, _: Option<u64>) -> RpcResult<alloy_rpc_types_eth::Header> {
        let hash = self.canonical_hash.unwrap_or(B256::repeat_byte(0x30));
        Ok(alloy_rpc_types_eth::Header { hash, ..Default::default() })
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
}

/// Mock rollup client that returns a configurable `SyncStatus`.
pub(crate) struct MockRollupClient {
    pub sync_status: SyncStatus,
}

#[async_trait]
impl RollupProvider for MockRollupClient {
    async fn rollup_config(&self) -> RpcResult<RollupConfig> {
        unimplemented!()
    }
    async fn sync_status(&self) -> RpcResult<SyncStatus> {
        Ok(self.sync_status.clone())
    }
    async fn output_at_block(&self, block_number: u64) -> RpcResult<OutputAtBlock> {
        Ok(OutputAtBlock {
            output_root: B256::repeat_byte(block_number as u8),
            block_ref: test_l2_block_ref(block_number, B256::repeat_byte(block_number as u8)),
        })
    }
}

/// Mock anchor state registry with configurable anchor root.
pub(crate) struct MockAnchorStateRegistry {
    pub anchor_root: AnchorRoot,
}

#[async_trait]
impl AnchorStateRegistryClient for MockAnchorStateRegistry {
    async fn get_anchor_root(&self) -> Result<AnchorRoot, ContractError> {
        Ok(self.anchor_root.clone())
    }
}

/// Mock dispute game factory with configurable game count.
pub(crate) struct MockDisputeGameFactory {
    pub game_count: u64,
}

#[async_trait]
impl DisputeGameFactoryClient for MockDisputeGameFactory {
    async fn game_count(&self) -> Result<u64, ContractError> {
        Ok(self.game_count)
    }
    async fn game_at_index(&self, _: u64) -> Result<GameAtIndex, ContractError> {
        Ok(GameAtIndex { game_type: u32::MAX, timestamp: 0, proxy: Address::ZERO })
    }
    async fn init_bonds(&self, _: u32) -> Result<U256, ContractError> {
        Ok(U256::ZERO)
    }
    async fn game_impls(&self, _: u32) -> Result<Address, ContractError> {
        Ok(Address::ZERO)
    }
}

/// Mock aggregate verifier that returns fixed values.
pub(crate) struct MockAggregateVerifier;

#[async_trait]
impl AggregateVerifierClient for MockAggregateVerifier {
    async fn game_info(&self, _: Address) -> Result<GameInfo, ContractError> {
        Ok(GameInfo { root_claim: B256::ZERO, l2_block_number: 0, parent_index: 0 })
    }
    async fn status(&self, _: Address) -> Result<u8, ContractError> {
        Ok(0)
    }
    async fn zk_prover(&self, _: Address) -> Result<Address, ContractError> {
        Ok(Address::ZERO)
    }
    async fn tee_prover(&self, _: Address) -> Result<Address, ContractError> {
        Ok(Address::ZERO)
    }
    async fn starting_block_number(&self, _: Address) -> Result<u64, ContractError> {
        Ok(0)
    }
    async fn read_block_interval(&self, _: Address) -> Result<u64, ContractError> {
        Ok(512)
    }
    async fn read_intermediate_block_interval(&self, _: Address) -> Result<u64, ContractError> {
        Ok(512)
    }
    async fn intermediate_output_roots(&self, _: Address) -> Result<Vec<B256>, ContractError> {
        Ok(vec![])
    }
}

/// Build a default `PerChainConfig` for tests.
pub(crate) fn test_per_chain_config() -> PerChainConfig {
    PerChainConfig {
        chain_id: U256::from(1),
        genesis: Genesis {
            l1: BlockId { hash: B256::ZERO, number: 0 },
            l2: BlockId { hash: B256::ZERO, number: 0 },
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

pub(crate) fn test_l1_block_ref(number: u64) -> L1BlockRef {
    L1BlockRef { hash: B256::ZERO, number, parent_hash: B256::ZERO, timestamp: 1_000_000 + number }
}

pub(crate) fn test_l2_block_ref(number: u64, hash: B256) -> L2BlockRef {
    L2BlockRef {
        hash,
        number,
        parent_hash: B256::ZERO,
        timestamp: 1_000_000 + number,
        l1origin: L1BlockId { hash: B256::ZERO, number: 100 + number },
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

pub(crate) fn test_anchor_root(block_number: u64) -> AnchorRoot {
    AnchorRoot { root: B256::ZERO, l2_block_number: block_number }
}

pub(crate) fn test_prover() -> crate::prover::Prover {
    use jsonrpsee::http_client::HttpClientBuilder;

    use crate::prover_client::RpcProverClient;

    let client = HttpClientBuilder::default().build("http://localhost:19999").expect("valid URL");
    let prover_client = RpcProverClient::new(client);
    crate::prover::Prover::new(test_per_chain_config(), std::sync::Arc::new(prover_client))
}

/// Mock output proposer that does nothing (returns `Ok(())`).
pub(crate) struct MockOutputProposer;

#[async_trait]
impl OutputProposer for MockOutputProposer {
    async fn propose_output(
        &self,
        _proposal: &Proposal,
        _l2_block_number: u64,
        _parent_index: u32,
        _intermediate_roots: &[B256],
    ) -> Result<(), ProposerError> {
        Ok(())
    }
}
