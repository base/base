//! Prover module for TEE-based block validation.
//!
//! This module provides the core prover functionality for generating
//! TEE-signed proposals for L2 block transitions.

mod types;
use std::sync::Arc;

use alloy_consensus::Header;
use alloy_primitives::{B256, BloomInput};
use base_enclave::{
    AggregateRequest, Proposal, RollupConfig, l2_block_to_block_info, output_root_v0_with_hash,
};
use base_enclave_client::ExecuteStatelessRequest;
use base_proof_rpc::{L1BlockId, L1Provider, L2BlockRef, OpBlock};
use base_protocol::Predeploys;
use base_tee_prover::{
    ConfigBuilder, ENCLAVE_TIMEOUT, ReceiptConverter, TeeExecutor, TransactionSerializer,
};
pub use types::ProverProposal;
#[cfg(test)]
pub(crate) use types::test_helpers;

use crate::{ProposerError, rpc::ProverL2Provider};

/// Maximum number of missing trie nodes to fetch via `debug_dbGet` per proposal attempt.
const MAX_MISSING_TRIE_NODE_REPAIRS: usize = 64;

/// Prover for generating TEE-signed proposals.
#[derive(Debug)]
pub struct Prover<L1, L2, E> {
    rollup_config: RollupConfig,
    config_hash: B256,
    l1_client: Arc<L1>,
    l2_client: Arc<L2>,
    enclave_client: E,
    /// The proposer address included in the signed journal.
    proposer: alloy_primitives::Address,
    /// The keccak256 hash of the TEE image PCR0.
    tee_image_hash: B256,
}

impl<L1, L2, E> Prover<L1, L2, E>
where
    L1: L1Provider,
    L2: ProverL2Provider,
    E: TeeExecutor,
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
    /// * `proposer` - The proposer address for the signed journal
    /// * `tee_image_hash` - The TEE image hash for the signed journal
    #[must_use]
    pub fn new(
        mut config: base_enclave::PerChainConfig,
        rollup_config: RollupConfig,
        l1_client: Arc<L1>,
        l2_client: Arc<L2>,
        enclave_client: E,
        proposer: alloy_primitives::Address,
        tee_image_hash: B256,
    ) -> Self {
        config.force_defaults();
        let config_hash = config.hash();

        Self {
            rollup_config,
            config_hash,
            l1_client,
            l2_client,
            enclave_client,
            proposer,
            tee_image_hash,
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

        let first_tx_bytes = TransactionSerializer::serialize_rpc_transaction(first_tx)
            .map_err(|e| ProposerError::TxSerialization(e.to_string()))?;

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
            self.l2_client.get_proof(Predeploys::L2_TO_L1_MESSAGE_PASSER, block_hash),
            self.l2_client.block_by_hash(block.header.parent_hash),
            self.l2_client.get_proof(Predeploys::L2_TO_L1_MESSAGE_PASSER, block.header.parent_hash),
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
        let prev_block_txs = TransactionSerializer::serialize_block_transactions(&prev_block, true)
            .map_err(|e| ProposerError::TxSerialization(e.to_string()))?;

        // Serialize current block transactions (excluding deposits)
        let sequenced_txs = TransactionSerializer::serialize_block_transactions(block, false)
            .map_err(|e| ProposerError::TxSerialization(e.to_string()))?;

        let chain_config = ConfigBuilder::build_chain_config(&self.rollup_config, "base-proposer");
        let l1_receipt_envelopes = ReceiptConverter::convert_receipts(l1_receipts);
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
                proposer: self.proposer,
                tee_image_hash: self.tee_image_hash,
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

                    // Try to repair missing trie nodes.
                    if let Some(missing_hash) = extract_missing_trie_hash(&err_str) {
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
                        witness.state.insert(key, format!("0x{}", hex::encode(preimage)));
                        repairs += 1;

                        tracing::warn!(
                            block_number = block.header.number,
                            repairs,
                            hash = %missing_hash,
                            "Recovered missing trie node via debug_dbGet; retrying enclave execution"
                        );
                        continue;
                    }

                    // Missing header errors are not recoverable from the proposer side.
                    if extract_missing_header_hash(&err_str).is_some() {
                        tracing::warn!(
                            block_number = block.header.number,
                            "Enclave cannot find header by hash -- this is likely an executor version issue with post-Pectra header hashing"
                        );
                        return Err(ProposerError::Enclave(err_str));
                    }

                    return Err(ProposerError::Enclave(err_str));
                }
            }
        };

        // Verify output root matches local computation
        let expected_output_root =
            output_root_v0_with_hash(&block.header.inner, msg_storage_hash, block_hash);
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
            l1origin: L1BlockId { hash: l1_origin_hash, number: l1_origin_number },
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
    /// # Arguments
    ///
    /// * `prev_output_root` - The output root before the first proposal
    /// * `prev_block_number` - The L2 block number before the first proposal
    /// * `proposals` - The proposals to aggregate
    /// * `intermediate_roots` - Intermediate output roots at checkpoint intervals
    ///
    /// # Errors
    ///
    /// Returns an error if no proposals are provided or enclave aggregation fails.
    pub async fn aggregate(
        &self,
        prev_output_root: B256,
        prev_block_number: u64,
        proposals: Vec<ProverProposal>,
        intermediate_roots: Vec<B256>,
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
            .aggregate_enclave(
                prev_output_root,
                prev_block_number,
                enclave_proposals,
                intermediate_roots,
            )
            .await?;

        Ok(ProverProposal { output, from, to, has_withdrawals })
    }

    /// Low-level enclave aggregation call.
    async fn aggregate_enclave(
        &self,
        prev_output_root: B256,
        prev_block_number: u64,
        proposals: Vec<Proposal>,
        intermediate_roots: Vec<B256>,
    ) -> Result<Proposal, ProposerError> {
        let request = AggregateRequest {
            config_hash: self.config_hash,
            prev_output_root,
            prev_block_number,
            proposals,
            proposer: self.proposer,
            tee_image_hash: self.tee_image_hash,
            intermediate_roots,
        };
        tokio::time::timeout(ENCLAVE_TIMEOUT, self.enclave_client.aggregate(request))
            .await
            .map_err(|_| ProposerError::Enclave("enclave aggregation timed out".into()))?
            .map_err(|e| ProposerError::Enclave(e.to_string()))
    }
}

/// Checks if a block has withdrawals by examining the logs bloom.
///
/// Withdrawals are detected by checking if the `L2ToL1MessagePasser` address
/// appears in the logs bloom filter.
fn check_withdrawals(header: &Header) -> bool {
    header
        .logs_bloom
        .contains_input(BloomInput::Raw(Predeploys::L2_TO_L1_MESSAGE_PASSER.as_slice()))
}

/// Extracts a missing header hash from enclave error text.
///
/// Looks for the substring "header not found for hash: 0x...".
fn extract_missing_header_hash(err: &str) -> Option<B256> {
    let marker = "header not found for hash:";
    let idx = err.find(marker)?;
    let suffix = err.get(idx + marker.len()..)?.trim_start();
    let mut end = suffix.len();
    for (i, c) in suffix.char_indices() {
        if !(c.is_ascii_hexdigit() || c == 'x') {
            end = i;
            break;
        }
    }
    let candidate = suffix.get(..end)?.trim_end_matches('"').trim_end_matches(',');
    if !candidate.starts_with("0x") {
        return None;
    }
    candidate.parse().ok()
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
    let candidate = suffix.get(..end)?.trim_end_matches('"').trim_end_matches(',');
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
    use base_enclave::{AggregateRequest, Proposal};
    use base_enclave_client::{ClientError, ExecuteStatelessRequest};
    use base_tee_prover::TeeExecutor;
    use rstest::rstest;
    use types::test_helpers::test_proposal;

    use super::*;
    use crate::test_utils::test_prover;

    fn zero_proposal() -> Proposal {
        Proposal {
            output_root: B256::ZERO,
            signature: Bytes::new(),
            l1_origin_hash: B256::ZERO,
            l1_origin_number: U256::ZERO,
            l2_block_number: U256::ZERO,
            prev_output_root: B256::ZERO,
            config_hash: B256::ZERO,
        }
    }

    #[test]
    fn test_check_withdrawals_empty_bloom() {
        let header = Header { logs_bloom: Default::default(), ..Default::default() };
        assert!(!check_withdrawals(&header));
    }

    // --- extract_missing_trie_hash tests ---

    #[rstest]
    #[case::valid(
        "execution failed: trie node not found for hash: 0xabcdef1234567890abcdef1234567890abcdef1234567890abcdef1234567890",
        Some("0xabcdef1234567890abcdef1234567890abcdef1234567890abcdef1234567890")
    )]
    #[case::no_match("execution failed: something completely different", None)]
    #[case::truncated("trie node not found for hash: 0xabcd", None)]
    #[case::trailing_chars(
        "trie node not found for hash: 0xabcdef1234567890abcdef1234567890abcdef1234567890abcdef1234567890\",",
        Some("0xabcdef1234567890abcdef1234567890abcdef1234567890abcdef1234567890")
    )]
    #[case::no_0x_prefix(
        "trie node not found for hash: abcdef1234567890abcdef1234567890abcdef1234567890abcdef1234567890",
        None
    )]
    fn test_extract_missing_trie_hash(#[case] err: &str, #[case] expected: Option<&str>) {
        let expected = expected.map(|h| h.parse::<B256>().unwrap());
        assert_eq!(extract_missing_trie_hash(err), expected);
    }

    // --- check_withdrawals tests ---

    #[test]
    fn test_check_withdrawals_with_message_passer() {
        let mut bloom = Bloom::default();
        bloom.accrue(BloomInput::Raw(Predeploys::L2_TO_L1_MESSAGE_PASSER.as_slice()));
        let header = Header { logs_bloom: bloom, ..Default::default() };
        assert!(check_withdrawals(&header));
    }

    #[test]
    fn test_check_withdrawals_with_other_address() {
        let mut bloom = Bloom::default();
        let other_address =
            alloy_primitives::address!("0x1111111111111111111111111111111111111111");
        bloom.accrue(BloomInput::Raw(other_address.as_slice()));
        let header = Header { logs_bloom: bloom, ..Default::default() };
        assert!(!check_withdrawals(&header));
    }

    // --- aggregate tests (via Prover::aggregate) ---

    /// Mock enclave client that returns a pre-configured aggregate result.
    struct MockEnclaveClient {
        aggregate_result: Proposal,
    }

    #[async_trait]
    impl TeeExecutor for MockEnclaveClient {
        async fn execute_stateless(
            &self,
            _req: ExecuteStatelessRequest,
        ) -> Result<Proposal, ClientError> {
            unimplemented!("not needed for aggregate tests")
        }

        async fn aggregate(&self, _req: AggregateRequest) -> Result<Proposal, ClientError> {
            Ok(self.aggregate_result.clone())
        }
    }

    #[tokio::test]
    async fn test_aggregate_empty_proposals() {
        let prover = test_prover(MockEnclaveClient { aggregate_result: zero_proposal() });

        let result = prover.aggregate(B256::ZERO, 0, vec![], vec![]).await;
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("no proposals to aggregate"));
    }

    #[tokio::test]
    async fn test_aggregate_single_proposal() {
        let prover = test_prover(MockEnclaveClient { aggregate_result: zero_proposal() });

        let single = test_proposal(5, 5, true);
        let result = prover.aggregate(B256::ZERO, 0, vec![single.clone()], vec![]).await;
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
            l1_origin_number: U256::from(125),
            l2_block_number: U256::from(25),
            prev_output_root: B256::repeat_byte(0xDD),
            config_hash: B256::repeat_byte(0xEE),
        };
        let prover = test_prover(MockEnclaveClient { aggregate_result: agg_output.clone() });

        let proposals = vec![
            test_proposal(10, 15, false),
            test_proposal(16, 20, false),
            test_proposal(21, 25, false),
        ];

        let result = prover.aggregate(B256::ZERO, 0, proposals, vec![]).await.unwrap();

        assert_eq!(result.from.number, 10);
        assert_eq!(result.to.number, 25);
        assert_eq!(result.output.output_root, agg_output.output_root);
    }

    #[rstest]
    #[case::none_have_withdrawals(false, false, false, false)]
    #[case::first_has_withdrawal(true, false, false, true)]
    #[case::middle_has_withdrawal(false, true, false, true)]
    #[case::last_has_withdrawal(false, false, true, true)]
    #[case::all_have_withdrawals(true, true, true, true)]
    #[tokio::test]
    async fn test_aggregate_withdrawal_combinations(
        #[case] w1: bool,
        #[case] w2: bool,
        #[case] w3: bool,
        #[case] expected: bool,
    ) {
        let agg_output = Proposal {
            output_root: B256::repeat_byte(0xAA),
            signature: Bytes::from(vec![0xBB; 65]),
            l1_origin_hash: B256::ZERO,
            l1_origin_number: U256::ZERO,
            l2_block_number: U256::from(3),
            prev_output_root: B256::ZERO,
            config_hash: B256::ZERO,
        };

        let prover = test_prover(MockEnclaveClient { aggregate_result: agg_output });
        let result = prover
            .aggregate(
                B256::ZERO,
                0,
                vec![test_proposal(1, 1, w1), test_proposal(2, 2, w2), test_proposal(3, 3, w3)],
                vec![],
            )
            .await
            .unwrap();
        assert_eq!(result.has_withdrawals, expected);
    }

    // --- enclave error / timeout tests ---

    /// Mock enclave client whose `aggregate` always returns an error.
    struct FailingEnclaveClient;

    #[async_trait]
    impl TeeExecutor for FailingEnclaveClient {
        async fn execute_stateless(
            &self,
            _: ExecuteStatelessRequest,
        ) -> Result<Proposal, ClientError> {
            unimplemented!()
        }

        async fn aggregate(&self, _: AggregateRequest) -> Result<Proposal, ClientError> {
            Err(ClientError::ClientCreation("enclave aggregate failed".into()))
        }
    }

    #[tokio::test]
    async fn test_aggregate_enclave_error() {
        let prover = test_prover(FailingEnclaveClient);

        let result = prover
            .aggregate(
                B256::ZERO,
                0,
                vec![test_proposal(1, 1, false), test_proposal(2, 2, false)],
                vec![],
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
    impl TeeExecutor for SlowEnclaveClient {
        async fn execute_stateless(
            &self,
            _: ExecuteStatelessRequest,
        ) -> Result<Proposal, ClientError> {
            unimplemented!()
        }

        async fn aggregate(&self, _: AggregateRequest) -> Result<Proposal, ClientError> {
            tokio::time::sleep(Duration::from_secs(601)).await;
            Ok(zero_proposal())
        }
    }

    #[tokio::test(flavor = "current_thread", start_paused = true)]
    async fn test_aggregate_enclave_timeout() {
        let prover = test_prover(SlowEnclaveClient);

        let result = prover
            .aggregate(
                B256::ZERO,
                0,
                vec![test_proposal(1, 1, false), test_proposal(2, 2, false)],
                vec![],
            )
            .await;

        assert!(result.is_err());
        let err = result.unwrap_err().to_string();
        assert!(err.contains("timed out"), "expected timeout error, got: {err}");
    }
}
