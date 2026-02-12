//! Prover module for TEE-based block validation.
//!
//! This module provides the core prover functionality for generating
//! TEE-signed proposals for L2 block transitions.

mod types;

pub use types::ProverProposal;

use std::sync::Arc;
use std::time::Duration;

use alloy_consensus::{Header, ReceiptEnvelope};
use alloy_eips::{Typed2718, eip2718::Encodable2718};
use alloy_primitives::{B256, Bytes};
use alloy_rpc_types_eth::TransactionReceipt;
use op_alloy_consensus::OpTxEnvelope;
use op_alloy_rpc_types::Transaction as OpTransaction;
use op_enclave_client::ExecuteStatelessRequest;
use op_enclave_core::types::config::RollupConfig;
use op_enclave_core::{
    ChainConfig, L2_TO_L1_MESSAGE_PASSER, Proposal, l2_block_to_block_info, output_root_v0,
};

use crate::{
    ProposerError,
    enclave::{EnclaveClientTrait, PerChainConfig},
    rpc::{L1BlockId, L1Client, L2BlockRef, L2Client, OpBlock},
};

/// Deposit transaction type identifier (EIP-2718 type byte for OP deposits).
const DEPOSIT_TX_TYPE: u8 = 0x7E;
/// Maximum number of missing trie nodes to fetch via `debug_dbGet` per proposal attempt.
const MAX_MISSING_TRIE_NODE_REPAIRS: usize = 64;
/// Timeout for enclave RPC calls (matches Go `PROPOSAL_TIMEOUT`).
const ENCLAVE_TIMEOUT: Duration = Duration::from_secs(600);

/// Prover for generating TEE-signed proposals.
#[derive(Debug)]
pub struct Prover<L1, L2, E> {
    rollup_config: RollupConfig,
    config_hash: B256,
    l1_client: Arc<L1>,
    l2_client: Arc<L2>,
    enclave_client: E,
}

impl<L1, L2, E> Prover<L1, L2, E>
where
    L1: L1Client,
    L2: L2Client,
    E: EnclaveClientTrait,
{
    /// Creates a new prover instance.
    ///
    /// # Arguments
    ///
    /// * `config` - Per-chain configuration for the L2 chain
    /// * `rollup_config` - Rollup configuration from op-node
    /// * `l1_client` - L1 RPC client
    /// * `l2_client` - L2 RPC client
    /// * `enclave_client` - Enclave RPC client
    #[must_use]
    pub fn new(
        mut config: PerChainConfig,
        rollup_config: RollupConfig,
        l1_client: Arc<L1>,
        l2_client: Arc<L2>,
        enclave_client: E,
    ) -> Self {
        config.force_defaults();
        let config_hash = config.hash();

        Self {
            rollup_config,
            config_hash,
            l1_client,
            l2_client,
            enclave_client,
        }
    }

    /// Returns the configuration hash for this prover.
    #[must_use]
    pub const fn config_hash(&self) -> B256 {
        self.config_hash
    }

    /// Generates a proposal for the given L2 block.
    ///
    /// This function:
    /// 1. Fetches required data in parallel (witness, proofs, L1 origin, receipts)
    /// 2. Serializes transactions for the enclave
    /// 3. Calls the enclave to execute stateless validation
    /// 4. Verifies the output root matches local computation
    /// 5. Returns the proposal with metadata
    ///
    /// # Errors
    ///
    /// Returns an error if:
    /// - RPC calls fail
    /// - Transaction serialization fails
    /// - Enclave execution fails
    /// - Output root verification fails
    pub async fn generate(&self, block: &OpBlock) -> Result<ProverProposal, ProposerError> {
        let block_hash = block.header.hash;

        // Get the first transaction to derive L1 origin info
        let first_tx = block.transactions.txns().next().ok_or_else(|| {
            ProposerError::BlockInfoDerivation("no transactions in block".to_string())
        })?;

        let first_tx_bytes = serialize_rpc_transaction(first_tx)?;

        // Derive L2 block info to get L1 origin
        let l2_block_info = l2_block_to_block_info(
            &self.rollup_config,
            &block.header.inner,
            block_hash,
            &first_tx_bytes,
        )
        .map_err(|e| ProposerError::BlockInfoDerivation(e.to_string()))?;

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
            self.l2_client.execution_witness(block.header.number),
            self.l2_client
                .get_proof(L2_TO_L1_MESSAGE_PASSER, block_hash),
            self.l2_client.block_by_hash(block.header.parent_hash),
            self.l2_client
                .get_proof(L2_TO_L1_MESSAGE_PASSER, block.header.parent_hash),
            self.l1_client.header_by_hash(l1_origin_hash),
            self.l1_client.block_receipts(l1_origin_hash),
        );

        // Unwrap all results
        let mut witness = witness_result?;
        let msg_account = msg_account_result?;
        let prev_block = prev_block_result?;
        let prev_msg_account = prev_msg_account_result?;
        let l1_origin = l1_origin_result?;
        let l1_receipts = l1_receipts_result?;

        // Extract storage hash before moving msg_account
        let msg_storage_hash = msg_account.storage_hash;

        // Serialize previous block transactions (all types including deposits)
        let prev_block_txs = serialize_block_transactions(&prev_block, true)?;

        // Serialize current block transactions (excluding deposits)
        let sequenced_txs = serialize_block_transactions(block, false)?;

        let chain_config = build_chain_config(&self.rollup_config);
        let l1_receipt_envelopes = convert_receipts(l1_receipts);
        let mut repairs = 0usize;
        let proposal = loop {
            // Clones here are acceptable: retries are rare (only on missing trie nodes)
            // and the data is small relative to the enclave RPC cost.
            let request = ExecuteStatelessRequest {
                config: chain_config.clone(),
                config_hash: self.config_hash,
                l1_origin: l1_origin.inner.clone(),
                l1_receipts: l1_receipt_envelopes.clone(),
                previous_block_txs: prev_block_txs.clone(),
                block_header: block.header.inner.clone(),
                sequenced_txs: sequenced_txs.clone(),
                witness: witness.clone(),
                message_account: msg_account.clone(),
                prev_message_account_hash: prev_msg_account.storage_hash,
            };

            match tokio::time::timeout(
                ENCLAVE_TIMEOUT,
                self.enclave_client.execute_stateless(request),
            )
            .await
            {
                Ok(Ok(proposal)) => break proposal,
                Err(_) => return Err(ProposerError::Enclave("enclave execution timed out".into())),
                Ok(Err(err)) => {
                    let err_str = err.to_string();
                    let Some(missing_hash) = extract_missing_trie_hash(&err_str) else {
                        return Err(ProposerError::Enclave(err_str));
                    };

                    if repairs >= MAX_MISSING_TRIE_NODE_REPAIRS {
                        tracing::warn!(
                            block_number = block.header.number,
                            repairs,
                            hash = %missing_hash,
                            "Reached maximum trie node repairs from debug_dbGet"
                        );
                        return Err(ProposerError::Enclave(err_str));
                    }

                    let key = format!("{missing_hash:#x}");
                    if witness.state.contains_key(&key) {
                        return Err(ProposerError::Enclave(err_str));
                    }

                    let preimage = self.l2_client.db_get(missing_hash).await?;
                    witness
                        .state
                        .insert(key, format!("0x{}", hex::encode(preimage)));
                    repairs += 1;

                    tracing::warn!(
                        block_number = block.header.number,
                        repairs,
                        hash = %missing_hash,
                        "Recovered missing trie node via debug_dbGet; retrying enclave execution"
                    );
                }
            }
        };

        // Verify output root matches local computation
        let expected_output_root = output_root_v0(&block.header.inner, msg_storage_hash);
        if proposal.output_root != expected_output_root {
            return Err(ProposerError::OutputRootMismatch {
                expected: expected_output_root,
                actual: proposal.output_root,
            });
        }

        // Verify L1 origin hash matches
        if proposal.l1_origin_hash != l1_origin_hash {
            return Err(ProposerError::L1OriginMismatch {
                expected: l1_origin_hash,
                actual: proposal.l1_origin_hash,
            });
        }

        // Verify L2 block number matches (convert U256 to u64)
        let proposal_block_number: u64 = proposal.l2_block_number.try_into().map_err(|_| {
            ProposerError::Internal("proposal block number overflows u64".to_string())
        })?;
        if proposal_block_number != block.header.number {
            return Err(ProposerError::BlockNumberMismatch {
                expected: block.header.number,
                actual: proposal_block_number,
            });
        }

        // Check if block has withdrawals
        let has_withdrawals = check_withdrawals(&block.header.inner);

        let block_ref = L2BlockRef {
            hash: block_hash,
            number: block.header.number,
            parent_hash: block.header.parent_hash,
            timestamp: block.header.inner.timestamp,
            l1origin: L1BlockId {
                hash: l1_origin_hash,
                number: l1_origin_number,
            },
            sequence_number: l2_block_info.seq_num,
        };

        Ok(ProverProposal {
            output: proposal,
            from: block_ref.clone(),
            to: block_ref,
            has_withdrawals,
        })
    }

    /// Aggregates multiple prover proposals into a single batched proposal.
    ///
    /// This handles the higher-level aggregation logic:
    /// - Empty input returns an error
    /// - Single proposal is returned as-is
    /// - Multiple proposals are aggregated via the enclave, with withdrawal
    ///   flags OR'd and block ranges tracked via `from`/`to`
    ///
    /// # Errors
    ///
    /// Returns an error if no proposals are provided or enclave aggregation fails.
    pub async fn aggregate(
        &self,
        prev_output_root: B256,
        proposals: Vec<ProverProposal>,
    ) -> Result<ProverProposal, ProposerError> {
        if proposals.is_empty() {
            return Err(ProposerError::Internal("no proposals to aggregate".into()));
        }
        if proposals.len() == 1 {
            return Ok(proposals.into_iter().next().unwrap());
        }

        let has_withdrawals = proposals.iter().any(|p| p.has_withdrawals);
        let enclave_proposals: Vec<Proposal> = proposals.iter().map(|p| p.output.clone()).collect();

        let from = proposals.first().unwrap().from.clone();
        let to = proposals.last().unwrap().to.clone();

        let output = self
            .aggregate_enclave(prev_output_root, enclave_proposals)
            .await?;

        Ok(ProverProposal {
            output,
            from,
            to,
            has_withdrawals,
        })
    }

    /// Low-level enclave aggregation call.
    async fn aggregate_enclave(
        &self,
        prev_output_root: B256,
        proposals: Vec<Proposal>,
    ) -> Result<Proposal, ProposerError> {
        tokio::time::timeout(
            ENCLAVE_TIMEOUT,
            self.enclave_client
                .aggregate(self.config_hash, prev_output_root, proposals),
        )
        .await
        .map_err(|_| ProposerError::Enclave("enclave aggregation timed out".into()))?
        .map_err(|e| ProposerError::Enclave(e.to_string()))
    }
}

/// Serializes an RPC transaction to EIP-2718 encoded bytes.
///
/// Extracts the `OpTxEnvelope` from the RPC transaction and encodes it.
/// This handles both standard Ethereum transactions and OP Stack deposit transactions (type 0x7E).
fn serialize_rpc_transaction(tx: &OpTransaction) -> Result<Bytes, ProposerError> {
    // Extract the inner OpTxEnvelope from the RPC transaction
    // op_alloy_rpc_types::Transaction.inner is alloy_rpc_types::Transaction<OpTxEnvelope>
    // Calling into_inner() on that returns the OpTxEnvelope
    // Clone required: into_inner() consumes, and we only have a reference.
    let envelope: OpTxEnvelope = tx.clone().inner.into_inner();

    let mut buf = Vec::new();
    envelope.encode_2718(&mut buf);
    Ok(Bytes::from(buf))
}

/// Serializes all transactions in a block to EIP-2718 encoded bytes.
///
/// # Arguments
///
/// * `block` - The block containing transactions
/// * `include_deposits` - Whether to include deposit transactions
fn serialize_block_transactions(
    block: &OpBlock,
    include_deposits: bool,
) -> Result<Vec<Bytes>, ProposerError> {
    block
        .transactions
        .txns()
        .filter(|tx| include_deposits || !is_deposit_tx(tx))
        .map(serialize_rpc_transaction)
        .collect()
}

/// Checks if an RPC transaction is a deposit transaction.
///
/// Deposit transactions have type 0x7E (126).
fn is_deposit_tx(tx: &OpTransaction) -> bool {
    tx.ty() == DEPOSIT_TX_TYPE
}

/// Converts transaction receipts to receipt envelopes for the enclave.
///
/// This converts the RPC `ReceiptEnvelope<Log>` to consensus `ReceiptEnvelope`
/// by mapping the log types.
fn convert_receipts(receipts: Vec<TransactionReceipt>) -> Vec<ReceiptEnvelope> {
    receipts
        .into_iter()
        .map(|r| convert_receipt_envelope(r.inner))
        .collect()
}

/// Converts an RPC receipt envelope to a consensus receipt envelope.
fn convert_receipt_envelope(
    receipt: alloy_consensus::ReceiptEnvelope<alloy_rpc_types_eth::Log>,
) -> ReceiptEnvelope {
    // Map the receipt to convert logs from RPC Log to consensus Log
    receipt.map_logs(|log| alloy_primitives::Log {
        address: log.inner.address,
        data: log.inner.data,
    })
}

/// Checks if a block has withdrawals by examining the logs bloom.
///
/// Withdrawals are detected by checking if the `L2ToL1MessagePasser` address
/// appears in the logs bloom filter.
fn check_withdrawals(header: &Header) -> bool {
    use alloy_primitives::BloomInput;
    header
        .logs_bloom
        .contains_input(BloomInput::Raw(L2_TO_L1_MESSAGE_PASSER.as_slice()))
}

/// Builds the `kona_genesis::ChainConfig` envelope expected by enclave RPC.
fn build_chain_config(rollup_config: &RollupConfig) -> ChainConfig {
    let mut config = ChainConfig {
        name: "base-proposer".to_string(),
        l1_chain_id: rollup_config.l1_chain_id,
        public_rpc: String::new(),
        sequencer_rpc: String::new(),
        explorer: String::new(),
        superchain_level: Default::default(),
        governed_by_optimism: false,
        superchain_time: None,
        data_availability_type: "eth-da".to_string(),
        chain_id: rollup_config.l2_chain_id.id(),
        batch_inbox_addr: rollup_config.batch_inbox_address,
        block_time: rollup_config.block_time,
        seq_window_size: rollup_config.seq_window_size,
        max_sequencer_drift: rollup_config.max_sequencer_drift,
        gas_paying_token: None,
        hardfork_config: rollup_config.hardforks,
        optimism: Some(rollup_config.chain_op_config),
        alt_da: rollup_config.alt_da_config.clone(),
        genesis: rollup_config.genesis,
        roles: None,
        addresses: Some(Default::default()),
    };

    // These address fields are required by kona_genesis::ChainConfig but are not
    // consumed by the enclave — it only reads deposit_contract_address and
    // l1_system_config_address from the PerChainConfig. The address_manager value
    // is mapped to protocol_versions_address solely to satisfy the struct; the
    // enclave discards it.
    if let Some(addresses) = config.addresses.as_mut() {
        addresses.address_manager = Some(rollup_config.protocol_versions_address);
        addresses.optimism_portal_proxy = Some(rollup_config.deposit_contract_address);
        addresses.system_config_proxy = Some(rollup_config.l1_system_config_address);
    }

    config
}

/// Extracts a missing trie node hash from enclave error text.
///
/// Looks for the substring "trie node not found for hash: 0x...".
fn extract_missing_trie_hash(err: &str) -> Option<B256> {
    let marker = "trie node not found for hash:";
    let idx = err.find(marker)?;
    let suffix = err.get(idx + marker.len()..)?.trim_start();
    let mut end = suffix.len();
    for (i, c) in suffix.char_indices() {
        if !(c.is_ascii_hexdigit() || c == 'x') {
            end = i;
            break;
        }
    }
    let candidate = suffix
        .get(..end)?
        .trim_end_matches('"')
        .trim_end_matches(',');
    if !candidate.starts_with("0x") {
        return None;
    }
    candidate.parse().ok()
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use alloy_primitives::{B256, Bloom, BloomInput, Bytes, U256};
    use async_trait::async_trait;
    use op_enclave_client::{ClientError, ExecuteStatelessRequest};

    use super::*;
    use crate::enclave::EnclaveClientTrait;
    use crate::test_utils::test_prover;
    use types::test_helpers::test_proposal;

    #[test]
    fn test_check_withdrawals_empty_bloom() {
        let header = Header {
            logs_bloom: Default::default(),
            ..Default::default()
        };
        assert!(!check_withdrawals(&header));
    }

    #[test]
    fn test_deposit_tx_type_constant() {
        // Verify the deposit transaction type constant is correct
        assert_eq!(DEPOSIT_TX_TYPE, 0x7E);
        assert_eq!(DEPOSIT_TX_TYPE, 126);
    }

    // --- extract_missing_trie_hash tests ---

    #[test]
    fn test_extract_missing_trie_hash_valid() {
        let hash_hex = "0xabcdef1234567890abcdef1234567890abcdef1234567890abcdef1234567890";
        let err = format!("execution failed: trie node not found for hash: {hash_hex}");
        let result = extract_missing_trie_hash(&err);
        assert_eq!(result, Some(hash_hex.parse::<B256>().unwrap()));
    }

    #[test]
    fn test_extract_missing_trie_hash_no_match() {
        let err = "execution failed: something completely different";
        assert_eq!(extract_missing_trie_hash(err), None);
    }

    #[test]
    fn test_extract_missing_trie_hash_truncated() {
        let err = "trie node not found for hash: 0xabcd";
        assert_eq!(extract_missing_trie_hash(err), None);
    }

    #[test]
    fn test_extract_missing_trie_hash_trailing_chars() {
        let hash_hex = "0xabcdef1234567890abcdef1234567890abcdef1234567890abcdef1234567890";
        let err = format!("trie node not found for hash: {hash_hex}\",");
        let result = extract_missing_trie_hash(&err);
        assert_eq!(result, Some(hash_hex.parse::<B256>().unwrap()));
    }

    #[test]
    fn test_extract_missing_trie_hash_no_0x_prefix() {
        let err = "trie node not found for hash: abcdef1234567890abcdef1234567890abcdef1234567890abcdef1234567890";
        assert_eq!(extract_missing_trie_hash(err), None);
    }

    // --- check_withdrawals tests ---

    #[test]
    fn test_check_withdrawals_with_message_passer() {
        let mut bloom = Bloom::default();
        bloom.accrue(BloomInput::Raw(L2_TO_L1_MESSAGE_PASSER.as_slice()));
        let header = Header {
            logs_bloom: bloom,
            ..Default::default()
        };
        assert!(check_withdrawals(&header));
    }

    #[test]
    fn test_check_withdrawals_with_other_address() {
        let mut bloom = Bloom::default();
        let other_address =
            alloy_primitives::address!("0x1111111111111111111111111111111111111111");
        bloom.accrue(BloomInput::Raw(other_address.as_slice()));
        let header = Header {
            logs_bloom: bloom,
            ..Default::default()
        };
        assert!(!check_withdrawals(&header));
    }

    // --- convert_receipt_envelope test ---

    #[test]
    fn test_convert_receipt_envelope_preserves_log_data() {
        use alloy_consensus::{Receipt, ReceiptWithBloom};

        let log_address = alloy_primitives::address!("0x2222222222222222222222222222222222222222");
        let log_data = alloy_primitives::LogData::new_unchecked(vec![], Bytes::from(vec![0x42]));
        let rpc_log = alloy_rpc_types_eth::Log {
            inner: alloy_primitives::Log {
                address: log_address,
                data: log_data.clone(),
            },
            ..Default::default()
        };

        let receipt = Receipt {
            status: true.into(),
            cumulative_gas_used: 21000,
            logs: vec![rpc_log],
        };

        let envelope = ReceiptEnvelope::Legacy(ReceiptWithBloom {
            receipt,
            logs_bloom: Default::default(),
        });

        let converted = convert_receipt_envelope(envelope);

        match converted {
            ReceiptEnvelope::Legacy(rwb) => {
                assert_eq!(rwb.receipt.logs.len(), 1);
                assert_eq!(rwb.receipt.logs[0].address, log_address);
                assert_eq!(rwb.receipt.logs[0].data, log_data);
            }
            _ => panic!("Expected Legacy envelope"),
        }
    }

    // --- aggregate tests (via Prover::aggregate) ---

    /// Mock enclave client that returns a pre-configured aggregate result.
    struct MockEnclaveClient {
        aggregate_result: Proposal,
    }

    #[async_trait]
    impl EnclaveClientTrait for MockEnclaveClient {
        async fn execute_stateless(
            &self,
            _req: ExecuteStatelessRequest,
        ) -> Result<Proposal, ClientError> {
            unimplemented!("not needed for aggregate tests")
        }

        async fn aggregate(
            &self,
            _config_hash: B256,
            _prev_output_root: B256,
            _proposals: Vec<Proposal>,
        ) -> Result<Proposal, ClientError> {
            Ok(self.aggregate_result.clone())
        }
    }

    #[tokio::test]
    async fn test_aggregate_empty_proposals() {
        let prover = test_prover(MockEnclaveClient {
            aggregate_result: Proposal {
                output_root: B256::ZERO,
                signature: Bytes::new(),
                l1_origin_hash: B256::ZERO,
                l2_block_number: U256::ZERO,
                prev_output_root: B256::ZERO,
                config_hash: B256::ZERO,
            },
        });

        let result = prover.aggregate(B256::ZERO, vec![]).await;
        assert!(result.is_err());
        assert!(
            result
                .unwrap_err()
                .to_string()
                .contains("no proposals to aggregate")
        );
    }

    #[tokio::test]
    async fn test_aggregate_single_proposal() {
        let prover = test_prover(MockEnclaveClient {
            aggregate_result: Proposal {
                output_root: B256::ZERO,
                signature: Bytes::new(),
                l1_origin_hash: B256::ZERO,
                l2_block_number: U256::ZERO,
                prev_output_root: B256::ZERO,
                config_hash: B256::ZERO,
            },
        });

        let single = test_proposal(5, 5, true);
        let result = prover.aggregate(B256::ZERO, vec![single.clone()]).await;
        let agg = result.unwrap();

        // Single proposal is returned as-is
        assert_eq!(agg.from.number, 5);
        assert_eq!(agg.to.number, 5);
        assert!(agg.has_withdrawals);
        assert_eq!(agg.output.output_root, single.output.output_root);
    }

    #[tokio::test]
    async fn test_aggregate_multiple_proposals_from_to() {
        let agg_output = Proposal {
            output_root: B256::repeat_byte(0xAA),
            signature: Bytes::from(vec![0xBB; 65]),
            l1_origin_hash: B256::repeat_byte(0xCC),
            l2_block_number: U256::from(25),
            prev_output_root: B256::repeat_byte(0xDD),
            config_hash: B256::repeat_byte(0xEE),
        };
        let prover = test_prover(MockEnclaveClient {
            aggregate_result: agg_output.clone(),
        });

        let proposals = vec![
            test_proposal(10, 15, false),
            test_proposal(16, 20, false),
            test_proposal(21, 25, false),
        ];

        let result = prover.aggregate(B256::ZERO, proposals).await.unwrap();

        assert_eq!(result.from.number, 10);
        assert_eq!(result.to.number, 25);
        assert_eq!(result.output.output_root, agg_output.output_root);
    }

    #[tokio::test]
    async fn test_aggregate_withdrawal_combinations() {
        let agg_output = Proposal {
            output_root: B256::repeat_byte(0xAA),
            signature: Bytes::from(vec![0xBB; 65]),
            l1_origin_hash: B256::ZERO,
            l2_block_number: U256::from(3),
            prev_output_root: B256::ZERO,
            config_hash: B256::ZERO,
        };

        // [F, F, F] → F
        let prover = test_prover(MockEnclaveClient {
            aggregate_result: agg_output.clone(),
        });
        let result = prover
            .aggregate(
                B256::ZERO,
                vec![
                    test_proposal(1, 1, false),
                    test_proposal(2, 2, false),
                    test_proposal(3, 3, false),
                ],
            )
            .await
            .unwrap();
        assert!(!result.has_withdrawals);

        // [T, F, F] → T
        let prover = test_prover(MockEnclaveClient {
            aggregate_result: agg_output.clone(),
        });
        let result = prover
            .aggregate(
                B256::ZERO,
                vec![
                    test_proposal(1, 1, true),
                    test_proposal(2, 2, false),
                    test_proposal(3, 3, false),
                ],
            )
            .await
            .unwrap();
        assert!(result.has_withdrawals);

        // [F, T, F] → T
        let prover = test_prover(MockEnclaveClient {
            aggregate_result: agg_output.clone(),
        });
        let result = prover
            .aggregate(
                B256::ZERO,
                vec![
                    test_proposal(1, 1, false),
                    test_proposal(2, 2, true),
                    test_proposal(3, 3, false),
                ],
            )
            .await
            .unwrap();
        assert!(result.has_withdrawals);

        // [F, F, T] → T
        let prover = test_prover(MockEnclaveClient {
            aggregate_result: agg_output.clone(),
        });
        let result = prover
            .aggregate(
                B256::ZERO,
                vec![
                    test_proposal(1, 1, false),
                    test_proposal(2, 2, false),
                    test_proposal(3, 3, true),
                ],
            )
            .await
            .unwrap();
        assert!(result.has_withdrawals);

        // [T, T, T] → T
        let prover = test_prover(MockEnclaveClient {
            aggregate_result: agg_output,
        });
        let result = prover
            .aggregate(
                B256::ZERO,
                vec![
                    test_proposal(1, 1, true),
                    test_proposal(2, 2, true),
                    test_proposal(3, 3, true),
                ],
            )
            .await
            .unwrap();
        assert!(result.has_withdrawals);
    }

    // --- enclave error / timeout tests ---

    /// Mock enclave client whose `aggregate` always returns an error.
    struct FailingEnclaveClient;

    #[async_trait]
    impl EnclaveClientTrait for FailingEnclaveClient {
        async fn execute_stateless(
            &self,
            _: ExecuteStatelessRequest,
        ) -> Result<Proposal, ClientError> {
            unimplemented!()
        }
        async fn aggregate(
            &self,
            _: B256,
            _: B256,
            _: Vec<Proposal>,
        ) -> Result<Proposal, ClientError> {
            Err(ClientError::ClientCreation(
                "enclave aggregate failed".into(),
            ))
        }
    }

    #[tokio::test]
    async fn test_aggregate_enclave_error() {
        let prover = test_prover(FailingEnclaveClient);

        let result = prover
            .aggregate(
                B256::ZERO,
                vec![test_proposal(1, 1, false), test_proposal(2, 2, false)],
            )
            .await;

        assert!(result.is_err());
        let err = result.unwrap_err().to_string();
        assert!(
            err.contains("enclave aggregate failed"),
            "expected enclave error message, got: {err}"
        );
    }

    /// Mock enclave client whose `aggregate` sleeps longer than `ENCLAVE_TIMEOUT`.
    struct SlowEnclaveClient;

    #[async_trait]
    impl EnclaveClientTrait for SlowEnclaveClient {
        async fn execute_stateless(
            &self,
            _: ExecuteStatelessRequest,
        ) -> Result<Proposal, ClientError> {
            unimplemented!()
        }
        async fn aggregate(
            &self,
            _: B256,
            _: B256,
            _: Vec<Proposal>,
        ) -> Result<Proposal, ClientError> {
            tokio::time::sleep(Duration::from_secs(601)).await;
            Ok(Proposal {
                output_root: B256::ZERO,
                signature: Bytes::new(),
                l1_origin_hash: B256::ZERO,
                l2_block_number: U256::ZERO,
                prev_output_root: B256::ZERO,
                config_hash: B256::ZERO,
            })
        }
    }

    #[tokio::test(flavor = "current_thread", start_paused = true)]
    async fn test_aggregate_enclave_timeout() {
        let prover = test_prover(SlowEnclaveClient);

        let result = prover
            .aggregate(
                B256::ZERO,
                vec![test_proposal(1, 1, false), test_proposal(2, 2, false)],
            )
            .await;

        assert!(result.is_err());
        let err = result.unwrap_err().to_string();
        assert!(
            err.contains("timed out"),
            "expected timeout error, got: {err}"
        );
    }
}
