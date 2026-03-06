//! TEE-based nullification proof generation for the challenger.
//!
//! Re-executes a specific intermediate block inside a TEE to produce a
//! nullification proof, proving that a claimed output root is wrong. This
//! mirrors the proposer's use of `execute_stateless()` but for the inverse
//! purpose: demonstrating that an onchain checkpoint is invalid.
//!
//! ## Extension traits
//!
//! - [`ChallengerL2Provider`] — extends [`L2Provider`] with
//!   `debug_executionWitness` support required for proof generation.
//! - [`ChallengerEnclaveClient`] — minimal enclave trait (only
//!   `execute_stateless`, no `aggregate`) to keep the challenger decoupled
//!   from the proposer crate. A blanket impl is provided for
//!   [`EnclaveClient`](base_enclave_client::EnclaveClient).
//!
//! ## Proof format
//!
//! The 130-byte output matches the `AggregateVerifier` contract interface:
//!
//! | Offset | Length | Field |
//! |--------|--------|-------|
//! | 0      | 1      | Proof type (`0x00` = TEE) |
//! | 1      | 32     | L1 origin block hash |
//! | 33     | 32     | L1 origin block number (big-endian `uint256`) |
//! | 65     | 65     | ECDSA signature (v-value adjusted to 27/28) |
//!
//! ## Availability
//!
//! TEE proof generation is gated behind the optional `--tee-endpoint` CLI
//! flag (`CHALLENGER_TEE_ENDPOINT`). When the endpoint is not configured,
//! the orchestrator skips TEE and falls back to ZK proof generation.

use std::sync::Arc;

use alloy_consensus::ReceiptEnvelope;
use alloy_eips::{Typed2718, eip2718::Encodable2718};
use alloy_primitives::{B256, Bytes, U256};
use alloy_rpc_types_eth::TransactionReceipt;
use async_trait::async_trait;
use base_alloy_rpc_types::Transaction as OpTransaction;
use base_enclave::{
    BlockId, ChainConfig, ExecutionWitness, Genesis, GenesisSystemConfig, PerChainConfig, Proposal,
    RollupConfig, l2_block_to_block_info,
};
use base_enclave_client::{ClientError, ExecuteStatelessRequest};
use base_proof_rpc::{L1Provider, L2Provider, OpBlock, RollupProvider, RpcError, RpcResult};
use base_protocol::Predeploys;
use thiserror::Error;
use tracing::info;

/// Length of an ECDSA signature in bytes (r + s + v).
const ECDSA_SIGNATURE_LENGTH: usize = 65;

/// Offset to add to ECDSA v-value (0/1 -> 27/28).
const ECDSA_V_OFFSET: u8 = 27;

/// Proof type byte for TEE proofs (matches `AggregateVerifier.ProofType.TEE`).
const PROOF_TYPE_TEE: u8 = 0;

/// Deposit transaction type identifier (EIP-2718 type byte for OP deposits).
const DEPOSIT_TX_TYPE: u8 = 0x7E;

/// Extension trait for L2 providers that support execution witness retrieval.
///
/// The base `L2Provider` trait does not include `debug_executionWitness`
/// because it is only needed for proof generation, not general L2 queries.
#[async_trait]
pub trait ChallengerL2Provider: L2Provider {
    /// Gets the execution witness for a block via `debug_executionWitness`.
    async fn execution_witness(&self, block_number: u64) -> RpcResult<ExecutionWitness>;
}

/// Trait abstracting the enclave RPC client for testability.
///
/// Only requires `execute_stateless` — the challenger does not need `aggregate`.
#[async_trait]
pub trait ChallengerEnclaveClient: Send + Sync {
    /// Executes stateless block validation in the enclave.
    async fn execute_stateless(
        &self,
        req: ExecuteStatelessRequest,
    ) -> Result<Proposal, ClientError>;
}

/// Blanket implementation for `base_enclave_client::EnclaveClient`.
#[async_trait]
impl ChallengerEnclaveClient for base_enclave_client::EnclaveClient {
    async fn execute_stateless(
        &self,
        req: ExecuteStatelessRequest,
    ) -> Result<Proposal, ClientError> {
        self.execute_stateless(req).await
    }
}

/// Errors that can occur during TEE proof generation.
#[derive(Debug, Error)]
pub enum TeeProofError {
    /// Enclave execution failed.
    #[error("enclave execution failed: {0}")]
    Enclave(#[from] ClientError),

    /// RPC data fetch failed.
    #[error("failed to fetch {context}: {source}")]
    Rpc {
        /// Description of the fetch operation.
        context: &'static str,
        /// The underlying RPC error.
        source: RpcError,
    },

    /// Non-RPC data preparation failed (arithmetic overflows, missing data, derivation errors).
    #[error("data preparation failed: {0}")]
    DataPrep(String),

    /// Proof encoding or data transformation failed.
    #[error("proof encoding failed: {0}")]
    Encoding(String),
}

impl TeeProofError {
    /// Returns `true` if the error is transient and the operation can be retried.
    pub const fn is_retryable(&self) -> bool {
        match self {
            Self::Rpc { source, .. } => source.is_retryable(),
            Self::Enclave(_) | Self::DataPrep(_) | Self::Encoding(_) => false,
        }
    }
}

/// Generates TEE-based nullification proofs for invalid candidate games.
///
/// Re-executes a specific intermediate block inside a TEE to produce a
/// proof that the claimed output root at that checkpoint is wrong.
#[derive(Debug)]
pub struct TeeProofGenerator<E, L1, L2, R> {
    /// Enclave client for stateless block execution.
    enclave_client: Arc<E>,
    /// L1 RPC provider.
    l1_provider: Arc<L1>,
    /// L2 RPC provider (with execution witness support).
    l2_provider: Arc<L2>,
    /// Rollup RPC provider for configuration.
    rollup_provider: Arc<R>,
    /// Keccak256 hash of the TEE image PCR0, used to identify the enclave
    /// image that will produce the proof.
    tee_image_hash: B256,
}

impl<E, L1, L2, R> TeeProofGenerator<E, L1, L2, R>
where
    E: ChallengerEnclaveClient,
    L1: L1Provider,
    L2: ChallengerL2Provider,
    R: RollupProvider,
{
    /// Creates a new TEE proof generator.
    #[must_use]
    pub const fn new(
        enclave_client: Arc<E>,
        l1_provider: Arc<L1>,
        l2_provider: Arc<L2>,
        rollup_provider: Arc<R>,
        tee_image_hash: B256,
    ) -> Self {
        Self { enclave_client, l1_provider, l2_provider, rollup_provider, tee_image_hash }
    }

    /// Generates a TEE nullification proof for an invalid intermediate checkpoint.
    ///
    /// The target block is computed as:
    /// `starting_block_number + (invalid_index + 1) * intermediate_block_interval`
    ///
    /// # Arguments
    ///
    /// * `game` - The candidate game containing the invalid checkpoint
    /// * `invalid_index` - Zero-based index of the first invalid intermediate root
    /// * `intermediate_block_interval` - Number of blocks between checkpoints
    ///
    /// # Returns
    ///
    /// 130-byte proof bytes: `proofType(1) + l1OriginHash(32) + l1OriginNumber(32) + signature(65)`
    ///
    /// # Errors
    ///
    /// Returns [`TeeProofError`] if data fetching, enclave execution, or encoding fails.
    pub async fn generate_tee_proof(
        &self,
        game: &crate::CandidateGame,
        invalid_index: usize,
        intermediate_block_interval: u64,
    ) -> Result<Bytes, TeeProofError> {
        let index_plus_one =
            u64::try_from(invalid_index).ok().and_then(|i| i.checked_add(1)).ok_or_else(|| {
                TeeProofError::DataPrep("arithmetic overflow computing target block".into())
            })?;
        let target_block_number = index_plus_one
            .checked_mul(intermediate_block_interval)
            .and_then(|offset| game.starting_block_number.checked_add(offset))
            .ok_or_else(|| {
                TeeProofError::DataPrep("arithmetic overflow computing target block".into())
            })?;

        info!(
            target_block = %target_block_number,
            game_index = %game.index,
            invalid_index = %invalid_index,
            "generating TEE proof"
        );

        // Fetch the rollup config
        let rollup_config = self
            .rollup_provider
            .rollup_config()
            .await
            .map_err(|e| TeeProofError::Rpc { context: "rollup config", source: e })?;

        // Fetch the target block to get its header and transactions
        let target_block = self
            .l2_provider
            .block_by_number(Some(target_block_number))
            .await
            .map_err(|e| TeeProofError::Rpc { context: "target block", source: e })?;

        let block_hash = target_block.header.hash;

        // Get the first transaction to derive L1 origin info
        let first_tx = target_block
            .transactions
            .txns()
            .next()
            .ok_or_else(|| TeeProofError::DataPrep("no transactions in target block".into()))?;

        let first_tx_bytes = Self::serialize_rpc_transaction(first_tx)?;

        // Derive L2 block info to get L1 origin
        let l2_block_info = l2_block_to_block_info(
            &rollup_config,
            &target_block.header.inner,
            block_hash,
            &first_tx_bytes,
        )
        .map_err(|e| TeeProofError::DataPrep(format!("L2 block info derivation: {e}")))?;

        let l1_origin_hash = l2_block_info.l1_origin.hash;
        let l1_origin_number = l2_block_info.l1_origin.number;

        // Fetch all required data in parallel
        let (
            witness_result,
            msg_account_result,
            prev_block_result,
            prev_msg_account_result,
            l1_origin_result,
            l1_receipts_result,
        ) = tokio::join!(
            self.l2_provider.execution_witness(target_block_number),
            self.l2_provider.get_proof(Predeploys::L2_TO_L1_MESSAGE_PASSER, block_hash),
            self.l2_provider.block_by_hash(target_block.header.parent_hash),
            self.l2_provider
                .get_proof(Predeploys::L2_TO_L1_MESSAGE_PASSER, target_block.header.parent_hash),
            self.l1_provider.header_by_hash(l1_origin_hash),
            self.l1_provider.block_receipts(l1_origin_hash),
        );

        let witness = witness_result
            .map_err(|e| TeeProofError::Rpc { context: "execution witness", source: e })?;
        let msg_account = msg_account_result
            .map_err(|e| TeeProofError::Rpc { context: "message account proof", source: e })?;
        let prev_block = prev_block_result
            .map_err(|e| TeeProofError::Rpc { context: "previous block", source: e })?;
        let prev_msg_account = prev_msg_account_result
            .map_err(|e| TeeProofError::Rpc { context: "previous message account", source: e })?;
        let l1_origin = l1_origin_result
            .map_err(|e| TeeProofError::Rpc { context: "L1 origin header", source: e })?;
        let l1_receipts = l1_receipts_result
            .map_err(|e| TeeProofError::Rpc { context: "L1 receipts", source: e })?;

        // Serialize previous block transactions (all types including deposits)
        let prev_block_txs = Self::serialize_block_transactions(&prev_block, true)?;

        // Serialize current block transactions (excluding deposits)
        let sequenced_txs = Self::serialize_block_transactions(&target_block, false)?;

        let chain_config = Self::build_chain_config(&rollup_config);
        let config_hash = Self::compute_config_hash(&rollup_config);
        let l1_receipt_envelopes = Self::convert_receipts(l1_receipts);

        let request = ExecuteStatelessRequest {
            config: chain_config,
            config_hash,
            l1_origin: l1_origin.inner,
            l1_receipts: l1_receipt_envelopes,
            previous_block_txs: prev_block_txs,
            block_header: target_block.header.inner,
            sequenced_txs,
            witness,
            message_account: msg_account,
            prev_message_account_hash: prev_msg_account.storage_hash,
            // The challenger proves invalidity, it does not propose new outputs.
            // Address::ZERO is the correct value for nullification proofs.
            proposer: alloy_primitives::Address::ZERO,
            tee_image_hash: self.tee_image_hash,
        };

        let proposal =
            self.enclave_client.execute_stateless(request).await.map_err(TeeProofError::Enclave)?;

        let proof_bytes = Self::encode_proof_bytes(&proposal, l1_origin_hash, l1_origin_number)?;

        info!(
            target_block = %target_block_number,
            game_index = %game.index,
            proof_len = %proof_bytes.len(),
            "TEE proof generated"
        );

        Ok(proof_bytes)
    }

    /// Encodes a TEE proposal into the 130-byte proof format.
    ///
    /// Format: `proofType(1) + l1OriginHash(32) + l1OriginNumber(32) + signature(65)`
    fn encode_proof_bytes(
        proposal: &Proposal,
        l1_origin_hash: B256,
        l1_origin_number: u64,
    ) -> Result<Bytes, TeeProofError> {
        let sig = &proposal.signature;
        if sig.len() < ECDSA_SIGNATURE_LENGTH {
            return Err(TeeProofError::Encoding(format!(
                "signature too short: expected at least {ECDSA_SIGNATURE_LENGTH} bytes, got {}",
                sig.len()
            )));
        }

        let mut proof_data = vec![0u8; 1 + 32 + 32 + ECDSA_SIGNATURE_LENGTH];

        // Byte 0: proof type (TEE = 0)
        proof_data[0] = PROOF_TYPE_TEE;

        // Bytes 1-32: L1 origin hash
        proof_data[1..33].copy_from_slice(l1_origin_hash.as_slice());

        // Bytes 33-64: L1 origin number as 32-byte big-endian uint256
        // The u64 is placed in the last 8 bytes of the 32-byte field (bytes 57-64)
        proof_data[57..65].copy_from_slice(&l1_origin_number.to_be_bytes());

        // Bytes 65-129: ECDSA signature with v-value adjusted from 0/1 to 27/28
        proof_data[65..130].copy_from_slice(&sig[..ECDSA_SIGNATURE_LENGTH]);
        proof_data[129] = match proof_data[129] {
            0 | 1 => proof_data[129] + ECDSA_V_OFFSET,
            27 | 28 => proof_data[129],
            v => {
                return Err(TeeProofError::Encoding(format!(
                    "unexpected ECDSA v-value: {v}, expected 0, 1, 27, or 28"
                )));
            }
        };

        Ok(Bytes::from(proof_data))
    }

    /// Serializes an RPC transaction to EIP-2718 encoded bytes.
    fn serialize_rpc_transaction(tx: &OpTransaction) -> Result<Bytes, TeeProofError> {
        let mut buf = Vec::new();
        tx.as_ref().encode_2718(&mut buf);
        Ok(Bytes::from(buf))
    }

    /// Serializes all transactions in a block to EIP-2718 encoded bytes.
    fn serialize_block_transactions(
        block: &OpBlock,
        include_deposits: bool,
    ) -> Result<Vec<Bytes>, TeeProofError> {
        block
            .transactions
            .txns()
            .filter(|tx| include_deposits || tx.ty() != DEPOSIT_TX_TYPE)
            .map(Self::serialize_rpc_transaction)
            .collect()
    }

    /// Converts transaction receipts to receipt envelopes for the enclave.
    fn convert_receipts(receipts: Vec<TransactionReceipt>) -> Vec<ReceiptEnvelope> {
        receipts
            .into_iter()
            .map(|r| {
                r.inner.map_logs(|log| alloy_primitives::Log {
                    address: log.inner.address,
                    data: log.inner.data,
                })
            })
            .collect()
    }

    /// Builds the chain configuration expected by the enclave RPC.
    fn build_chain_config(rollup_config: &RollupConfig) -> ChainConfig {
        let mut config = ChainConfig {
            name: "base-challenger".to_string(),
            l1_chain_id: rollup_config.l1_chain_id,
            public_rpc: String::new(),
            sequencer_rpc: String::new(),
            explorer: String::new(),
            data_availability_type: "eth-da".to_string(),
            chain_id: rollup_config.l2_chain_id.id(),
            batch_inbox_addr: rollup_config.batch_inbox_address,
            block_time: rollup_config.block_time,
            seq_window_size: rollup_config.seq_window_size,
            max_sequencer_drift: rollup_config.max_sequencer_drift,
            gas_paying_token: None,
            protocol_versions_addr: Some(rollup_config.protocol_versions_address),
            hardfork_config: rollup_config.hardforks,
            optimism: Some(rollup_config.chain_op_config),
            genesis: rollup_config.genesis,
            roles: None,
            addresses: Some(Default::default()),
        };

        if let Some(addresses) = config.addresses.as_mut() {
            addresses.address_manager = Some(rollup_config.protocol_versions_address);
            addresses.optimism_portal_proxy = Some(rollup_config.deposit_contract_address);
            addresses.system_config_proxy = Some(rollup_config.l1_system_config_address);
        }

        config
    }

    /// Computes the canonical config hash from the rollup configuration.
    ///
    /// Builds a [`PerChainConfig`] from the rollup config, applies
    /// [`force_defaults()`](PerChainConfig::force_defaults) for deterministic
    /// values, then returns the keccak256 hash — mirroring the proposer's
    /// approach.
    fn compute_config_hash(rollup_config: &RollupConfig) -> B256 {
        let sc = rollup_config.genesis.system_config.as_ref();
        let (batcher_addr, scalar, gas_limit) = sc
            .map(|c| (c.batcher_address, B256::from(c.scalar.to_be_bytes::<32>()), c.gas_limit))
            .unwrap_or_default();

        let mut per_chain = PerChainConfig {
            chain_id: U256::from(rollup_config.l2_chain_id.id()),
            genesis: Genesis {
                l1: BlockId {
                    hash: rollup_config.genesis.l1.hash,
                    number: rollup_config.genesis.l1.number,
                },
                l2: BlockId {
                    hash: rollup_config.genesis.l2.hash,
                    number: rollup_config.genesis.l2.number,
                },
                l2_time: rollup_config.genesis.l2_time,
                system_config: GenesisSystemConfig {
                    batcher_addr,
                    overhead: B256::ZERO,
                    scalar,
                    gas_limit,
                },
            },
            block_time: rollup_config.block_time,
            deposit_contract_address: rollup_config.deposit_contract_address,
            l1_system_config_address: rollup_config.l1_system_config_address,
        };
        per_chain.force_defaults();
        per_chain.hash()
    }
}

#[cfg(test)]
mod tests {
    use std::{collections::HashMap, sync::Mutex};

    use alloy_consensus::{
        Header as ConsensusHeader, Receipt, ReceiptWithBloom, Sealable, Signed, TxEip1559,
    };
    use alloy_primitives::{
        Address, LogData, Signature as PrimitiveSignature, TxKind, U256, address, b256,
    };
    use alloy_rpc_types_eth::{Block, BlockTransactions, Header as RpcHeader};
    use base_alloy_consensus::{OpTxEnvelope, TxDeposit};
    use base_enclave::{AccountResult, default_rollup_config};
    use base_proof_contracts::{GameAtIndex, GameInfo};
    use base_protocol::L1BlockInfoBedrock;
    use rstest::rstest;

    use super::*;
    use crate::CandidateGame;

    /// Concrete type alias for tests so associated functions are callable.
    type TestGenerator = TeeProofGenerator<
        MockEnclaveClient,
        MockL1Provider,
        MockChallengerL2Provider,
        MockRollupProvider,
    >;

    // ========================================================================
    // Mock types
    // ========================================================================

    /// Mock enclave client for testing.
    ///
    /// Optionally captures the last request for assertion in integration tests.
    #[derive(Debug)]
    struct MockEnclaveClient {
        result: Result<Proposal, ClientError>,
        captured_request: Mutex<Option<ExecuteStatelessRequest>>,
    }

    impl MockEnclaveClient {
        /// Creates a mock that returns the given result without capturing.
        fn new(result: Result<Proposal, ClientError>) -> Self {
            Self { result, captured_request: Mutex::new(None) }
        }
    }

    #[async_trait]
    impl ChallengerEnclaveClient for MockEnclaveClient {
        async fn execute_stateless(
            &self,
            req: ExecuteStatelessRequest,
        ) -> Result<Proposal, ClientError> {
            *self.captured_request.lock().unwrap() = Some(req);
            match &self.result {
                Ok(p) => Ok(p.clone()),
                Err(e) => Err(ClientError::ClientCreation(e.to_string())),
            }
        }
    }

    /// Mock L1 provider.
    #[derive(Debug)]
    struct MockL1Provider {
        headers: HashMap<B256, RpcHeader>,
        receipts: HashMap<B256, Vec<TransactionReceipt>>,
    }

    #[async_trait]
    impl L1Provider for MockL1Provider {
        async fn block_number(&self) -> RpcResult<u64> {
            Ok(0)
        }

        async fn header_by_number(&self, _number: Option<u64>) -> RpcResult<RpcHeader> {
            Err(RpcError::BlockNotFound("not implemented".into()))
        }

        async fn header_by_hash(&self, hash: B256) -> RpcResult<RpcHeader> {
            self.headers
                .get(&hash)
                .cloned()
                .ok_or_else(|| RpcError::HeaderNotFound(format!("no header for {hash}")))
        }

        async fn block_receipts(&self, hash: B256) -> RpcResult<Vec<TransactionReceipt>> {
            self.receipts
                .get(&hash)
                .cloned()
                .ok_or_else(|| RpcError::BlockNotFound(format!("no receipts for {hash}")))
        }

        async fn code_at(&self, _address: Address, _block_number: Option<u64>) -> RpcResult<Bytes> {
            Ok(Bytes::new())
        }

        async fn call_contract(
            &self,
            _to: Address,
            _data: Bytes,
            _block_number: Option<u64>,
        ) -> RpcResult<Bytes> {
            Ok(Bytes::new())
        }

        async fn get_balance(&self, _address: Address) -> RpcResult<U256> {
            Ok(U256::ZERO)
        }
    }

    /// Mock L2 provider with execution witness support.
    #[derive(Debug)]
    struct MockChallengerL2Provider {
        blocks: HashMap<u64, OpBlock>,
        blocks_by_hash: HashMap<B256, OpBlock>,
        proofs: HashMap<B256, AccountResult>,
        witness: Option<ExecutionWitness>,
        error: Option<String>,
    }

    impl MockChallengerL2Provider {
        fn new() -> Self {
            Self {
                blocks: HashMap::new(),
                blocks_by_hash: HashMap::new(),
                proofs: HashMap::new(),
                witness: None,
                error: None,
            }
        }
    }

    #[async_trait]
    impl L2Provider for MockChallengerL2Provider {
        async fn chain_config(&self) -> RpcResult<serde_json::Value> {
            Ok(serde_json::Value::Null)
        }

        async fn get_proof(&self, _address: Address, block_hash: B256) -> RpcResult<AccountResult> {
            self.proofs
                .get(&block_hash)
                .cloned()
                .ok_or_else(|| RpcError::ProofNotFound(format!("no proof for {block_hash}")))
        }

        async fn header_by_number(&self, _number: Option<u64>) -> RpcResult<RpcHeader> {
            Err(RpcError::BlockNotFound("not implemented".into()))
        }

        async fn block_by_number(&self, number: Option<u64>) -> RpcResult<OpBlock> {
            if let Some(err) = &self.error {
                return Err(RpcError::BlockNotFound(err.clone()));
            }
            let n = number.unwrap_or(0);
            self.blocks
                .get(&n)
                .cloned()
                .ok_or_else(|| RpcError::BlockNotFound(format!("block {n} not found")))
        }

        async fn block_by_hash(&self, hash: B256) -> RpcResult<OpBlock> {
            self.blocks_by_hash
                .get(&hash)
                .cloned()
                .ok_or_else(|| RpcError::BlockNotFound(format!("block {hash} not found")))
        }
    }

    #[async_trait]
    impl ChallengerL2Provider for MockChallengerL2Provider {
        async fn execution_witness(&self, _block_number: u64) -> RpcResult<ExecutionWitness> {
            self.witness.clone().ok_or_else(|| RpcError::BlockNotFound("no witness".into()))
        }
    }

    /// Mock rollup provider.
    #[derive(Debug)]
    struct MockRollupProvider {
        config: Option<RollupConfig>,
    }

    #[async_trait]
    impl RollupProvider for MockRollupProvider {
        async fn rollup_config(&self) -> RpcResult<RollupConfig> {
            self.config.clone().ok_or_else(|| RpcError::BlockNotFound("no config".into()))
        }

        async fn sync_status(&self) -> RpcResult<base_proof_rpc::SyncStatus> {
            Err(RpcError::BlockNotFound("not implemented".into()))
        }
    }

    fn test_candidate_game(starting_block: u64) -> CandidateGame {
        let proxy = address!("0000000000000000000000000000000000000001");
        CandidateGame {
            index: 42,
            factory: GameAtIndex { game_type: 1, timestamp: 1_000_000, proxy },
            info: GameInfo {
                root_claim: B256::repeat_byte(0xAA),
                l2_block_number: starting_block.saturating_add(100),
                parent_index: 0,
            },
            starting_block_number: starting_block,
        }
    }

    /// Creates a deposit transaction carrying L1 block info (Bedrock format).
    ///
    /// The returned `OpTransaction` can be placed as the first transaction in an
    /// `OpBlock` so that `l2_block_to_block_info` can derive the L1 origin.
    fn make_deposit_tx(l1_hash: B256, l1_number: u64) -> OpTransaction {
        let l1_info = L1BlockInfoBedrock::new(
            l1_number,
            1_700_000_000, // timestamp
            1_000_000_000, // base_fee
            l1_hash,
            0,                  // sequence_number
            Address::ZERO,      // batcher_address
            U256::ZERO,         // l1_fee_overhead
            U256::from(684000), // l1_fee_scalar
        );
        let calldata = base_protocol::L1BlockInfoTx::Bedrock(l1_info).encode_calldata();

        let deposit = TxDeposit {
            source_hash: B256::repeat_byte(0x01),
            from: address!("DeaDDEaDDeAdDeAdDEAdDEaddeAddEAdDEAd0001"),
            to: TxKind::Call(address!("4200000000000000000000000000000000000015")),
            mint: 0,
            value: U256::ZERO,
            gas_limit: 1_000_000,
            is_system_transaction: true,
            input: calldata,
        };

        let sealed = deposit.seal_slow();
        let envelope = OpTxEnvelope::Deposit(sealed);

        OpTransaction {
            inner: alloy_rpc_types_eth::Transaction {
                inner: alloy_consensus::transaction::Recovered::new_unchecked(
                    envelope,
                    Address::ZERO,
                ),
                block_hash: None,
                block_number: None,
                transaction_index: Some(0),
                effective_gas_price: Some(0),
            },
            deposit_nonce: Some(0),
            deposit_receipt_version: None,
        }
    }

    /// Builds a minimal `OpBlock` containing only a deposit transaction.
    fn make_test_block(
        block_number: u64,
        parent_hash: B256,
        l1_origin_hash: B256,
        l1_origin_number: u64,
    ) -> OpBlock {
        let deposit_tx = make_deposit_tx(l1_origin_hash, l1_origin_number);
        let consensus_header = ConsensusHeader {
            parent_hash,
            number: block_number,
            timestamp: 1_700_000_000 + block_number * 2,
            ..Default::default()
        };
        let block_hash = consensus_header.hash_slow();
        let rpc_header =
            RpcHeader { hash: block_hash, inner: consensus_header, ..Default::default() };

        Block {
            header: rpc_header,
            uncles: vec![],
            transactions: BlockTransactions::Full(vec![deposit_tx]),
            withdrawals: None,
        }
    }

    /// Creates a minimal `AccountResult` for testing.
    fn mock_account_result() -> AccountResult {
        AccountResult {
            address: Predeploys::L2_TO_L1_MESSAGE_PASSER,
            account_proof: vec![],
            balance: U256::ZERO,
            code_hash: b256!("c5d2460186f7233c927e7db2dcc703c0e500b653ca82273b7bfad8045d85a470"),
            nonce: U256::ZERO,
            storage_hash: B256::repeat_byte(0x01),
            storage_proof: vec![],
        }
    }

    /// Builds fully-wired test fixtures: mock providers populated with
    /// consistent block, header, proof, and witness data.
    ///
    /// Returns `(l2_provider, l1_provider, rollup_provider, l1_origin_hash)`.
    fn wired_providers(
        target_block_number: u64,
    ) -> (MockChallengerL2Provider, MockL1Provider, MockRollupProvider, B256) {
        let l1_origin_hash = B256::repeat_byte(0xEE);
        let l1_origin_number = 500u64;

        // Target block (the one we re-execute)
        let parent_hash = B256::repeat_byte(0x22);
        let target_block =
            make_test_block(target_block_number, parent_hash, l1_origin_hash, l1_origin_number);
        let target_hash = target_block.header.hash;

        // Previous block (parent of target)
        let prev_block = make_test_block(
            target_block_number - 1,
            B256::repeat_byte(0x33),
            l1_origin_hash,
            l1_origin_number,
        );

        let account = mock_account_result();
        let witness = ExecutionWitness {
            headers: vec![prev_block.header.inner.clone()],
            codes: HashMap::new(),
            state: HashMap::new(),
        };

        let l1_header = RpcHeader {
            hash: l1_origin_hash,
            inner: ConsensusHeader { number: l1_origin_number, ..Default::default() },
            ..Default::default()
        };

        let mut l2 = MockChallengerL2Provider::new();
        l2.blocks.insert(target_block_number, target_block);
        l2.blocks_by_hash.insert(parent_hash, prev_block);
        l2.proofs.insert(target_hash, account.clone());
        l2.proofs.insert(parent_hash, account);
        l2.witness = Some(witness);

        let mut l1_headers = HashMap::new();
        l1_headers.insert(l1_origin_hash, l1_header);
        let mut l1_receipts = HashMap::new();
        l1_receipts.insert(l1_origin_hash, vec![]);

        let l1 = MockL1Provider { headers: l1_headers, receipts: l1_receipts };

        let rollup = MockRollupProvider { config: Some(default_rollup_config()) };

        (l2, l1, rollup, l1_origin_hash)
    }

    // ========================================================================
    // Proof encoding tests
    // ========================================================================

    fn test_proposal(l1_hash: B256, l1_number: u64) -> Proposal {
        // Build a 65-byte signature with v=0 (last byte) for valid encoding
        let mut sig = vec![0xAB; 65];
        sig[64] = 0; // v-value = 0 (will be adjusted to 27)
        Proposal {
            output_root: B256::repeat_byte(0x11),
            signature: Bytes::from(sig),
            l1_origin_hash: l1_hash,
            l1_origin_number: U256::from(l1_number),
            l2_block_number: U256::from(100u64),
            prev_output_root: B256::ZERO,
            config_hash: B256::ZERO,
        }
    }

    #[test]
    fn test_encode_proof_bytes_length() {
        let proposal = test_proposal(B256::repeat_byte(0xCC), 500);
        let proof =
            TestGenerator::encode_proof_bytes(&proposal, B256::repeat_byte(0xCC), 500).unwrap();
        assert_eq!(proof.len(), 130);
    }

    #[test]
    fn test_encode_proof_bytes_type() {
        let proposal = test_proposal(B256::repeat_byte(0xCC), 500);
        let proof =
            TestGenerator::encode_proof_bytes(&proposal, B256::repeat_byte(0xCC), 500).unwrap();
        assert_eq!(proof[0], PROOF_TYPE_TEE);
    }

    #[test]
    fn test_encode_proof_bytes_l1_origin_hash() {
        let l1_hash = B256::repeat_byte(0xDD);
        let proposal = test_proposal(l1_hash, 500);
        let proof = TestGenerator::encode_proof_bytes(&proposal, l1_hash, 500).unwrap();
        assert_eq!(&proof[1..33], l1_hash.as_slice());
    }

    #[test]
    fn test_encode_proof_bytes_l1_origin_number() {
        let proposal = test_proposal(B256::ZERO, 12345);
        let proof = TestGenerator::encode_proof_bytes(&proposal, B256::ZERO, 12345).unwrap();
        // Leading 24 bytes of the uint256 field (bytes 33-56) must be zero padding
        assert_eq!(&proof[33..57], &[0u8; 24]);
        // u64 is placed in bytes 57-64 (last 8 bytes of the 32-byte field)
        let mut expected = [0u8; 8];
        expected.copy_from_slice(&proof[57..65]);
        assert_eq!(u64::from_be_bytes(expected), 12345);
    }

    #[rstest]
    #[case::v_zero(0, 27)]
    #[case::v_one(1, 28)]
    #[case::v_27(27, 27)]
    #[case::v_28(28, 28)]
    fn test_encode_proof_bytes_v_adjustment(#[case] input_v: u8, #[case] expected_v: u8) {
        let mut proposal = test_proposal(B256::ZERO, 0);
        let mut sig = proposal.signature.to_vec();
        sig[64] = input_v;
        proposal.signature = Bytes::from(sig);

        let proof = TestGenerator::encode_proof_bytes(&proposal, B256::ZERO, 0).unwrap();
        assert_eq!(proof[129], expected_v);
    }

    #[test]
    fn test_encode_proof_bytes_invalid_v() {
        let mut proposal = test_proposal(B256::ZERO, 0);
        let mut sig = proposal.signature.to_vec();
        sig[64] = 5;
        proposal.signature = Bytes::from(sig);

        let result = TestGenerator::encode_proof_bytes(&proposal, B256::ZERO, 0);
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("unexpected ECDSA v-value"));
    }

    #[test]
    fn test_encode_proof_bytes_short_signature() {
        let mut proposal = test_proposal(B256::ZERO, 0);
        proposal.signature = Bytes::from(vec![0u8; 32]);

        let result = TestGenerator::encode_proof_bytes(&proposal, B256::ZERO, 0);
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("signature too short"));
    }

    // ========================================================================
    // Target block overflow tests
    // ========================================================================

    /// Creates a generator with empty/no-op providers for arithmetic-only tests.
    fn generator_with_no_data() -> TestGenerator {
        TeeProofGenerator::new(
            Arc::new(MockEnclaveClient::new(Ok(test_proposal(B256::ZERO, 0)))),
            Arc::new(MockL1Provider { headers: HashMap::new(), receipts: HashMap::new() }),
            Arc::new(MockChallengerL2Provider::new()),
            Arc::new(MockRollupProvider { config: None }),
            B256::ZERO,
        )
    }

    #[tokio::test]
    async fn test_generate_tee_proof_index_overflow() {
        // invalid_index = usize::MAX → try_from succeeds (u64::MAX on 64-bit) but
        // checked_add(1) overflows, returning DataPrep error before any RPC calls.
        let generator = generator_with_no_data();
        let game = test_candidate_game(0);

        let result = generator.generate_tee_proof(&game, usize::MAX, 1).await;
        let err = result.unwrap_err();
        assert!(matches!(err, TeeProofError::DataPrep(_)));
        assert!(err.to_string().contains("arithmetic overflow"));
    }

    #[tokio::test]
    async fn test_generate_tee_proof_mul_overflow() {
        // (1 + 1) * (u64::MAX / 2 + 1) overflows in checked_mul
        let generator = generator_with_no_data();
        let game = test_candidate_game(0);

        let result = generator.generate_tee_proof(&game, 1, u64::MAX / 2 + 1).await;
        let err = result.unwrap_err();
        assert!(matches!(err, TeeProofError::DataPrep(_)));
        assert!(err.to_string().contains("arithmetic overflow"));
    }

    #[tokio::test]
    async fn test_generate_tee_proof_add_overflow() {
        // starting_block = u64::MAX - 50, (0 + 1) * 100 = 100 → overflows in checked_add
        let generator = generator_with_no_data();
        let game = test_candidate_game(u64::MAX - 50);

        let result = generator.generate_tee_proof(&game, 0, 100).await;
        let err = result.unwrap_err();
        assert!(matches!(err, TeeProofError::DataPrep(_)));
        assert!(err.to_string().contains("arithmetic overflow"));
    }

    // ========================================================================
    // Transaction serialization tests
    // ========================================================================

    /// Creates a non-deposit (EIP-1559) transaction for testing tx filtering.
    fn make_eip1559_tx() -> OpTransaction {
        let tx = TxEip1559 { chain_id: 1, gas_limit: 21000, ..Default::default() };
        let sig = PrimitiveSignature::new(U256::from(1), U256::from(2), false);
        let signed = Signed::new_unchecked(tx, sig, B256::ZERO);
        let envelope = OpTxEnvelope::Eip1559(signed);

        OpTransaction {
            inner: alloy_rpc_types_eth::Transaction {
                inner: alloy_consensus::transaction::Recovered::new_unchecked(
                    envelope,
                    Address::ZERO,
                ),
                block_hash: None,
                block_number: None,
                transaction_index: Some(1),
                effective_gas_price: Some(0),
            },
            deposit_nonce: None,
            deposit_receipt_version: None,
        }
    }

    #[test]
    fn test_serialize_block_transactions_deposit_filtering() {
        let deposit_tx = make_deposit_tx(B256::ZERO, 100);
        let eip1559_tx = make_eip1559_tx();
        let consensus_header = ConsensusHeader { number: 1, ..Default::default() };
        let block_hash = consensus_header.hash_slow();
        let rpc_header =
            RpcHeader { hash: block_hash, inner: consensus_header, ..Default::default() };

        let block = Block {
            header: rpc_header,
            uncles: vec![],
            transactions: BlockTransactions::Full(vec![deposit_tx, eip1559_tx]),
            withdrawals: None,
        };

        // include_deposits=true: both transactions returned
        let all_txs = TestGenerator::serialize_block_transactions(&block, true).unwrap();
        assert_eq!(all_txs.len(), 2);

        // include_deposits=false: only the EIP-1559 tx (deposit is filtered out)
        let sequenced_txs = TestGenerator::serialize_block_transactions(&block, false).unwrap();
        assert_eq!(sequenced_txs.len(), 1);

        // Verify the non-deposit tx starts with EIP-1559 type byte (0x02)
        assert_eq!(sequenced_txs[0][0], 0x02);
    }

    // ========================================================================
    // Error type tests
    // ========================================================================

    #[test]
    fn test_tee_proof_error_enclave_display() {
        let err = TeeProofError::Enclave(ClientError::ClientCreation("test".into()));
        assert!(err.to_string().contains("enclave execution failed"));
    }

    #[test]
    fn test_tee_proof_error_rpc_display() {
        let err = TeeProofError::Rpc {
            context: "rollup config",
            source: RpcError::Timeout("rpc timeout".into()),
        };
        assert_eq!(err.to_string(), "failed to fetch rollup config: Request timeout: rpc timeout");
    }

    #[test]
    fn test_tee_proof_error_data_prep_display() {
        let err = TeeProofError::DataPrep("arithmetic overflow".into());
        assert_eq!(err.to_string(), "data preparation failed: arithmetic overflow");
    }

    #[test]
    fn test_is_retryable_rpc_transport() {
        let err = TeeProofError::Rpc {
            context: "target block",
            source: RpcError::Transport("connection reset".into()),
        };
        assert!(err.is_retryable());
    }

    #[test]
    fn test_is_retryable_rpc_timeout() {
        let err = TeeProofError::Rpc {
            context: "target block",
            source: RpcError::Timeout("timed out".into()),
        };
        assert!(err.is_retryable());
    }

    #[test]
    fn test_is_retryable_rpc_connection() {
        let err = TeeProofError::Rpc {
            context: "target block",
            source: RpcError::Connection("refused".into()),
        };
        assert!(err.is_retryable());
    }

    #[test]
    fn test_is_not_retryable_rpc_block_not_found() {
        let err = TeeProofError::Rpc {
            context: "target block",
            source: RpcError::BlockNotFound("block 123 not found".into()),
        };
        assert!(!err.is_retryable());
    }

    #[test]
    fn test_is_not_retryable_data_prep() {
        let err = TeeProofError::DataPrep("arithmetic overflow".into());
        assert!(!err.is_retryable());
    }

    #[test]
    fn test_is_not_retryable_enclave() {
        let err = TeeProofError::Enclave(ClientError::ClientCreation("down".into()));
        assert!(!err.is_retryable());
    }

    #[test]
    fn test_is_not_retryable_encoding() {
        let err = TeeProofError::Encoding("bad bytes".into());
        assert!(!err.is_retryable());
    }

    #[test]
    fn test_tee_proof_error_encoding_display() {
        let err = TeeProofError::Encoding("bad bytes".into());
        assert_eq!(err.to_string(), "proof encoding failed: bad bytes");
    }

    #[test]
    fn test_tee_proof_error_from_client_error() {
        let client_err = ClientError::ClientCreation("conn refused".into());
        let tee_err: TeeProofError = client_err.into();
        assert!(matches!(tee_err, TeeProofError::Enclave(_)));
    }

    // ========================================================================
    // Integration-style tests for generate_tee_proof
    // ========================================================================

    #[tokio::test]
    async fn test_generate_tee_proof_rollup_config_missing() {
        let enclave = Arc::new(MockEnclaveClient::new(Ok(test_proposal(B256::ZERO, 0))));
        let l1 = Arc::new(MockL1Provider { headers: HashMap::new(), receipts: HashMap::new() });
        let l2 = Arc::new(MockChallengerL2Provider::new());
        let rollup = Arc::new(MockRollupProvider { config: None });

        let generator = TeeProofGenerator::new(enclave, l1, l2, rollup, B256::ZERO);
        let game = test_candidate_game(100);

        let result = generator.generate_tee_proof(&game, 0, 10).await;
        assert!(result.is_err());
        let err = result.unwrap_err();
        assert!(matches!(err, TeeProofError::Rpc { .. }));
        assert!(err.to_string().contains("rollup config"));
    }

    #[tokio::test]
    async fn test_generate_tee_proof_block_fetch_error() {
        let enclave = Arc::new(MockEnclaveClient::new(Ok(test_proposal(B256::ZERO, 0))));
        let l1 = Arc::new(MockL1Provider { headers: HashMap::new(), receipts: HashMap::new() });
        let l2 = Arc::new(MockChallengerL2Provider {
            error: Some("node unavailable".into()),
            ..MockChallengerL2Provider::new()
        });
        let rollup = Arc::new(MockRollupProvider { config: Some(default_rollup_config()) });

        let generator = TeeProofGenerator::new(enclave, l1, l2, rollup, B256::ZERO);
        let game = test_candidate_game(100);

        let result = generator.generate_tee_proof(&game, 0, 10).await;
        let err = result.unwrap_err();
        assert!(matches!(err, TeeProofError::Rpc { .. }));
        assert!(err.to_string().contains("target block"));
    }

    #[tokio::test]
    async fn test_generate_tee_proof_empty_block() {
        let target_block_number = 110u64;
        let enclave = Arc::new(MockEnclaveClient::new(Ok(test_proposal(B256::ZERO, 0))));
        let l1 = Arc::new(MockL1Provider { headers: HashMap::new(), receipts: HashMap::new() });

        // Build a block with zero transactions
        let consensus_header =
            ConsensusHeader { number: target_block_number, ..Default::default() };
        let block_hash = consensus_header.hash_slow();
        let rpc_header =
            RpcHeader { hash: block_hash, inner: consensus_header, ..Default::default() };
        let empty_block = Block {
            header: rpc_header,
            uncles: vec![],
            transactions: BlockTransactions::Full(vec![]),
            withdrawals: None,
        };

        let mut l2 = MockChallengerL2Provider::new();
        l2.blocks.insert(target_block_number, empty_block);

        let rollup = Arc::new(MockRollupProvider { config: Some(default_rollup_config()) });
        let generator = TeeProofGenerator::new(enclave, l1, Arc::new(l2), rollup, B256::ZERO);
        let game = test_candidate_game(100);

        let result = generator.generate_tee_proof(&game, 0, 10).await;
        let err = result.unwrap_err();
        assert!(matches!(err, TeeProofError::DataPrep(_)));
        assert!(err.to_string().contains("no transactions"));
    }

    #[tokio::test]
    async fn test_generate_tee_proof_enclave_error() {
        let target_block_number = 110u64;
        let (l2, l1, rollup, _) = wired_providers(target_block_number);

        // Enclave client configured to fail
        let enclave = Arc::new(MockEnclaveClient::new(Err(ClientError::ClientCreation(
            "enclave down".into(),
        ))));

        let generator = TeeProofGenerator::new(
            enclave,
            Arc::new(l1),
            Arc::new(l2),
            Arc::new(rollup),
            B256::ZERO,
        );
        let game = test_candidate_game(100);

        let result = generator.generate_tee_proof(&game, 0, 10).await;
        assert!(result.is_err());
        let err = result.unwrap_err();
        assert!(matches!(err, TeeProofError::Enclave(_)), "expected Enclave error, got: {err}");
        assert!(err.to_string().contains("enclave down"));
    }

    #[tokio::test]
    async fn test_generate_tee_proof_happy_path() {
        let target_block_number = 110u64;
        let (l2, l1, rollup, l1_origin_hash) = wired_providers(target_block_number);

        // Enclave returns a successful proposal with a valid signature
        let enclave = Arc::new(MockEnclaveClient::new(Ok(test_proposal(l1_origin_hash, 500))));

        let generator = TeeProofGenerator::new(
            Arc::clone(&enclave),
            Arc::new(l1),
            Arc::new(l2),
            Arc::new(rollup),
            B256::ZERO,
        );
        let game = test_candidate_game(100);

        let result = generator.generate_tee_proof(&game, 0, 10).await;
        assert!(result.is_ok(), "expected Ok, got: {}", result.unwrap_err());

        let proof = result.unwrap();

        // Verify 130-byte proof format
        assert_eq!(proof.len(), 130);

        // Byte 0: proof type TEE
        assert_eq!(proof[0], PROOF_TYPE_TEE);

        // Bytes 1-32: L1 origin hash
        assert_eq!(&proof[1..33], l1_origin_hash.as_slice());

        // Bytes 33-56: leading 24 zero-padding bytes of the uint256 field
        assert_eq!(&proof[33..57], &[0u8; 24]);

        // Bytes 57-64: L1 origin number (500) in last 8 bytes of the 32-byte field
        let mut number_bytes = [0u8; 8];
        number_bytes.copy_from_slice(&proof[57..65]);
        assert_eq!(u64::from_be_bytes(number_bytes), 500);

        // Byte 129: v-value should be adjusted (0 → 27)
        assert_eq!(proof[129], 27);

        // Verify the enclave received a request with the correct target block number
        let captured = enclave.captured_request.lock().unwrap();
        let req = captured.as_ref().expect("enclave should have received a request");
        assert_eq!(req.block_header.number, target_block_number);
    }

    // ========================================================================
    // Data transformation helper tests
    // ========================================================================

    #[test]
    fn test_convert_receipts_preserves_log_data() {
        let log_address = address!("2222222222222222222222222222222222222222");
        let log_data = LogData::new_unchecked(vec![], Bytes::from(vec![0x42]));
        let rpc_log = alloy_rpc_types_eth::Log {
            inner: alloy_primitives::Log { address: log_address, data: log_data.clone() },
            ..Default::default()
        };

        let receipt =
            Receipt { status: true.into(), cumulative_gas_used: 21000, logs: vec![rpc_log] };
        let envelope =
            ReceiptEnvelope::Legacy(ReceiptWithBloom { receipt, logs_bloom: Default::default() });

        let tx_receipt = TransactionReceipt {
            inner: envelope,
            transaction_hash: B256::ZERO,
            transaction_index: Some(0),
            block_hash: None,
            block_number: None,
            gas_used: 21000,
            effective_gas_price: 1_000_000_000,
            blob_gas_used: None,
            blob_gas_price: None,
            from: Address::ZERO,
            to: None,
            contract_address: None,
        };

        let converted = TestGenerator::convert_receipts(vec![tx_receipt]);
        assert_eq!(converted.len(), 1);

        match &converted[0] {
            ReceiptEnvelope::Legacy(rwb) => {
                assert_eq!(rwb.receipt.logs.len(), 1);
                assert_eq!(rwb.receipt.logs[0].address, log_address);
                assert_eq!(rwb.receipt.logs[0].data, log_data);
            }
            _ => panic!("expected Legacy receipt envelope"),
        }
    }

    #[test]
    fn test_build_chain_config_maps_rollup_fields() {
        let mut rollup = default_rollup_config();
        rollup.batch_inbox_address = address!("1111111111111111111111111111111111111111");
        rollup.protocol_versions_address = address!("2222222222222222222222222222222222222222");
        rollup.deposit_contract_address = address!("3333333333333333333333333333333333333333");
        rollup.l1_system_config_address = address!("4444444444444444444444444444444444444444");
        rollup.block_time = 2;
        rollup.l1_chain_id = 1;

        let config = TestGenerator::build_chain_config(&rollup);

        assert_eq!(config.batch_inbox_addr, rollup.batch_inbox_address);
        assert_eq!(config.block_time, 2);
        assert_eq!(config.l1_chain_id, 1);
        assert_eq!(config.protocol_versions_addr, Some(rollup.protocol_versions_address));

        let addresses = config.addresses.expect("addresses should be populated");
        assert_eq!(addresses.address_manager, Some(rollup.protocol_versions_address));
        assert_eq!(addresses.optimism_portal_proxy, Some(rollup.deposit_contract_address));
        assert_eq!(addresses.system_config_proxy, Some(rollup.l1_system_config_address));
    }
}
