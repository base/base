use std::{collections::BTreeMap, num::NonZeroUsize};

use alloy_consensus::Transaction;
use alloy_eips::{eip2718::Encodable2718, eip4844::FIELD_ELEMENTS_PER_BLOB};
use alloy_genesis::ChainConfig;
use alloy_network::Network;
use alloy_primitives::{B64, B256, keccak256};
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
// ponytail: 256 covers EVM history; increase `l2_lookback` if span-batch overlap needs older blocks.
pub const L2_HEADER_LOOKBACK: u64 = 256;

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

impl WitnessProviders {
    /// Creates a provider bundle.
    pub const fn new(
        l1: alloy_provider::RootProvider,
        l2: alloy_provider::RootProvider<Base>,
        blobs: OnlineBlobProvider<OnlineBeaconClient>,
    ) -> Self {
        Self { l1, l2, blobs }
    }
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
    /// Number of L2 blocks fetched before the agreed head for history reads.
    pub l2_lookback: u64,
}

impl WitnessGenerator {
    /// Creates a generator with three concurrent jobs, matching one proof node's current
    /// `debug_executePayload` limit.
    pub const fn new(config: WitnessConfig, providers: WitnessProviders) -> Self {
        Self {
            config,
            providers,
            concurrency: NonZeroUsize::new(3).expect("3 is non-zero"),
            l2_lookback: L2_HEADER_LOOKBACK,
        }
    }

    /// Overrides the maximum number of concurrent block jobs.
    pub const fn with_concurrency(mut self, concurrency: NonZeroUsize) -> Self {
        self.concurrency = concurrency;
        self
    }

    /// Overrides the L2 history fetched before the agreed head.
    pub const fn with_l2_lookback(mut self, l2_lookback: u64) -> Self {
        self.l2_lookback = l2_lookback;
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

        let agreed_consensus = agreed_block
            .clone()
            .map_header(|header| header.into_inner())
            .into_consensus()
            .map_transactions(|tx| tx.inner.inner.into_inner());
        let safe_head = L2BlockInfo::from_block_and_genesis(
            &agreed_consensus,
            &self.config.rollup_config.genesis,
        )
        .map_err(|error| WitnessError::Encoding(error.to_string()))?;
        let channel_timeout =
            self.config.rollup_config.channel_timeout(safe_head.block_info.timestamp);
        let l1_start = safe_head
            .l1_origin
            .number
            .saturating_sub(channel_timeout)
            .max(self.config.rollup_config.genesis.l1.number);
        let l1_end = self.config.request.l1_head_number;
        if l1_start > l1_end {
            return Err(WitnessError::InvalidL1Ancestry(format!(
                "origin {l1_start} is after head {l1_end}",
            )));
        }

        let l2_start = agreed_number.saturating_sub(self.l2_lookback);
        let (l2_chunks, l1_chunks, output_chunk) = tokio::try_join!(
            self.fetch_l2_range(l2_start, claimed_number, agreed_number),
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

    /// Fetches an L2 range and its execution witnesses with bounded concurrency.
    pub async fn fetch_l2_range(
        &self,
        start: u64,
        end: u64,
        agreed: u64,
    ) -> Result<Vec<(u64, B256, B256, PreimageMap)>> {
        let support_numbers = (start..=agreed).collect::<Vec<_>>();
        let execution_numbers = agreed
            .checked_add(1)
            .filter(|execution_start| execution_start <= &end)
            .map_or_else(Vec::new, |execution_start| (execution_start..=end).collect());
        let (mut support, execution) = tokio::try_join!(
            self.fetch_l2_blocks(support_numbers, false),
            self.fetch_l2_blocks(execution_numbers, true),
        )?;
        support.extend(execution);
        Ok(support)
    }

    /// Fetches the provided L2 block numbers with bounded concurrency.
    pub async fn fetch_l2_blocks(
        &self,
        numbers: Vec<u64>,
        execute: bool,
    ) -> Result<Vec<(u64, B256, B256, PreimageMap)>> {
        stream::iter(numbers)
            .map(|number| self.fetch_l2_block(number, execute))
            .buffer_unordered(self.concurrency.get())
            .try_collect()
            .await
    }

    /// Fetches one L2 block and optionally its execution witness.
    pub async fn fetch_l2_block(
        &self,
        number: u64,
        execute: bool,
    ) -> Result<(u64, B256, B256, PreimageMap)> {
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

        let transactions = block.transactions.clone().into_transactions().collect::<Vec<_>>();
        let encoded_transactions =
            transactions.iter().map(|tx| tx.inner.encoded_2718()).collect::<Vec<_>>();
        let blob_hashes = transactions
            .iter()
            .filter(|tx| tx.inner.to() == Some(self.config.rollup_config.batch_inbox_address))
            .filter_map(|tx| tx.inner.blob_versioned_hashes())
            .flatten()
            .copied()
            .collect::<Vec<_>>();
        let block_info = BlockInfo {
            hash,
            number,
            parent_hash: block.header.inner.parent_hash,
            timestamp: block.header.inner.timestamp,
        };
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
        let (receipts, blobs): (Vec<alloy_primitives::Bytes>, Vec<_>) =
            tokio::try_join!(receipts, blobs)?;

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
        if !blob_hashes.is_empty() {
            for (blob_hash, blob) in blob_hashes.into_iter().zip(blobs) {
                self.insert_blob_preimages(&mut chunk, blob_hash, blob)?;
            }
        }

        Ok((number, hash, block.header.inner.parent_hash, chunk))
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
    use alloy_genesis::ChainConfig;
    use base_common_genesis::RollupConfig;

    use super::*;

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
        let generator = WitnessGenerator::new(config, WitnessProviders::new(l1, l2, blobs));

        assert_eq!(generator.boot_preimages().unwrap().len(), 10);
    }
}
