use std::{
    collections::{BTreeMap, BTreeSet},
    num::NonZeroUsize,
};

use alloy_consensus::Transaction;
use alloy_eips::{
    eip2718::Encodable2718,
    eip2935::{HISTORY_SERVE_WINDOW, HISTORY_STORAGE_ADDRESS},
    eip4844::FIELD_ELEMENTS_PER_BLOB,
};
use alloy_genesis::ChainConfig;
use alloy_network::Network;
use alloy_primitives::{B64, B256, Bytes, U256, keccak256};
use alloy_provider::Provider;
use alloy_rlp::Encodable;
use alloy_rpc_types::{Block, debug::ExecutionWitness};
use ark_ff::{BigInteger, PrimeField};
use base_common_consensus::{HoloceneExtraData, JovianExtraData, Predeploys};
use base_common_genesis::RollupConfig;
use base_common_network::Base;
use base_common_rpc_types_engine::BasePayloadAttributes;
use base_consensus_providers::{
    BlobWithCommitmentAndProof, OnlineBeaconClient, OnlineBlobProvider,
};
use base_proof::{
    INTERMEDIATE_BLOCK_INTERVAL_KEY, L1_CONFIG_KEY, L1_HEAD_KEY, L1_HEAD_NUMBER_KEY,
    L2_CHAIN_ID_KEY, L2_CLAIM_BLOCK_NUMBER_KEY, L2_CLAIM_KEY, L2_OUTPUT_ROOT_KEY,
    L2_ROLLUP_CONFIG_KEY, PROPOSER_KEY, ROOTS_OF_UNITY,
};
use base_proof_preimage::{PreimageKey, PreimageKeyType};
use base_proof_primitives::ProofRequest;
use base_protocol::{BlockInfo, L2BlockInfo, OutputRoot};
use futures::{StreamExt, TryStreamExt, stream};

use crate::{PreimageMap, Result, WitnessError};

/// Number of prior L2 headers available to the EVM `BLOCKHASH` opcode.
pub const L2_HEADER_LOOKBACK: u64 = 256;

/// Maximum number of storage keys accepted by `eth_getProof`.
const MAX_STORAGE_PROOF_KEYS: usize = 100;

/// L2 RPC block type used to reconstruct payload attributes.
pub type L2RpcBlock =
    Block<<Base as Network>::TransactionResponse, <Base as Network>::HeaderResponse>;

/// Inputs that vary by proof or chain.
#[derive(Debug, Clone)]
pub struct WitnessConfig {
    /// Per-proof request.
    pub request: ProofRequest,
    /// L2 chain identifier.
    pub l2_chain_id: u64,
    /// Rollup configuration used by derivation.
    pub rollup_config: RollupConfig,
    /// L1 chain configuration used by the proof client.
    pub l1_config: ChainConfig,
}

/// RPC providers used during witness generation.
#[derive(Debug, Clone)]
pub struct WitnessProviders {
    /// L1 execution provider.
    pub l1: alloy_provider::RootProvider,
    /// L2 execution provider.
    pub l2: alloy_provider::RootProvider<Base>,
    /// L1 beacon blob provider.
    pub blobs: OnlineBlobProvider<OnlineBeaconClient>,
}

/// Builds a complete preimage vector without a host-side proof replay.
#[derive(Debug, Clone)]
pub struct WitnessGenerator {
    /// Proof and chain inputs.
    pub config: WitnessConfig,
    /// Upstream providers.
    pub providers: WitnessProviders,
    /// Maximum concurrent block jobs.
    pub concurrency: NonZeroUsize,
}

impl WitnessGenerator {
    /// Creates a generator with three concurrent jobs, matching one proof node's current
    /// `debug_executePayload` limit.
    pub const fn new(config: WitnessConfig, providers: WitnessProviders) -> Self {
        Self { config, providers, concurrency: NonZeroUsize::new(3).expect("3 is non-zero") }
    }

    /// Overrides the maximum number of concurrent block jobs.
    pub const fn with_concurrency(mut self, concurrency: NonZeroUsize) -> Self {
        self.concurrency = concurrency;
        self
    }

    /// Generates the preimage vector accepted by the existing enclave protocol.
    pub async fn generate(&self) -> Result<Vec<(PreimageKey, Vec<u8>)>> {
        let agreed_block = self.fetch_agreed_l2_block().await?;
        let agreed_number = agreed_block.header.inner.number;
        let claimed_number = self.config.request.claimed_l2_block_number;
        if agreed_number > claimed_number {
            return Err(WitnessError::InvalidRange {
                agreed: agreed_number,
                claimed: claimed_number,
            });
        }

        let safe_head = self.l2_block_info(agreed_block.clone())?;
        let (reset_l2_start, l1_start) = self.find_l2_reset_start(&safe_head).await?;
        let span_l2_start = self.find_l2_span_start(&safe_head, l1_start).await?;
        let l1_end = self.config.request.l1_head_number;
        if l1_start > l1_end {
            return Err(WitnessError::InvalidL1Ancestry(format!(
                "origin {l1_start} is after head {l1_end}",
            )));
        }

        let l2_start = Self::l2_support_start(
            reset_l2_start,
            span_l2_start,
            agreed_number,
            self.config.rollup_config.genesis.l2.number,
        );
        let (l2_chunks, l1_chunks, output_chunk) = tokio::try_join!(
            self.fetch_l2_range(
                l2_start,
                claimed_number,
                agreed_number,
                claimed_number > agreed_number
                    && self.config.rollup_config.is_isthmus_active(safe_head.block_info.timestamp),
            ),
            self.fetch_l1_range(l1_start, l1_end),
            self.build_starting_output(&agreed_block),
        )?;

        self.validate_l2_chain(&l2_chunks, agreed_number)?;
        self.validate_l1_chain(&l1_chunks, &safe_head)?;

        let mut witness = self.boot_preimages()?;
        witness.extend(output_chunk)?;
        for (_, _, _, chunk) in l2_chunks {
            witness.extend(chunk)?;
        }
        for (_, _, _, chunk) in l1_chunks {
            witness.extend(chunk)?;
        }
        Ok(witness.into_preimages())
    }

    /// Fetches and validates the agreed L2 block.
    pub async fn fetch_agreed_l2_block(&self) -> Result<L2RpcBlock> {
        let expected_hash = self.config.request.agreed_l2_head_hash;
        let block = self
            .providers
            .l2
            .get_block_by_hash(expected_hash)
            .full()
            .await
            .map_err(|error| WitnessError::Rpc {
                operation: "fetch agreed L2 block",
                error: error.to_string(),
            })?
            .ok_or(WitnessError::BlockHashNotFound { layer: "L2", hash: expected_hash })?;
        self.validate_l2_block(&block, block.header.inner.number)?;
        if block.header.hash != expected_hash {
            return Err(WitnessError::InvalidBlock {
                layer: "L2",
                number: block.header.inner.number,
                reason: format!(
                    "expected agreed hash {expected_hash}, received {}",
                    block.header.hash
                ),
            });
        }
        Ok(block)
    }

    /// Returns whether an L2 block is old enough to anchor the derivation pipeline reset.
    pub const fn is_initial_reset_anchor(
        l1_origin_number: u64,
        safe_l1_origin_number: u64,
        channel_timeout: u64,
    ) -> bool {
        l1_origin_number.saturating_add(channel_timeout) <= safe_l1_origin_number
    }

    /// Finds the L2 reset anchor and its L1 origin for the agreed safe head.
    ///
    /// L1 origins are monotonic on the canonical L2 chain, so this only probes logarithmically
    /// many blocks before fetching the complete support range.
    pub async fn find_l2_reset_start(&self, safe_head: &L2BlockInfo) -> Result<(u64, u64)> {
        let l1_origin_timestamp_lower_bound = safe_head.block_info.timestamp.saturating_sub(
            self.config.rollup_config.max_sequencer_drift(safe_head.block_info.timestamp),
        );
        let channel_timeout =
            self.config.rollup_config.channel_timeout(l1_origin_timestamp_lower_bound);
        let l2_genesis = self.config.rollup_config.genesis.l2.number;
        let genesis = self.fetch_l2_block_info(l2_genesis).await?;
        let mut reset_start = (l2_genesis, genesis.l1_origin.number);
        if !Self::is_initial_reset_anchor(
            genesis.l1_origin.number,
            safe_head.l1_origin.number,
            channel_timeout,
        ) {
            return Ok(reset_start);
        }

        let mut lower = l2_genesis.saturating_add(1);
        let mut upper = safe_head.block_info.number;

        while lower <= upper {
            let number = lower + (upper - lower) / 2;
            let block = if number == safe_head.block_info.number {
                *safe_head
            } else {
                self.fetch_l2_block_info(number).await?
            };
            if Self::is_initial_reset_anchor(
                block.l1_origin.number,
                safe_head.l1_origin.number,
                channel_timeout,
            ) {
                reset_start = (number, block.l1_origin.number);
                if number == upper {
                    break;
                }
                lower = number + 1;
            } else {
                upper = number - 1;
            }
        }

        Ok(reset_start)
    }

    /// Selects the earliest L2 block required by reset recovery, span validation, and
    /// `BLOCKHASH` reads.
    pub fn l2_support_start(
        reset_start: u64,
        span_start: u64,
        agreed: u64,
        l2_genesis: u64,
    ) -> u64 {
        reset_start.min(span_start).min(agreed.saturating_sub(L2_HEADER_LOOKBACK)).max(l2_genesis)
    }

    /// Returns the earliest L1 origin a valid span batch's parent can have.
    pub const fn earliest_span_parent_l1_origin(l1_start: u64, seq_window_size: u64) -> u64 {
        l1_start.saturating_sub(seq_window_size).saturating_sub(1)
    }

    /// Finds the earliest L2 header that can be required to validate a span batch.
    ///
    /// A batch included at the reset origin may use the whole sequencing window and may build on
    /// the preceding L1 origin. L2 origins are monotonic, so binary search finds the first
    /// header in that parent-history range without fetching every intervening block.
    pub async fn find_l2_span_start(&self, safe_head: &L2BlockInfo, l1_start: u64) -> Result<u64> {
        let l2_genesis = self.config.rollup_config.genesis.l2.number;
        let min_parent_origin = Self::earliest_span_parent_l1_origin(
            l1_start,
            self.config.rollup_config.seq_window_size,
        );
        if min_parent_origin <= self.config.rollup_config.genesis.l1.number {
            return Ok(l2_genesis);
        }

        let mut lower = l2_genesis;
        let mut upper = safe_head.block_info.number;
        while lower < upper {
            let number = lower + (upper - lower) / 2;
            let block = if number == safe_head.block_info.number {
                *safe_head
            } else {
                self.fetch_l2_block_info(number).await?
            };
            if block.l1_origin.number < min_parent_origin {
                lower = number + 1;
            } else {
                upper = number;
            }
        }
        Ok(lower)
    }

    /// Fetches an L2 range and its execution witnesses with bounded concurrency.
    pub async fn fetch_l2_range(
        &self,
        start: u64,
        end: u64,
        agreed: u64,
        fetch_eip_2935_proofs: bool,
    ) -> Result<Vec<(u64, B256, B256, PreimageMap)>> {
        let execution_end =
            if agreed < end { end.min(self.fetch_l2_head_number().await?) } else { agreed };
        let history_proofs = async {
            if fetch_eip_2935_proofs {
                self.fetch_eip_2935_proofs(agreed, start..agreed).await
            } else {
                Ok(PreimageMap::new())
            }
        };
        let (mut support, execution, history_proofs) = tokio::try_join!(
            self.fetch_l2_blocks(start..=agreed, false),
            self.fetch_l2_blocks((agreed..execution_end).map(|number| number + 1), true),
            history_proofs,
        )?;
        support
            .iter_mut()
            .find(|(number, _, _, _)| *number == agreed)
            .expect("support range must contain agreed block")
            .3
            .extend(history_proofs)?;
        support.extend(execution);
        Ok(support)
    }

    /// Fetches EIP-2935 account and storage proof nodes at the agreed L2 block.
    pub async fn fetch_eip_2935_proofs(
        &self,
        agreed_block_number: u64,
        target_numbers: impl IntoIterator<Item = u64>,
    ) -> Result<PreimageMap> {
        let slots = Self::eip_2935_storage_slots(agreed_block_number, target_numbers);
        let proofs = stream::iter(slots.chunks(MAX_STORAGE_PROOF_KEYS))
            .map(|slots| async move {
                let proof = self
                    .providers
                    .l2
                    .get_proof(HISTORY_STORAGE_ADDRESS, slots.to_vec())
                    .block_id(self.config.request.agreed_l2_head_hash.into())
                    .await
                    .map_err(|error| WitnessError::Rpc {
                        operation: "fetch EIP-2935 storage proof",
                        error: error.to_string(),
                    })?;
                let mut chunk = PreimageMap::new();
                for node in proof.account_proof {
                    chunk.insert_keccak(node.into())?;
                }
                for storage_proof in proof.storage_proof {
                    for node in storage_proof.proof {
                        chunk.insert_keccak(node.into())?;
                    }
                }
                Ok(chunk)
            })
            .buffer_unordered(self.concurrency.get())
            .try_collect::<Vec<_>>()
            .await?;
        let mut chunk = PreimageMap::new();
        for proof in proofs {
            chunk.extend(proof)?;
        }
        Ok(chunk)
    }

    /// Returns EIP-2935 storage keys for historical L2 block numbers at an agreed block.
    pub fn eip_2935_storage_slots(
        agreed_block_number: u64,
        target_numbers: impl IntoIterator<Item = u64>,
    ) -> Vec<B256> {
        target_numbers
            .into_iter()
            .map(|number| {
                let slot =
                    if agreed_block_number.saturating_sub(number) <= HISTORY_SERVE_WINDOW as u64 {
                        number
                    } else {
                        agreed_block_number
                    } % HISTORY_SERVE_WINDOW as u64;
                B256::from(U256::from(slot).to_be_bytes())
            })
            .collect::<BTreeSet<_>>()
            .into_iter()
            .collect()
    }

    /// Fetches the provided L2 block numbers with bounded concurrency.
    pub async fn fetch_l2_blocks(
        &self,
        numbers: impl IntoIterator<Item = u64>,
        execute: bool,
    ) -> Result<Vec<(u64, B256, B256, PreimageMap)>> {
        stream::iter(numbers)
            .map(|number| self.fetch_l2_block(number, execute))
            .buffer_unordered(self.concurrency.get())
            .try_collect()
            .await
    }

    /// Fetches the current canonical L2 head number.
    pub async fn fetch_l2_head_number(&self) -> Result<u64> {
        self.providers.l2.get_block_number().await.map_err(|error| WitnessError::Rpc {
            operation: "fetch L2 head",
            error: error.to_string(),
        })
    }

    /// Fetches one L2 block and optionally its execution witness.
    pub async fn fetch_l2_block(
        &self,
        number: u64,
        execute: bool,
    ) -> Result<(u64, B256, B256, PreimageMap)> {
        let block = self.fetch_l2_rpc_block(number).await?;

        let hash = block.header.hash;
        let parent_hash = block.header.inner.parent_hash;
        let mut chunk = PreimageMap::new();
        let mut raw_header = Vec::new();
        block.header.inner.encode(&mut raw_header);
        chunk.insert_keccak(raw_header)?;

        let encoded_transactions = block
            .transactions
            .clone()
            .into_transactions()
            .map(|tx| tx.inner.inner.encoded_2718())
            .collect::<Vec<_>>();
        chunk.insert_ordered_trie(&encoded_transactions)?;

        if execute {
            let attributes = self.payload_attributes_from_l2_block(block)?;
            let execution: ExecutionWitness = self
                .providers
                .l2
                .client()
                .request("debug_executePayload", (parent_hash, attributes))
                .await
                .map_err(|error| WitnessError::Rpc {
                    operation: "debug_executePayload",
                    error: error.to_string(),
                })?;
            // Reth does not implement `debug_dbGet`, so omitted nodes cannot be recovered as
            // they are in the legacy host. `debug_executePayload` must return a complete witness.
            for preimage in execution
                .state
                .into_iter()
                .chain(execution.codes)
                .chain(execution.keys)
                .chain(execution.headers)
            {
                chunk.insert_keccak(preimage.into())?;
            }
        }

        Ok((number, hash, parent_hash, chunk))
    }

    /// Fetches and validates an L2 block by number.
    pub async fn fetch_l2_rpc_block(&self, number: u64) -> Result<L2RpcBlock> {
        let block = self
            .providers
            .l2
            .get_block_by_number(number.into())
            .full()
            .await
            .map_err(|error| WitnessError::Rpc {
                operation: "fetch L2 block",
                error: error.to_string(),
            })?
            .ok_or(WitnessError::BlockNotFound { layer: "L2", number })?;
        self.validate_l2_block(&block, number)?;
        Ok(block)
    }

    /// Fetches the L1 origin information encoded in an L2 block.
    pub async fn fetch_l2_block_info(&self, number: u64) -> Result<L2BlockInfo> {
        self.l2_block_info(self.fetch_l2_rpc_block(number).await?)
    }

    /// Decodes the L1 origin information encoded in an L2 block.
    pub fn l2_block_info(&self, block: L2RpcBlock) -> Result<L2BlockInfo> {
        let block = block
            .map_header(|header| header.into_inner())
            .into_consensus()
            .map_transactions(|tx| tx.inner.inner.into_inner());
        L2BlockInfo::from_block_and_genesis(&block, &self.config.rollup_config.genesis)
            .map_err(|error| WitnessError::Encoding(error.to_string()))
    }

    /// Validates an L2 RPC block's number and hash.
    pub fn validate_l2_block(&self, block: &L2RpcBlock, number: u64) -> Result<()> {
        if block.header.inner.number != number {
            return Err(WitnessError::InvalidBlock {
                layer: "L2",
                number,
                reason: format!("received block number {}", block.header.inner.number),
            });
        }
        let actual_hash = block.header.inner.hash_slow();
        if block.header.hash != actual_hash {
            return Err(WitnessError::InvalidBlock {
                layer: "L2",
                number,
                reason: format!(
                    "reported hash {} does not match encoded hash {actual_hash}",
                    block.header.hash
                ),
            });
        }
        Ok(())
    }

    /// Validates the fetched L2 parent chain and agreed hash.
    pub fn validate_l2_chain(
        &self,
        chunks: &[(u64, B256, B256, PreimageMap)],
        agreed: u64,
    ) -> Result<()> {
        let blocks = chunks
            .iter()
            .map(|(number, hash, parent, _)| (*number, (*hash, *parent)))
            .collect::<BTreeMap<_, _>>();
        for number in blocks.keys().copied().skip(1) {
            let (_, parent) = blocks[&number];
            let previous = blocks.get(&number.saturating_sub(1)).ok_or_else(|| {
                WitnessError::InvalidBlock {
                    layer: "L2",
                    number,
                    reason: "missing parent block".to_string(),
                }
            })?;
            if parent != previous.0 {
                return Err(WitnessError::InvalidBlock {
                    layer: "L2",
                    number,
                    reason: format!("parent {parent} does not match {}", previous.0),
                });
            }
        }
        if blocks.get(&agreed).map(|(hash, _)| *hash)
            != Some(self.config.request.agreed_l2_head_hash)
        {
            return Err(WitnessError::InvalidBlock {
                layer: "L2",
                number: agreed,
                reason: "agreed block hash mismatch".to_string(),
            });
        }
        Ok(())
    }

    /// Fetches an L1 range, including transaction, receipt, and blob preimages.
    pub async fn fetch_l1_range(
        &self,
        start: u64,
        end: u64,
    ) -> Result<Vec<(u64, B256, B256, PreimageMap)>> {
        stream::iter(start..=end)
            .map(|number| self.fetch_l1_block(number))
            .buffer_unordered(self.concurrency.get())
            .try_collect()
            .await
    }

    /// Fetches one L1 block and all derivation preimages addressable from it.
    pub async fn fetch_l1_block(&self, number: u64) -> Result<(u64, B256, B256, PreimageMap)> {
        let block = self
            .providers
            .l1
            .get_block_by_number(number.into())
            .full()
            .await
            .map_err(|error| WitnessError::Rpc {
                operation: "fetch L1 block",
                error: error.to_string(),
            })?
            .ok_or(WitnessError::BlockNotFound { layer: "L1", number })?;
        if block.header.number != number {
            return Err(WitnessError::InvalidBlock {
                layer: "L1",
                number,
                reason: format!("received block number {}", block.header.number),
            });
        }
        let hash = block.header.hash;
        let actual_hash = block.header.inner.hash_slow();
        if hash != actual_hash {
            return Err(WitnessError::InvalidBlock {
                layer: "L1",
                number,
                reason: format!("reported hash {hash} does not match encoded hash {actual_hash}"),
            });
        }

        let transactions = block.transactions.into_transactions().collect::<Vec<_>>();
        let encoded_transactions =
            transactions.iter().map(|tx| tx.inner.encoded_2718()).collect::<Vec<_>>();
        // ponytail: inbox-only filtering is a safe superset; restore batcher tracking if
        // unauthorized inbox blobs measurably inflate witnesses.
        let blob_hashes = transactions
            .iter()
            .filter(|tx| tx.inner.to() == Some(self.config.rollup_config.batch_inbox_address))
            .filter_map(|tx| tx.inner.blob_versioned_hashes())
            .flatten()
            .copied()
            .collect::<Vec<_>>();
        let parent_hash = block.header.inner.parent_hash;
        let block_info =
            BlockInfo { hash, number, parent_hash, timestamp: block.header.inner.timestamp };
        let receipts = async {
            self.providers.l1.client().request("debug_getRawReceipts", [hash]).await.map_err(
                |error| WitnessError::Rpc {
                    operation: "debug_getRawReceipts",
                    error: error.to_string(),
                },
            )
        };
        let blobs = async {
            if blob_hashes.is_empty() {
                return Ok(Vec::new());
            }
            self.providers.blobs.fetch_blobs_with_proofs(&block_info, &blob_hashes).await.map_err(
                |error| WitnessError::Rpc {
                    operation: "fetch blob sidecars",
                    error: error.to_string(),
                },
            )
        };
        let (receipts, blobs): (Vec<Bytes>, Vec<_>) = tokio::try_join!(receipts, blobs)?;

        let mut chunk = PreimageMap::new();
        let mut raw_header = Vec::new();
        block.header.inner.encode(&mut raw_header);
        chunk.insert_keccak(raw_header)?;
        chunk.insert_ordered_trie(&encoded_transactions)?;
        chunk.insert_ordered_trie(&receipts)?;

        if blobs.len() != blob_hashes.len() {
            return Err(WitnessError::BlobCountMismatch {
                expected: blob_hashes.len(),
                actual: blobs.len(),
            });
        }
        for (blob_hash, blob) in blob_hashes.into_iter().zip(blobs) {
            self.insert_blob_preimages(&mut chunk, blob_hash, blob)?;
        }

        Ok((number, hash, parent_hash, chunk))
    }

    /// Validates the fetched L1 parent chain and its safe origin/head anchors.
    pub fn validate_l1_chain(
        &self,
        chunks: &[(u64, B256, B256, PreimageMap)],
        safe_head: &L2BlockInfo,
    ) -> Result<()> {
        let blocks = chunks
            .iter()
            .map(|(number, hash, parent, _)| (*number, (*hash, *parent)))
            .collect::<BTreeMap<_, _>>();
        for number in blocks.keys().copied().skip(1) {
            let (_, parent) = blocks[&number];
            let previous = blocks.get(&number.saturating_sub(1)).ok_or_else(|| {
                WitnessError::InvalidL1Ancestry(format!("missing block {}", number - 1))
            })?;
            if parent != previous.0 {
                return Err(WitnessError::InvalidL1Ancestry(format!(
                    "block {number} parent {parent} does not match {}",
                    previous.0
                )));
            }
        }
        if blocks.get(&safe_head.l1_origin.number).map(|(hash, _)| *hash)
            != Some(safe_head.l1_origin.hash)
        {
            return Err(WitnessError::InvalidL1Ancestry(
                "safe L1 origin hash mismatch".to_string(),
            ));
        }
        if blocks.get(&self.config.request.l1_head_number).map(|(hash, _)| *hash)
            != Some(self.config.request.l1_head)
        {
            return Err(WitnessError::InvalidL1Ancestry("L1 head hash mismatch".to_string()));
        }
        Ok(())
    }

    /// Constructs and validates the agreed starting output preimage.
    pub async fn build_starting_output(&self, agreed: &L2RpcBlock) -> Result<PreimageMap> {
        let proof = self
            .providers
            .l2
            .get_proof(Predeploys::L2_TO_L1_MESSAGE_PASSER, Vec::new())
            .block_id(self.config.request.agreed_l2_head_hash.into())
            .await
            .map_err(|error| WitnessError::Rpc {
                operation: "fetch starting output proof",
                error: error.to_string(),
            })?;
        let output = OutputRoot::from_parts(
            agreed.header.inner.state_root,
            proof.storage_hash,
            self.config.request.agreed_l2_head_hash,
        );
        if output.hash() != self.config.request.agreed_l2_output_root {
            return Err(WitnessError::OutputRootMismatch);
        }
        let mut chunk = PreimageMap::new();
        chunk.insert_keccak(output.encode().into())?;
        Ok(chunk)
    }

    /// Constructs all local boot preimages read by `BootInfo`.
    pub fn boot_preimages(&self) -> Result<PreimageMap> {
        let request = &self.config.request;
        let mut chunk = PreimageMap::new();
        let entries = [
            (L1_HEAD_KEY.to::<u64>(), request.l1_head.to_vec()),
            (L2_OUTPUT_ROOT_KEY.to::<u64>(), request.agreed_l2_output_root.to_vec()),
            (L2_CLAIM_KEY.to::<u64>(), request.claimed_l2_output_root.to_vec()),
            (
                L2_CLAIM_BLOCK_NUMBER_KEY.to::<u64>(),
                request.claimed_l2_block_number.to_be_bytes().to_vec(),
            ),
            (L2_CHAIN_ID_KEY.to::<u64>(), self.config.l2_chain_id.to_be_bytes().to_vec()),
            (
                L2_ROLLUP_CONFIG_KEY.to::<u64>(),
                serde_json::to_vec(&self.config.rollup_config)
                    .map_err(|error| WitnessError::Encoding(error.to_string()))?,
            ),
            (
                L1_CONFIG_KEY.to::<u64>(),
                serde_json::to_vec(&self.config.l1_config)
                    .map_err(|error| WitnessError::Encoding(error.to_string()))?,
            ),
            (PROPOSER_KEY.to::<u64>(), request.proposer.to_vec()),
            (
                INTERMEDIATE_BLOCK_INTERVAL_KEY.to::<u64>(),
                request.intermediate_block_interval.to_be_bytes().to_vec(),
            ),
            (L1_HEAD_NUMBER_KEY.to::<u64>(), request.l1_head_number.to_be_bytes().to_vec()),
        ];
        for (key, value) in entries {
            chunk.insert(PreimageKey::new_local(key), value)?;
        }
        Ok(chunk)
    }

    /// Converts a canonical L2 block into the attributes consumed by `debug_executePayload`.
    pub fn payload_attributes_from_l2_block(
        &self,
        block: L2RpcBlock,
    ) -> Result<BasePayloadAttributes> {
        let timestamp = block.header.inner.timestamp;
        let mut attributes = BasePayloadAttributes::default();
        attributes.payload_attributes.timestamp = timestamp;
        attributes.payload_attributes.prev_randao = block.header.inner.mix_hash;
        attributes.payload_attributes.suggested_fee_recipient = block.header.inner.beneficiary;
        attributes.payload_attributes.parent_beacon_block_root =
            block.header.inner.parent_beacon_block_root;
        attributes.payload_attributes.withdrawals =
            block.withdrawals.as_ref().map(|withdrawals| withdrawals.0.clone());
        attributes.transactions = Some(
            block
                .transactions
                .into_transactions()
                .map(|tx| tx.as_ref().encoded_2718().into())
                .collect(),
        );
        attributes.no_tx_pool = Some(true);
        attributes.gas_limit = Some(block.header.inner.gas_limit);

        if self.config.rollup_config.is_jovian_active(timestamp) {
            let (elasticity, denominator, min_base_fee) =
                JovianExtraData::decode(&block.header.inner.extra_data)
                    .map_err(|error| WitnessError::Encoding(error.to_string()))?;
            attributes.eip_1559_params =
                Some(Self::encode_payload_eip_1559_params(elasticity, denominator));
            attributes.min_base_fee = Some(min_base_fee);
        } else if self.config.rollup_config.is_holocene_active(timestamp) {
            let (elasticity, denominator) =
                HoloceneExtraData::decode(&block.header.inner.extra_data)
                    .map_err(|error| WitnessError::Encoding(error.to_string()))?;
            attributes.eip_1559_params =
                Some(Self::encode_payload_eip_1559_params(elasticity, denominator));
        }

        Ok(attributes)
    }

    /// Encodes Holocene/Jovian EIP-1559 parameters for payload attributes.
    pub fn encode_payload_eip_1559_params(elasticity: u32, denominator: u32) -> B64 {
        let mut encoded = [0u8; 8];
        encoded[..4].copy_from_slice(&denominator.to_be_bytes());
        encoded[4..].copy_from_slice(&elasticity.to_be_bytes());
        B64::from(encoded)
    }

    /// Inserts all oracle preimages for one blob sidecar.
    pub fn insert_blob_preimages(
        &self,
        chunk: &mut PreimageMap,
        blob_hash: B256,
        blob: BlobWithCommitmentAndProof,
    ) -> Result<()> {
        let BlobWithCommitmentAndProof { blob, kzg_commitment, kzg_proof } = blob;
        chunk.insert(
            PreimageKey::new(*blob_hash, PreimageKeyType::Sha256),
            kzg_commitment.to_vec(),
        )?;

        let mut blob_key = [0u8; 80];
        blob_key[..48].copy_from_slice(kzg_commitment.as_ref());
        for index in 0..FIELD_ELEMENTS_PER_BLOB {
            blob_key[48..].copy_from_slice(
                ROOTS_OF_UNITY[index as usize].into_bigint().to_bytes_be().as_ref(),
            );
            let key_hash = keccak256(blob_key);
            chunk.insert_keccak(blob_key.to_vec())?;
            chunk.insert(
                PreimageKey::new(*key_hash, PreimageKeyType::Blob),
                blob.as_ref()[(index as usize) << 5..(index as usize + 1) << 5].to_vec(),
            )?;
        }

        blob_key[72..].copy_from_slice(FIELD_ELEMENTS_PER_BLOB.to_be_bytes().as_ref());
        let proof_key_hash = keccak256(blob_key);
        chunk.insert_keccak(blob_key.to_vec())?;
        chunk
            .insert(PreimageKey::new(*proof_key_hash, PreimageKeyType::Blob), kzg_proof.to_vec())?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use std::{env, thread, time::Instant};

    use alloy_eips::BlockNumberOrTag;
    use alloy_genesis::ChainConfig;
    use alloy_provider::Provider;
    use base_common_chains::L1_CONFIGS;
    use base_common_genesis::RollupConfig;
    use base_proof_host::{Host, HostConfig, ProverConfig};
    use base_proof_preimage::WitnessOracle;
    use base_proof_tee_nitro_enclave::Oracle;

    use super::*;

    const MAINNET_L2_CHAIN_ID: u64 = 8453;
    const MAINNET_BLOCK_RANGE: u64 = 600;
    const MAINNET_INTERMEDIATE_ROOT_INTERVAL: u64 = 30;
    const WITNESS_BENCH_STACK_SIZE: usize = 32 * 1024 * 1024;

    #[test]
    fn boot_preimages_contains_every_local_input() {
        let config = WitnessConfig {
            request: ProofRequest::default(),
            l2_chain_id: 8453,
            rollup_config: RollupConfig::default(),
            l1_config: ChainConfig::default(),
        };
        let l1 = alloy_provider::RootProvider::new_http("http://127.0.0.1:1".parse().unwrap());
        let l2 = alloy_provider::RootProvider::new_http("http://127.0.0.1:1".parse().unwrap());
        let blobs = OnlineBlobProvider {
            beacon_client: OnlineBeaconClient::new_http("http://127.0.0.1:1".to_string()),
            genesis_time: 0,
            slot_interval: 12,
        };
        let generator = WitnessGenerator::new(config, WitnessProviders { l1, l2, blobs });

        assert_eq!(generator.boot_preimages().unwrap().len(), 10);
    }

    #[test]
    fn eip_2935_storage_slots_wrap_and_deduplicate() {
        let agreed = HISTORY_SERVE_WINDOW as u64 * 2 + 10;
        assert_eq!(
            WitnessGenerator::eip_2935_storage_slots(
                agreed,
                vec![0, HISTORY_SERVE_WINDOW as u64 + 11, agreed - HISTORY_SERVE_WINDOW as u64],
            ),
            vec![B256::with_last_byte(10), B256::with_last_byte(11)],
        );
    }

    #[test]
    fn l2_support_extends_to_the_initial_reset_anchor() {
        assert!(WitnessGenerator::is_initial_reset_anchor(950, 1_000, 50));
        assert!(!WitnessGenerator::is_initial_reset_anchor(951, 1_000, 50));
        assert_eq!(WitnessGenerator::l2_support_start(700, 900, 1_000, 0), 700);
    }

    #[test]
    fn l2_support_extends_to_the_span_parent_before_the_header_lookback() {
        assert_eq!(WitnessGenerator::earliest_span_parent_l1_origin(1_000, 300), 699);
        assert_eq!(WitnessGenerator::l2_support_start(900, 700, 1_000, 0), 700);
    }

    #[test]
    #[ignore = "requires archive mainnet RPCs; see the crate README for the required environment"]
    fn benchmark_mainnet_witness_generation() {
        thread::Builder::new()
            .name("witness-benchmark".to_string())
            .stack_size(WITNESS_BENCH_STACK_SIZE)
            .spawn(|| {
                tokio::runtime::Builder::new_multi_thread()
                    .enable_all()
                    .build()
                    .expect("start witness benchmark runtime")
                    .block_on(benchmark_mainnet_witness_generation_inner());
            })
            .expect("start witness benchmark thread")
            .join()
            .expect("witness benchmark thread must not panic");
    }

    async fn benchmark_mainnet_witness_generation_inner() {
        let l1_eth_url = required_env("L1_ETH_URL");
        let l2_eth_url = required_env("L2_ETH_URL");
        let l2_node_url = required_env("L2_NODE_URL");
        let l1_beacon_url = required_env("L1_BEACON_URL");
        let block_range = env::var("WITNESS_BENCH_BLOCK_RANGE")
            .map_or(MAINNET_BLOCK_RANGE, |value| {
                value.parse().expect("WITNESS_BENCH_BLOCK_RANGE must be a positive integer")
            });
        assert!(block_range > 0, "WITNESS_BENCH_BLOCK_RANGE must be greater than zero");
        let l1 = alloy_provider::RootProvider::new_http(
            l1_eth_url.parse().expect("L1_ETH_URL must be a valid URL"),
        );
        let l2 = alloy_provider::RootProvider::new_http(
            l2_eth_url.parse().expect("L2_ETH_URL must be a valid URL"),
        );
        let rollup_config = mainnet_rollup_config(&l2_node_url).await;
        assert_eq!(rollup_config.l2_chain_id.id(), MAINNET_L2_CHAIN_ID);
        let l1_config = L1_CONFIGS
            .get(&rollup_config.l1_chain_id)
            .expect("mainnet rollup config must use a known L1 config")
            .clone();
        let request = mainnet_request(&l1, &l2, &rollup_config, block_range).await;

        let legacy_host = Host::new(HostConfig {
            request: request.clone(),
            prover: ProverConfig {
                l1_eth_url: l1_eth_url.clone(),
                l2_eth_url: l2_eth_url.clone(),
                l2_node_url: l2_node_url.clone(),
                l1_beacon_url: l1_beacon_url.clone(),
                l2_chain_id: MAINNET_L2_CHAIN_ID,
                rollup_config: rollup_config.clone(),
                l1_config: l1_config.clone(),
                enable_experimental_witness_endpoint: true,
            },
            data_dir: None,
        });
        let legacy_start = Instant::now();
        let legacy_witness = legacy_host
            .build_witness(Oracle::empty())
            .await
            .expect("Nitro host witness generation must succeed");
        let legacy_elapsed = legacy_start.elapsed();

        let generator_start = Instant::now();
        let blobs = OnlineBlobProvider::init(OnlineBeaconClient::new_http(l1_beacon_url)).await;
        let generator = WitnessGenerator::new(
            WitnessConfig { request, l2_chain_id: MAINNET_L2_CHAIN_ID, rollup_config, l1_config },
            WitnessProviders { l1, l2, blobs },
        );
        let generated_witness =
            generator.generate().await.expect("parallel witness generation must succeed");
        let generator_elapsed = generator_start.elapsed();
        let speedup = legacy_elapsed.as_secs_f64() / generator_elapsed.as_secs_f64();
        let legacy_preimage_count = legacy_witness.preimage_count().expect("legacy witness count");
        let generated_preimage_count = generated_witness.len();

        assert!(generated_preimage_count > 0);
        assert!(legacy_preimage_count > 0);
        println!(
            "mainnet witness benchmark (range: {block_range} L2 blocks)\n  Nitro host replay: {legacy_elapsed:?} ({legacy_preimage_count} preimages)\n  parallel generator: {generator_elapsed:?} ({generated_preimage_count} preimages)\n  speedup: {speedup:.2}x",
        );
    }

    async fn mainnet_request(
        l1: &alloy_provider::RootProvider,
        l2: &alloy_provider::RootProvider<Base>,
        rollup_config: &RollupConfig,
        block_range: u64,
    ) -> ProofRequest {
        let claimed = l2
            .get_block_by_number(BlockNumberOrTag::Safe)
            .full()
            .await
            .expect("fetch mainnet safe L2 block")
            .expect("mainnet safe L2 block must exist");
        let agreed_number = claimed
            .header
            .inner
            .number
            .checked_sub(block_range)
            .expect("mainnet safe L2 block must cover the requested range");
        let agreed = l2
            .get_block_by_number(agreed_number.into())
            .full()
            .await
            .expect("fetch agreed L2 block")
            .expect("agreed L2 block must exist");
        let claimed_l2_info = claimed
            .clone()
            .map_header(|header| header.into_inner())
            .into_consensus()
            .map_transactions(|tx| tx.inner.inner.into_inner());
        let claimed_l2_info =
            L2BlockInfo::from_block_and_genesis(&claimed_l2_info, &rollup_config.genesis)
                .expect("safe L2 block must encode L1 origin information");
        let l1_head = l1
            .get_block_by_number(BlockNumberOrTag::Latest)
            .await
            .expect("fetch latest L1 block")
            .expect("latest L1 block must exist");
        assert!(
            l1_head.header.number >= claimed_l2_info.l1_origin.number,
            "latest L1 block must not precede the claimed L2 origin"
        );
        let (agreed_l2_output_root, claimed_l2_output_root) =
            tokio::join!(output_root(l2, &agreed), output_root(l2, &claimed),);

        ProofRequest {
            l1_head: l1_head.header.hash,
            agreed_l2_head_hash: agreed.header.hash,
            agreed_l2_output_root,
            claimed_l2_output_root,
            claimed_l2_block_number: claimed.header.inner.number,
            intermediate_block_interval: MAINNET_INTERMEDIATE_ROOT_INTERVAL,
            l1_head_number: l1_head.header.number,
            ..Default::default()
        }
    }

    async fn output_root(l2: &alloy_provider::RootProvider<Base>, block: &L2RpcBlock) -> B256 {
        let proof = l2
            .get_proof(Predeploys::L2_TO_L1_MESSAGE_PASSER, Vec::new())
            .block_id(block.header.hash.into())
            .await
            .expect("fetch L2ToL1MessagePasser proof");

        OutputRoot::from_parts(block.header.inner.state_root, proof.storage_hash, block.header.hash)
            .hash()
    }

    async fn mainnet_rollup_config(l2_node_url: &str) -> RollupConfig {
        let l2_node: alloy_provider::RootProvider = alloy_provider::RootProvider::new_http(
            l2_node_url.parse().expect("L2_NODE_URL must be a valid URL"),
        );

        l2_node
            .client()
            .request("optimism_rollupConfig", ())
            .await
            .expect("L2_NODE_URL must return an optimism rollup config")
    }

    fn required_env(name: &str) -> String {
        env::var(name).unwrap_or_else(|_| panic!("{name} must be set"))
    }
}
