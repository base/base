use std::time::SystemTime;

use alloy_consensus::Block;
use alloy_eips::eip7685::EMPTY_REQUESTS_HASH;
use alloy_primitives::{Address, B256, keccak256};
use alloy_rpc_types_engine::{ExecutionPayloadV3, PayloadError};
use base_common_consensus::BaseTxEnvelope;
use base_common_genesis::RollupConfig;
use base_common_rpc_types_engine::{
    BaseExecutionPayload, BaseExecutionPayloadV4, BasePayloadError, NetworkPayloadEnvelope,
};
use base_protocol::{BaseTimeScheduleError, BaseTimeUpdateTx};
use libp2p::gossipsub::MessageAcceptance;

use super::BlockHandler;
use crate::Metrics;

/// Error that can occur when validating a block.
#[derive(Debug, thiserror::Error)]
pub enum BlockInvalidError {
    /// The block has an invalid timestamp.
    #[error("Invalid timestamp. Current: {current}, Received: {received}")]
    Timestamp {
        /// The current timestamp.
        current: u64,
        /// The received timestamp.
        received: u64,
    },
    /// The block has an invalid base fee per gas.
    #[error("Base fee per gas overflow")]
    BaseFeePerGasOverflow(#[from] PayloadError),
    /// The block has an invalid hash.
    #[error("Invalid block hash. Expected: {expected}, Received: {received}")]
    BlockHash {
        /// The expected block hash.
        expected: B256,
        /// The received block hash.
        received: B256,
    },
    /// The block has an invalid signature.
    #[error("Invalid signature.")]
    Signature,
    /// The block has an invalid signer.
    #[error("Invalid signer, expected: {expected}, received: {received}")]
    Signer {
        /// The expected signer.
        expected: Address,
        /// The received signer.
        received: Address,
    },
    /// Invalid block.
    #[error(transparent)]
    InvalidBlock(#[from] BasePayloadError),
    /// The block timestamp does not match the absolute Denim schedule.
    #[error(transparent)]
    InvalidBaseTimeSchedule(#[from] BaseTimeScheduleError),
    /// The block has an invalid parent beacon block root.
    #[error("Payload is on v3+ topic, but has empty parent beacon root")]
    ParentBeaconRoot,
    /// The block has an invalid blob gas used.
    #[error("Payload is on v3+ topic, but has non-zero blob gas used")]
    BlobGasUsed,
    /// The block has an invalid excess blob gas.
    #[error("Payload is on v3+ topic, but has non-zero excess blob gas")]
    ExcessBlobGas,
    /// The block has an invalid withdrawals root.
    #[error("Payload is on v4+ topic, but has non-empty withdrawals root")]
    WithdrawalsRoot,
    /// The carried block access list does not hash to the header's `block_access_list_hash`.
    #[error("Block access list hash mismatch. Header: {header}, Carried: {carried}")]
    BlockAccessListHashMismatch {
        /// The `block_access_list_hash` committed to by the header.
        header: B256,
        /// The `keccak256` hash of the block access list carried by the payload.
        carried: B256,
    },
    /// The payload and the header disagree on whether a block access list is present.
    #[error(
        "Block access list presence mismatch. Payload carries list: {payload_has_list}, header has commitment: {header_has_commitment}"
    )]
    BlockAccessListPresence {
        /// Whether the payload version carries a block access list (V5 only).
        payload_has_list: bool,
        /// Whether the header carries a `block_access_list_hash`.
        header_has_commitment: bool,
    },
    /// Too many blocks were validated for the same height.
    ///
    /// No longer emitted: the per-height seen-hash cache evicts its oldest entry and admits the
    /// newest block instead of rejecting it, so earlier candidates cannot starve the honest block
    /// for a height. The variant and its `too_many_blocks` metric label remain part of the public
    /// error and metric surface.
    #[error("Too many blocks seen for height {height}")]
    TooManyBlocks {
        /// The height of the block.
        height: u64,
    },
    /// The block has already been seen.
    #[error("Block seen before")]
    BlockSeen {
        /// The hash of the block.
        block_hash: B256,
    },
}

impl From<BlockInvalidError> for MessageAcceptance {
    fn from(value: BlockInvalidError) -> Self {
        // We only want to ignore blocks that we have already seen.
        match value {
            BlockInvalidError::BlockSeen { block_hash: _ } => Self::Ignore,
            _ => Self::Reject,
        }
    }
}

impl BlockHandler {
    /// The maximum number of blocks to keep in the seen hashes map.
    ///
    /// Note: this value must be high enough to ensure we prevent replay attacks.
    /// Ie, the entries pruned must be old enough blocks to be considered invalid
    /// if new blocks for that height are received.
    ///
    /// This value is chosen to match the reference node validator's lru cache size.
    /// See: <https://github.com/ethereum-optimism/optimism/blob/836d50be5d5f4ae14ffb2ea6106720a2b080cdae/op-node/p2p/gossip.go#L266>
    pub const SEEN_HASH_CACHE_SIZE: usize = 1_000;

    /// The maximum number of blocks to keep per height.
    /// This value is chosen according to the Base specs:
    /// <https://specs.base.org/protocol/consensus/p2p#block-validation>
    const MAX_BLOCKS_TO_KEEP: usize = 5;

    /// Determines if a block is valid.
    ///
    /// We validate the block according to the rules defined here:
    /// <https://specs.base.org/protocol/consensus/p2p#block-validation>
    ///
    /// The block encoding/compression are assumed to be valid at this point (they are first checked
    /// in the handle).
    pub fn block_valid(
        &mut self,
        envelope: &NetworkPayloadEnvelope,
    ) -> Result<(), BlockInvalidError> {
        // Record total validation attempts
        Metrics::block_validation_total().increment(1);

        // Record block version distribution
        let version = match &envelope.payload {
            BaseExecutionPayload::V1(_) => "v1",
            BaseExecutionPayload::V2(_) => "v2",
            BaseExecutionPayload::V3(_) => "v3",
            BaseExecutionPayload::V4(_) => "v4",
            BaseExecutionPayload::V5(_) => "v5",
        };
        Metrics::block_version(version).increment(1);

        let validation_result =
            base_metrics::time!(Metrics::block_validation_duration_seconds(), {
                self.validate_block_internal(envelope)
            });

        // Record success/failure metrics
        match &validation_result {
            Ok(()) => {
                Metrics::block_validation_success().increment(1);
            }
            Err(err) => {
                let reason = match err {
                    BlockInvalidError::Timestamp { current, received } => {
                        if *received > *current + 5 { "timestamp_future" } else { "timestamp_past" }
                    }
                    BlockInvalidError::BlockHash { .. } => "invalid_hash",
                    BlockInvalidError::Signature => "invalid_signature",
                    BlockInvalidError::Signer { .. } => "invalid_signer",
                    BlockInvalidError::TooManyBlocks { .. } => "too_many_blocks",
                    BlockInvalidError::BlockSeen { .. } => "block_seen",
                    BlockInvalidError::InvalidBlock(_)
                    | BlockInvalidError::BaseFeePerGasOverflow(_) => "invalid_block",
                    BlockInvalidError::InvalidBaseTimeSchedule(_) => "invalid_base_time_schedule",
                    BlockInvalidError::ParentBeaconRoot => "parent_beacon_root",
                    BlockInvalidError::BlobGasUsed => "blob_gas_used",
                    BlockInvalidError::ExcessBlobGas => "excess_blob_gas",
                    BlockInvalidError::WithdrawalsRoot => "withdrawals_root",
                    BlockInvalidError::BlockAccessListHashMismatch { .. } => {
                        "block_access_list_hash"
                    }
                    BlockInvalidError::BlockAccessListPresence { .. } => {
                        "block_access_list_presence"
                    }
                };
                Metrics::block_validation_failed(reason).increment(1);
            }
        }

        validation_result
    }

    /// Internal validation logic extracted for cleaner metrics instrumentation.
    fn validate_block_internal(
        &mut self,
        envelope: &NetworkPayloadEnvelope,
    ) -> Result<(), BlockInvalidError> {
        let current_timestamp =
            SystemTime::now().duration_since(SystemTime::UNIX_EPOCH).unwrap().as_secs();

        // The timestamp is at most 5 seconds in the future.
        let is_future = envelope.payload.timestamp() > current_timestamp + 5;
        // The timestamp is at most 60 seconds in the past.
        let is_past =
            envelope.payload.timestamp() < current_timestamp.saturating_sub(Self::MAX_BLOCK_AGE);

        // CHECK: The timestamp is not too far in the future or past.
        if is_future || is_past {
            return Err(BlockInvalidError::Timestamp {
                current: current_timestamp,
                received: envelope.payload.timestamp(),
            });
        }

        // CHECK: The signature is valid before doing expensive block conversion and hashing work.
        let msg = envelope.payload_hash.signature_message(self.rollup_config.l2_chain_id.id());
        let block_signer = *self.signer_recv.borrow();

        // The block has a valid signature.
        let Ok(msg_signer) = envelope.signature.recover_address_from_prehash(&msg) else {
            return Err(BlockInvalidError::Signature);
        };

        // The block is signed by the expected signer (the unsafe block signer).
        if msg_signer != block_signer {
            return Err(BlockInvalidError::Signer { expected: block_signer, received: msg_signer });
        }

        // CHECK: Ensure the block hash is valid.
        let expected = envelope.payload.block_hash();
        let mut block: Block<BaseTxEnvelope> = envelope.payload.clone().try_into_block()?;
        block.header.parent_beacon_block_root = envelope.parent_beacon_block_root;
        // If isthmus is active, set the requests hash to the empty hash.
        if self.rollup_config.is_isthmus_active(envelope.payload.timestamp()) {
            block.header.requests_hash = Some(EMPTY_REQUESTS_HASH);
        }

        // CHECK: The block access list carried by the payload matches the header commitment.
        Self::validate_block_access_list(&envelope.payload, block.header.block_access_list_hash)?;

        let received = block.header.hash_slow();
        if received != expected {
            return Err(BlockInvalidError::BlockHash { expected, received });
        }

        // CHECK: The payload is valid for the specific version of this block.
        self.validate_version_specific_payload(envelope)?;

        // Reject invalid schedules before gossip acceptance, forwarding, or recording as seen.
        BaseTimeUpdateTx::validate_block_timestamp(
            &self.rollup_config,
            &block.body.transactions,
            block.header.number,
            block.header.timestamp,
        )?;

        if let Some(seen_hashes_at_height) =
            self.seen_hashes.get_mut(&envelope.payload.block_number())
        {
            // CHECK: If the block has already been seen, ignore it.
            if seen_hashes_at_height.contains(&envelope.payload.block_hash()) {
                return Err(BlockInvalidError::BlockSeen {
                    block_hash: envelope.payload.block_hash(),
                });
            }
        }

        let seen_hashes_at_height =
            self.seen_hashes.entry(envelope.payload.block_number()).or_default();
        // Evict the oldest hash rather than rejecting the newest block. The recorded hashes are
        // pre-execution candidates the CL cannot tell apart, so rejecting the newest would let a
        // signer fill the per-height quota with well-formed but EL-invalid blocks and starve the
        // honest block for that height. The per-height bound still holds, and a re-broadcast
        // evicted hash is only re-forwarded once, which the execution layer deduplicates by block
        // hash.
        if seen_hashes_at_height.len() >= Self::MAX_BLOCKS_TO_KEEP {
            seen_hashes_at_height.pop_front();
        }
        seen_hashes_at_height.push_back(envelope.payload.block_hash());

        // Mark the block as seen.
        if self.seen_hashes.len() >= Self::SEEN_HASH_CACHE_SIZE {
            self.seen_hashes.pop_first();
        }

        Ok(())
    }

    /// Validates the block access list a payload carries against a header commitment.
    ///
    /// A V5 payload must carry a list and commit to it, and any other version must not, so a
    /// payload whose list disagrees with `header_commitment` is rejected here.
    ///
    /// The commitment passed by [`Self::block_valid`] is the one the rebuilt header derives from
    /// the carried bytes, so it cannot disagree with them: the guarantee that a peer cannot swap
    /// the list out of a gossiped block comes from the block hash comparison that follows (the
    /// commitment is part of the hashed header). This check is defence in depth for the case where
    /// the commitment comes from elsewhere, such as a header derived from L1 batch data.
    pub fn validate_block_access_list(
        payload: &BaseExecutionPayload,
        header_commitment: Option<B256>,
    ) -> Result<(), BlockInvalidError> {
        match (payload.as_v5(), header_commitment) {
            (Some(payload), Some(header)) => {
                let carried = keccak256(&payload.block_access_list);
                if carried != header {
                    return Err(BlockInvalidError::BlockAccessListHashMismatch { header, carried });
                }
                Ok(())
            }
            (None, None) => Ok(()),
            (payload, header) => Err(BlockInvalidError::BlockAccessListPresence {
                payload_has_list: payload.is_some(),
                header_has_commitment: header.is_some(),
            }),
        }
    }

    /// Validate version specific contents of the payload.
    fn validate_version_specific_payload(
        &self,
        envelope: &NetworkPayloadEnvelope,
    ) -> Result<(), BlockInvalidError> {
        // Validation for v1 payloads are mostly ensured by type-safety, by decoding the
        // payload to the ExecutionPayloadV1 type:
        // 1. The block should not have any withdrawals
        // 2. The block should not have any withdrawals list
        // 3. The block should not have any blob gas used
        // 4. The block should not have any excess blob gas
        // 5. The block should not have any withdrawals root
        // 6. The block should not have any parent beacon block root (validated because ignored by
        //    the decoder, this causes a hash mismatch. See tests)

        // Same as v1, except:
        // 1. The block should have an empty withdrawals list. This is checked during the call to
        //    [`BaseExecutionPayload::try_into_block`].

        // Same as v2, except:
        // 1. The block should have a zero blob gas used
        // 2. The block should have a zero excess blob gas
        // 3. The block should have a non empty parent beacon block root
        fn validate_v3(
            rollup_config: &RollupConfig,
            block: &ExecutionPayloadV3,
            parent_beacon_block_root: Option<B256>,
        ) -> Result<(), BlockInvalidError> {
            // If Jovian is not active, the blob gas used should be zero.
            if !rollup_config.is_jovian_active(block.timestamp()) && block.blob_gas_used != 0 {
                return Err(BlockInvalidError::BlobGasUsed);
            }

            if block.excess_blob_gas != 0 {
                return Err(BlockInvalidError::ExcessBlobGas);
            }

            if parent_beacon_block_root.is_none() {
                return Err(BlockInvalidError::ParentBeaconRoot);
            }

            Ok(())
        }

        // Same as v3, except:
        // 1. The block should have an non-empty withdrawals root (checked by type-safety)
        fn validate_v4(
            rollup_config: &RollupConfig,
            block: &BaseExecutionPayloadV4,
            parent_beacon_block_root: Option<B256>,
        ) -> Result<(), BlockInvalidError> {
            validate_v3(rollup_config, &block.payload_inner, parent_beacon_block_root)
        }

        match &envelope.payload {
            BaseExecutionPayload::V1(_) | BaseExecutionPayload::V2(_) => Ok(()),
            BaseExecutionPayload::V3(payload) => {
                validate_v3(&self.rollup_config, payload, envelope.parent_beacon_block_root)
            }
            BaseExecutionPayload::V4(payload) => {
                validate_v4(&self.rollup_config, payload, envelope.parent_beacon_block_root)
            }
            BaseExecutionPayload::V5(payload) => validate_v4(
                &self.rollup_config,
                &payload.payload_inner,
                envelope.parent_beacon_block_root,
            ),
        }
    }
}

#[cfg(test)]
pub(crate) mod tests {

    use alloy_chains::Chain;
    use alloy_consensus::{Block, EMPTY_OMMER_ROOT_HASH};
    use alloy_eips::{eip2718::Encodable2718, eip4895::Withdrawal};
    use alloy_primitives::{Address, B256, Bytes, Signature};
    use alloy_rlp::BufMut;
    use alloy_rpc_types_engine::{ExecutionPayloadV1, ExecutionPayloadV2, ExecutionPayloadV3};
    use arbitrary::{Arbitrary, Unstructured};
    use base_common_consensus::BaseTxEnvelope;
    use base_common_genesis::RollupConfig;
    use base_common_rpc_types_engine::{
        BaseExecutionPayload, BaseExecutionPayloadV4, BaseExecutionPayloadV5, PayloadHash,
    };
    use libp2p::gossipsub::Message;

    use crate::Handler;
    use super::*;

    fn valid_block() -> Block<BaseTxEnvelope> {
        // Simulate some random data
        let mut data = vec![0; 1024 * 1024];
        let mut rng = rand::rng();
        rand::Rng::fill(&mut rng, &mut data[..]);

        // Create unstructured data with the random bytes
        let u = Unstructured::new(&data);

        // Generate a random instance of MyStruct
        let mut block: Block<BaseTxEnvelope> = Block::arbitrary_take_rest(u).unwrap();

        let transactions: Vec<Bytes> =
            block.body.transactions().map(|tx| tx.encoded_2718().into()).collect();

        let transactions_root =
            alloy_consensus::proofs::ordered_trie_root_with_encoder(&transactions, |item, buf| {
                buf.put_slice(item)
            });

        block.header.transactions_root = transactions_root;

        // We always need to set the base fee per gas to a positive value to ensure the block is
        // valid.
        block.header.base_fee_per_gas =
            Some(block.header.base_fee_per_gas.unwrap_or_default().saturating_add(1));

        let current_timestamp =
            SystemTime::now().duration_since(SystemTime::UNIX_EPOCH).unwrap().as_secs();
        block.header.timestamp = current_timestamp;

        block
    }

    /// Make the block v1 compatible
    fn v1_valid_block() -> Block<BaseTxEnvelope> {
        let mut block = valid_block();
        block.header.withdrawals_root = None;
        block.header.blob_gas_used = None;
        block.header.excess_blob_gas = None;
        block.header.parent_beacon_block_root = None;
        block.header.requests_hash = None;
        block.header.ommers_hash = EMPTY_OMMER_ROOT_HASH;
        block.header.difficulty = Default::default();
        block.header.nonce = Default::default();

        block
    }

    /// Make the block v2 compatible
    pub fn v2_valid_block() -> Block<BaseTxEnvelope> {
        let mut block = v1_valid_block();

        block.body.withdrawals = Some(vec![].into());
        let withdrawals_root = alloy_consensus::proofs::calculate_withdrawals_root(
            &block.body.withdrawals.clone().unwrap_or_default(),
        );

        block.header.withdrawals_root = Some(withdrawals_root);

        block
    }

    /// Make the block v3 compatible
    pub fn v3_valid_block() -> Block<BaseTxEnvelope> {
        let mut block = valid_block();

        block.body.withdrawals = Some(vec![].into());
        let withdrawals_root = alloy_consensus::proofs::calculate_withdrawals_root(
            &block.body.withdrawals.clone().unwrap_or_default(),
        );
        block.header.withdrawals_root = Some(withdrawals_root);

        block.header.blob_gas_used = Some(0);
        block.header.excess_blob_gas = Some(0);
        block.header.parent_beacon_block_root =
            Some(block.header.parent_beacon_block_root.unwrap_or_default());

        block.header.requests_hash = None;
        block.header.ommers_hash = EMPTY_OMMER_ROOT_HASH;
        block.header.difficulty = Default::default();
        block.header.nonce = Default::default();

        block
    }

    /// Make the block v4 compatible
    pub fn v4_valid_block() -> Block<BaseTxEnvelope> {
        v3_valid_block()
    }

    /// The block access list bytes carried by [`v5_valid_block`].
    pub const V5_BLOCK_ACCESS_LIST: &[u8] = &[0xc0, 0xde, 0xba, 0xbe];

    /// Make the block v5 compatible: it commits to [`V5_BLOCK_ACCESS_LIST`] and carries the L2
    /// block number as the slot number.
    pub fn v5_valid_block() -> Block<BaseTxEnvelope> {
        let mut block = v4_valid_block();
        block.header.block_access_list_hash = Some(keccak256(V5_BLOCK_ACCESS_LIST));
        block.header.slot_number = Some(block.header.number);
        block
    }

    /// Builds the V5 payload for a block produced by [`v5_valid_block`].
    pub fn v5_payload(
        block: &Block<BaseTxEnvelope>,
        block_access_list: Bytes,
    ) -> BaseExecutionPayloadV5 {
        let v4 = BaseExecutionPayloadV4::from_v3_with_withdrawals_root(
            ExecutionPayloadV3::from_block_slow(block),
            block.withdrawals_root.unwrap(),
        );
        BaseExecutionPayloadV5::new(v4, block_access_list, block.header.slot_number.unwrap())
    }

    /// Wraps a V5 payload into an unsigned-hash envelope for `block`.
    pub fn v5_envelope(
        block: &Block<BaseTxEnvelope>,
        payload: BaseExecutionPayloadV5,
    ) -> NetworkPayloadEnvelope {
        NetworkPayloadEnvelope {
            payload: BaseExecutionPayload::V5(payload),
            signature: Signature::test_signature(),
            payload_hash: PayloadHash(B256::ZERO),
            parent_beacon_block_root: Some(
                block.header.parent_beacon_block_root.unwrap_or_default(),
            ),
        }
    }

    /// Creates a handler whose unsafe signer is the signer of `envelope`.
    fn handler_for(envelope: &NetworkPayloadEnvelope) -> BlockHandler {
        let msg = envelope.payload_hash.signature_message(8453);
        let signer = envelope.signature.recover_address_from_prehash(&msg).unwrap();
        let (_, unsafe_signer) = tokio::sync::watch::channel(signer);
        BlockHandler::new(
            RollupConfig { l2_chain_id: Chain::base_mainnet(), ..Default::default() },
            unsafe_signer,
        )
    }

    /// A V5 payload whose carried list hashes to the header commitment is accepted.
    #[test]
    fn test_v5_block_with_matching_block_access_list() {
        let block = v5_valid_block();
        let envelope =
            v5_envelope(&block, v5_payload(&block, Bytes::from_static(V5_BLOCK_ACCESS_LIST)));
        let mut handler = handler_for(&envelope);

        assert!(
            handler.block_valid(&envelope).is_ok(),
            "a V5 payload carrying the committed block access list must be accepted"
        );
    }

    /// Swapping the carried list for different bytes while keeping the signed block hash is
    /// rejected at the CL: the reconstructed header commits to the carried list, so the block
    /// hash no longer matches.
    #[test]
    fn test_v5_block_with_tampered_block_access_list_is_rejected() {
        let block = v5_valid_block();
        let envelope = v5_envelope(&block, v5_payload(&block, Bytes::from_static(&[0xde, 0xad])));
        let mut handler = handler_for(&envelope);

        let result = handler.block_valid(&envelope);
        assert!(
            matches!(
                result,
                Err(BlockInvalidError::BlockHash { expected, received })
                    if expected == envelope.payload.block_hash() && received != expected
            ),
            "a tampered block access list must be rejected with a block hash mismatch, got {result:?}"
        );
    }

    /// A carried list that does not hash to the header commitment is rejected with both hashes.
    #[test]
    fn test_block_access_list_hash_mismatch() {
        let block = v5_valid_block();
        let payload = v5_payload(&block, Bytes::from_static(&[0xde, 0xad]));
        let committed = keccak256(V5_BLOCK_ACCESS_LIST);

        let result = BlockHandler::validate_block_access_list(
            &BaseExecutionPayload::V5(payload),
            Some(committed),
        );
        assert!(
            matches!(
                result,
                Err(BlockInvalidError::BlockAccessListHashMismatch { header, carried })
                    if header == committed && carried == keccak256([0xde, 0xad])
            ),
            "carried list must be reported against the header commitment, got {result:?}"
        );
    }

    /// A matching V5 list, and a V4 payload with no commitment, are accepted.
    #[test]
    fn test_block_access_list_accepts_matching_and_pre_amsterdam() {
        let block = v5_valid_block();
        let v5 =
            BaseExecutionPayload::V5(v5_payload(&block, Bytes::from_static(V5_BLOCK_ACCESS_LIST)));
        assert!(
            BlockHandler::validate_block_access_list(&v5, Some(keccak256(V5_BLOCK_ACCESS_LIST)))
                .is_ok(),
            "a V5 payload whose list hashes to the commitment must pass"
        );

        let block = v4_valid_block();
        let v4 = BaseExecutionPayload::V4(BaseExecutionPayloadV4::from_v3_with_withdrawals_root(
            ExecutionPayloadV3::from_block_slow(&block),
            block.withdrawals_root.unwrap(),
        ));
        assert!(
            BlockHandler::validate_block_access_list(&v4, None).is_ok(),
            "a pre-Amsterdam V4 payload without a commitment must be unchanged"
        );
    }

    /// V5 without a header commitment, and V4 with one, are both rejected.
    #[test]
    fn test_block_access_list_presence_mismatch() {
        let block = v5_valid_block();
        let v5 =
            BaseExecutionPayload::V5(v5_payload(&block, Bytes::from_static(V5_BLOCK_ACCESS_LIST)));
        let result = BlockHandler::validate_block_access_list(&v5, None);
        assert!(
            matches!(
                result,
                Err(BlockInvalidError::BlockAccessListPresence {
                    payload_has_list: true,
                    header_has_commitment: false
                })
            ),
            "V5 payload without a header commitment must be rejected, got {result:?}"
        );

        let block = v4_valid_block();
        let v4 = BaseExecutionPayload::V4(BaseExecutionPayloadV4::from_v3_with_withdrawals_root(
            ExecutionPayloadV3::from_block_slow(&block),
            block.withdrawals_root.unwrap(),
        ));
        let result =
            BlockHandler::validate_block_access_list(&v4, Some(keccak256(V5_BLOCK_ACCESS_LIST)));
        assert!(
            matches!(
                result,
                Err(BlockInvalidError::BlockAccessListPresence {
                    payload_has_list: false,
                    header_has_commitment: true
                })
            ),
            "V4 payload with a header commitment must be rejected, got {result:?}"
        );
    }

    /// Generates a random valid block and ensure it is v1 compatible
    #[test]
    fn test_block_valid() {
        let block = v1_valid_block();

        let v1 = ExecutionPayloadV1::from_block_slow(&block);

        let payload = BaseExecutionPayload::V1(v1);
        let envelope = NetworkPayloadEnvelope {
            payload,
            signature: Signature::test_signature(),
            payload_hash: PayloadHash(B256::ZERO),
            parent_beacon_block_root: None,
        };

        let msg = envelope.payload_hash.signature_message(8453);
        let signer = envelope.signature.recover_address_from_prehash(&msg).unwrap();
        let (_, unsafe_signer) = tokio::sync::watch::channel(signer);
        let mut handler = BlockHandler::new(
            RollupConfig { l2_chain_id: Chain::base_mainnet(), ..Default::default() },
            unsafe_signer,
        );

        assert!(handler.block_valid(&envelope).is_ok());
    }

    /// Generates a random block with an invalid timestamp and ensure it is rejected
    #[test]
    fn test_block_invalid_timestamp_early() {
        let mut block = v1_valid_block();

        block.header.timestamp -= 61;

        let v1 = ExecutionPayloadV1::from_block_slow(&block);

        let payload = BaseExecutionPayload::V1(v1);
        let envelope = NetworkPayloadEnvelope {
            payload,
            signature: Signature::test_signature(),
            payload_hash: PayloadHash(B256::ZERO),
            parent_beacon_block_root: None,
        };

        let msg = envelope.payload_hash.signature_message(8453);
        let signer = envelope.signature.recover_address_from_prehash(&msg).unwrap();
        let (_, unsafe_signer) = tokio::sync::watch::channel(signer);
        let mut handler = BlockHandler::new(
            RollupConfig { l2_chain_id: Chain::base_mainnet(), ..Default::default() },
            unsafe_signer,
        );

        assert!(matches!(handler.block_valid(&envelope), Err(BlockInvalidError::Timestamp { .. })));
    }

    /// Generates a random block with an invalid timestamp and ensure it is rejected
    #[test]
    fn test_block_invalid_timestamp_too_far() {
        let mut block = v1_valid_block();

        block.header.timestamp += 60;

        let v1 = ExecutionPayloadV1::from_block_slow(&block);

        let payload = BaseExecutionPayload::V1(v1);
        let envelope = NetworkPayloadEnvelope {
            payload,
            signature: Signature::test_signature(),
            payload_hash: PayloadHash(B256::ZERO),
            parent_beacon_block_root: None,
        };

        let msg = envelope.payload_hash.signature_message(8453);
        let signer = envelope.signature.recover_address_from_prehash(&msg).unwrap();
        let (_, unsafe_signer) = tokio::sync::watch::channel(signer);
        let mut handler = BlockHandler::new(
            RollupConfig { l2_chain_id: Chain::base_mainnet(), ..Default::default() },
            unsafe_signer,
        );

        assert!(matches!(handler.block_valid(&envelope), Err(BlockInvalidError::Timestamp { .. })));
    }

    /// Generates a random block with an invalid hash and ensure it is rejected
    #[test]
    fn test_block_invalid_hash() {
        let block = v1_valid_block();

        let mut v1 = ExecutionPayloadV1::from_block_slow(&block);

        v1.block_hash = B256::ZERO;

        let payload = BaseExecutionPayload::V1(v1);
        let envelope = NetworkPayloadEnvelope {
            payload,
            signature: Signature::test_signature(),
            payload_hash: PayloadHash(B256::ZERO),
            parent_beacon_block_root: None,
        };

        let msg = envelope.payload_hash.signature_message(8453);
        let signer = envelope.signature.recover_address_from_prehash(&msg).unwrap();
        let (_, unsafe_signer) = tokio::sync::watch::channel(signer);
        let mut handler = BlockHandler::new(
            RollupConfig { l2_chain_id: Chain::base_mainnet(), ..Default::default() },
            unsafe_signer,
        );

        assert!(matches!(handler.block_valid(&envelope), Err(BlockInvalidError::BlockHash { .. })));
    }

    #[test]
    fn test_cannot_validate_same_block_twice() {
        let block = v1_valid_block();

        let v1 = ExecutionPayloadV1::from_block_slow(&block);

        let payload = BaseExecutionPayload::V1(v1);
        let envelope = NetworkPayloadEnvelope {
            payload,
            signature: Signature::test_signature(),
            payload_hash: PayloadHash(B256::ZERO),
            parent_beacon_block_root: None,
        };

        let msg = envelope.payload_hash.signature_message(8453);
        let signer = envelope.signature.recover_address_from_prehash(&msg).unwrap();
        let (_, unsafe_signer) = tokio::sync::watch::channel(signer);
        let mut handler = BlockHandler::new(
            RollupConfig { l2_chain_id: Chain::base_mainnet(), ..Default::default() },
            unsafe_signer,
        );

        assert!(handler.block_valid(&envelope).is_ok());
        assert!(matches!(handler.block_valid(&envelope), Err(BlockInvalidError::BlockSeen { .. })));
    }

    /// A height accepts more distinct blocks than [`BlockHandler::MAX_BLOCKS_TO_KEEP`]: the
    /// per-height cache evicts its oldest hash instead of rejecting the newest block.
    #[test]
    fn test_per_height_cache_evicts_the_oldest_block() {
        let first_block = v1_valid_block();

        let initial_height = first_block.header.number;

        let v1 = ExecutionPayloadV1::from_block_slow(&first_block);

        let payload = BaseExecutionPayload::V1(v1);
        let envelope = NetworkPayloadEnvelope {
            payload,
            signature: Signature::test_signature(),
            payload_hash: PayloadHash(B256::ZERO),
            parent_beacon_block_root: None,
        };

        let msg = envelope.payload_hash.signature_message(8453);
        let signer = envelope.signature.recover_address_from_prehash(&msg).unwrap();
        let (_, unsafe_signer) = tokio::sync::watch::channel(signer);
        let mut handler = BlockHandler::new(
            RollupConfig { l2_chain_id: Chain::base_mainnet(), ..Default::default() },
            unsafe_signer,
        );

        assert!(handler.block_valid(&envelope).is_ok());

        // Fill the per-height cache with `MAX_BLOCKS_TO_KEEP` distinct hashes.
        let next_payloads = (0..BlockHandler::MAX_BLOCKS_TO_KEEP - 1)
            .map(|_| {
                let mut block = v1_valid_block();
                // The blocks have the same height
                block.header.number = initial_height;

                let v1 = ExecutionPayloadV1::from_block_slow(&block);

                let payload = BaseExecutionPayload::V1(v1);
                NetworkPayloadEnvelope {
                    payload,
                    signature: Signature::test_signature(),
                    payload_hash: PayloadHash(B256::ZERO),
                    parent_beacon_block_root: None,
                }
            })
            .collect::<Vec<_>>();

        for candidate in &next_payloads {
            assert!(
                handler.block_valid(candidate).is_ok(),
                "every distinct block below the per-height bound must be accepted"
            );
        }

        // A further distinct block is admitted: the cache evicts its oldest hash instead of
        // rejecting the newest block.
        let mut newest_block = v1_valid_block();
        newest_block.header.number = initial_height;
        let newest_envelope = NetworkPayloadEnvelope {
            payload: BaseExecutionPayload::V1(ExecutionPayloadV1::from_block_slow(&newest_block)),
            signature: Signature::test_signature(),
            payload_hash: PayloadHash(B256::ZERO),
            parent_beacon_block_root: None,
        };

        assert!(
            handler.block_valid(&newest_envelope).is_ok(),
            "the newest block must be admitted once the per-height cache is full"
        );

        let seen = handler
            .seen_hashes
            .get(&initial_height)
            .expect("the height must still be recorded in the seen-hash cache");
        assert_eq!(
            seen.len(),
            BlockHandler::MAX_BLOCKS_TO_KEEP,
            "the per-height cache must stay bounded by MAX_BLOCKS_TO_KEEP"
        );
        assert!(
            !seen.contains(&envelope.payload.block_hash()),
            "the oldest hash must be evicted"
        );
        assert_eq!(
            seen.front(),
            Some(&next_payloads[0].payload.block_hash()),
            "the hash recorded second must now be the oldest"
        );
        assert_eq!(
            seen.back(),
            Some(&newest_envelope.payload.block_hash()),
            "the newest hash must be recorded"
        );
    }

    /// A signer holding the sequencer key cannot starve the honest block at a height by
    /// publishing `MAX_BLOCKS_TO_KEEP` well-formed, correctly signed V5 envelopes whose block
    /// access lists are malformed. The CL cannot execute them, so it accepts all of them; the
    /// honest block must still be admitted and forwarded rather than rejected with
    /// `TooManyBlocks`.
    #[test]
    fn test_malformed_block_access_lists_cannot_starve_the_honest_block() {
        let height = v5_valid_block().header.number;

        // Each envelope commits to and carries a different malformed block access list, so each
        // has its own valid block hash and passes the CL checks. Only the execution layer can
        // reject the malformed lists.
        let attacker_envelopes = (0..BlockHandler::MAX_BLOCKS_TO_KEEP)
            .map(|i| {
                let mut block = v5_valid_block();
                block.header.number = height;
                block.header.slot_number = Some(height);
                let malformed = Bytes::from(vec![0xde, 0xad, i as u8]);
                block.header.block_access_list_hash = Some(keccak256(&malformed));
                v5_envelope(&block, v5_payload(&block, malformed))
            })
            .collect::<Vec<_>>();

        let mut handler = handler_for(&attacker_envelopes[0]);

        for (i, envelope) in attacker_envelopes.iter().enumerate() {
            assert!(
                handler.block_valid(envelope).is_ok(),
                "attacker envelope {i} must be accepted by the CL, which cannot execute it"
            );
        }
        assert_eq!(
            handler.seen_hashes.get(&height).expect("the height must be recorded").len(),
            BlockHandler::MAX_BLOCKS_TO_KEEP,
            "the attacker must have filled the per-height cache"
        );

        // The honest block for the same height must be admitted.
        let mut honest_block = v5_valid_block();
        honest_block.header.number = height;
        honest_block.header.slot_number = Some(height);
        let honest = v5_envelope(
            &honest_block,
            v5_payload(&honest_block, Bytes::from_static(V5_BLOCK_ACCESS_LIST)),
        );
        assert!(
            handler.block_valid(&honest).is_ok(),
            "the honest block must be admitted once the per-height cache is full"
        );

        let seen = handler.seen_hashes.get(&height).expect("the height must be recorded");
        assert!(
            seen.contains(&honest.payload.block_hash()),
            "the honest block must be recorded as seen"
        );
        assert!(
            !seen.contains(&attacker_envelopes[0].payload.block_hash()),
            "the oldest attacker hash must be evicted"
        );

        // ...and it must reach the execution layer.
        let probe =
            BlockHandler::new(RollupConfig::default(), tokio::sync::watch::channel(Address::ZERO).1);
        let encoded = probe.encode(probe.blocks_v5_topic.clone(), honest).unwrap();
        let decoded = NetworkPayloadEnvelope::decode_v5(&encoded).unwrap();
        let msg = decoded.payload_hash.signature_message(8453);
        let signer = decoded.signature.recover_address_from_prehash(&msg).unwrap();
        let (_, unsafe_signer) = tokio::sync::watch::channel(signer);
        let mut forwarding_handler = BlockHandler::new(
            RollupConfig { l2_chain_id: Chain::base_mainnet(), ..Default::default() },
            unsafe_signer,
        );
        let message = Message {
            source: None,
            sequence_number: None,
            topic: forwarding_handler.blocks_v5_topic.clone().into(),
            data: encoded,
        };

        let (acceptance, forwarded) = forwarding_handler.handle(message);
        assert!(
            matches!(acceptance, MessageAcceptance::Accept),
            "the honest block must be accepted, got {acceptance:?}"
        );
        assert!(forwarded.is_some(), "the honest block must be forwarded to the execution layer");
    }

    /// Blocks with invalid signatures should be rejected.
    #[test]
    fn test_invalid_signature() {
        let block = v1_valid_block();

        let v1 = ExecutionPayloadV1::from_block_slow(&block);

        let payload = BaseExecutionPayload::V1(v1);
        let mut envelope = NetworkPayloadEnvelope {
            payload,
            signature: Signature::test_signature(),
            payload_hash: PayloadHash(B256::ZERO),
            parent_beacon_block_root: None,
        };

        let msg = envelope.payload_hash.signature_message(8453);
        let signer = envelope.signature.recover_address_from_prehash(&msg).unwrap();
        let (_, unsafe_signer) = tokio::sync::watch::channel(signer);
        let mut handler = BlockHandler::new(
            RollupConfig { l2_chain_id: Chain::base_mainnet(), ..Default::default() },
            unsafe_signer,
        );

        let mut signature_bytes = envelope.signature.as_bytes();
        signature_bytes[0] = !signature_bytes[0];
        envelope.signature = Signature::from_raw_array(&signature_bytes).unwrap();

        assert!(handler.seen_hashes.is_empty());
        assert!(matches!(handler.block_valid(&envelope), Err(BlockInvalidError::Signature)));
    }

    /// Invalid signatures should be rejected before block hash validation.
    #[test]
    fn test_invalid_signature_precedes_block_hash_validation() {
        let block = v1_valid_block();

        let mut v1 = ExecutionPayloadV1::from_block_slow(&block);
        v1.block_hash = B256::ZERO;

        let payload = BaseExecutionPayload::V1(v1);
        let mut envelope = NetworkPayloadEnvelope {
            payload,
            signature: Signature::test_signature(),
            payload_hash: PayloadHash(B256::ZERO),
            parent_beacon_block_root: None,
        };

        let msg = envelope.payload_hash.signature_message(8453);
        let signer = envelope.signature.recover_address_from_prehash(&msg).unwrap();
        let (_, unsafe_signer) = tokio::sync::watch::channel(signer);
        let mut handler = BlockHandler::new(
            RollupConfig { l2_chain_id: Chain::base_mainnet(), ..Default::default() },
            unsafe_signer,
        );

        let mut signature_bytes = envelope.signature.as_bytes();
        signature_bytes[0] = !signature_bytes[0];
        envelope.signature = Signature::from_raw_array(&signature_bytes).unwrap();

        assert!(handler.seen_hashes.is_empty());
        assert!(matches!(handler.block_valid(&envelope), Err(BlockInvalidError::Signature)));
    }

    /// Blocks with invalid signers should be rejected.
    #[test]
    fn test_invalid_signer() {
        let block = v1_valid_block();

        let v1 = ExecutionPayloadV1::from_block_slow(&block);

        let payload = BaseExecutionPayload::V1(v1);
        let envelope = NetworkPayloadEnvelope {
            payload,
            signature: Signature::test_signature(),
            payload_hash: PayloadHash(B256::ZERO),
            parent_beacon_block_root: None,
        };

        let (_, unsafe_signer) = tokio::sync::watch::channel(Address::default());
        let mut handler = BlockHandler::new(
            RollupConfig { l2_chain_id: Chain::base_mainnet(), ..Default::default() },
            unsafe_signer,
        );

        assert!(matches!(handler.block_valid(&envelope), Err(BlockInvalidError::Signer { .. })));
    }

    /// If we specify a non empty parent beacon block root for blocks with v1/v2 payloads we
    /// get a hash mismatch error because the decoder enforces that these versions of the execution
    /// payload don't contain the parent beacon block root.
    #[test]
    fn test_v1_v2_block_invalid_parent_beacon_block_root() {
        let block = v1_valid_block();

        let v1 = ExecutionPayloadV1::from_block_slow(&block);

        let payload = BaseExecutionPayload::V1(v1);
        let envelope = NetworkPayloadEnvelope {
            payload,
            signature: Signature::test_signature(),
            payload_hash: PayloadHash(B256::ZERO),
            parent_beacon_block_root: Some(B256::ZERO),
        };

        let msg = envelope.payload_hash.signature_message(8453);
        let signer = envelope.signature.recover_address_from_prehash(&msg).unwrap();
        let (_, unsafe_signer) = tokio::sync::watch::channel(signer);
        let mut handler = BlockHandler::new(
            RollupConfig { l2_chain_id: Chain::base_mainnet(), ..Default::default() },
            unsafe_signer,
        );

        assert!(matches!(handler.block_valid(&envelope), Err(BlockInvalidError::BlockHash { .. })));

        let block = v2_valid_block();

        let v2 = ExecutionPayloadV2::from_block_slow(&block);

        let payload = BaseExecutionPayload::V2(v2);
        let envelope = NetworkPayloadEnvelope {
            payload,
            signature: Signature::test_signature(),
            payload_hash: PayloadHash(B256::ZERO),
            parent_beacon_block_root: Some(B256::ZERO),
        };

        let msg = envelope.payload_hash.signature_message(8453);
        let signer = envelope.signature.recover_address_from_prehash(&msg).unwrap();
        let (_, unsafe_signer) = tokio::sync::watch::channel(signer);
        let mut handler = BlockHandler::new(
            RollupConfig { l2_chain_id: Chain::base_mainnet(), ..Default::default() },
            unsafe_signer,
        );

        assert!(matches!(handler.block_valid(&envelope), Err(BlockInvalidError::BlockHash { .. })));
    }

    #[test]
    fn test_v2_block() {
        let block = v2_valid_block();

        let v2 = ExecutionPayloadV2::from_block_slow(&block);

        let payload = BaseExecutionPayload::V2(v2);
        let envelope = NetworkPayloadEnvelope {
            payload,
            signature: Signature::test_signature(),
            payload_hash: PayloadHash(B256::ZERO),
            parent_beacon_block_root: None,
        };

        let msg = envelope.payload_hash.signature_message(8453);
        let signer = envelope.signature.recover_address_from_prehash(&msg).unwrap();
        let (_, unsafe_signer) = tokio::sync::watch::channel(signer);
        let mut handler = BlockHandler::new(
            RollupConfig { l2_chain_id: Chain::base_mainnet(), ..Default::default() },
            unsafe_signer,
        );

        assert!(handler.block_valid(&envelope).is_ok());
    }

    #[test]
    fn test_v2_non_empty_withdrawals() {
        let mut block = v2_valid_block();
        block.body.withdrawals = Some(vec![Withdrawal::default()].into());
        let withdrawals_root = alloy_consensus::proofs::calculate_withdrawals_root(
            &block.body.withdrawals.clone().unwrap_or_default(),
        );
        block.header.withdrawals_root = Some(withdrawals_root);

        let v2 = ExecutionPayloadV2::from_block_slow(&block);

        let payload = BaseExecutionPayload::V2(v2);
        let envelope = NetworkPayloadEnvelope {
            payload,
            signature: Signature::test_signature(),
            payload_hash: PayloadHash(B256::ZERO),
            parent_beacon_block_root: None,
        };

        let msg = envelope.payload_hash.signature_message(8453);
        let signer = envelope.signature.recover_address_from_prehash(&msg).unwrap();
        let (_, unsafe_signer) = tokio::sync::watch::channel(signer);
        let mut handler = BlockHandler::new(
            RollupConfig { l2_chain_id: Chain::base_mainnet(), ..Default::default() },
            unsafe_signer,
        );

        assert!(matches!(
            handler.block_valid(&envelope),
            Err(BlockInvalidError::InvalidBlock(BasePayloadError::NonEmptyL1Withdrawals))
        ));
    }

    #[test]
    fn test_v3_block() {
        let block = v3_valid_block();

        let v3 = ExecutionPayloadV3::from_block_slow(&block);

        let payload = BaseExecutionPayload::V3(v3);
        let envelope = NetworkPayloadEnvelope {
            payload,
            signature: Signature::test_signature(),
            payload_hash: PayloadHash(B256::ZERO),
            parent_beacon_block_root: Some(
                block.header.parent_beacon_block_root.unwrap_or_default(),
            ),
        };

        let msg = envelope.payload_hash.signature_message(8453);
        let signer = envelope.signature.recover_address_from_prehash(&msg).unwrap();
        let (_, unsafe_signer) = tokio::sync::watch::channel(signer);
        let mut handler = BlockHandler::new(
            RollupConfig { l2_chain_id: Chain::base_mainnet(), ..Default::default() },
            unsafe_signer,
        );

        assert!(handler.block_valid(&envelope).is_ok());
    }

    #[test]
    fn test_v3_non_empty_withdrawals() {
        let mut block = v3_valid_block();
        block.body.withdrawals = Some(vec![Withdrawal::default()].into());
        let withdrawals_root = alloy_consensus::proofs::calculate_withdrawals_root(
            &block.body.withdrawals.clone().unwrap_or_default(),
        );
        block.header.withdrawals_root = Some(withdrawals_root);

        let v3 = ExecutionPayloadV3::from_block_slow(&block);

        let payload = BaseExecutionPayload::V3(v3);
        let envelope = NetworkPayloadEnvelope {
            payload,
            signature: Signature::test_signature(),
            payload_hash: PayloadHash(B256::ZERO),
            parent_beacon_block_root: Some(
                block.header.parent_beacon_block_root.unwrap_or_default(),
            ),
        };

        let msg = envelope.payload_hash.signature_message(8453);
        let signer = envelope.signature.recover_address_from_prehash(&msg).unwrap();
        let (_, unsafe_signer) = tokio::sync::watch::channel(signer);
        let mut handler = BlockHandler::new(
            RollupConfig { l2_chain_id: Chain::base_mainnet(), ..Default::default() },
            unsafe_signer,
        );

        assert!(matches!(
            handler.block_valid(&envelope),
            Err(BlockInvalidError::InvalidBlock(BasePayloadError::NonEmptyL1Withdrawals))
        ));
    }

    #[test]
    fn test_v3_gas_params() {
        let mut block = v3_valid_block();
        block.header.blob_gas_used = Some(1);

        let v3 = ExecutionPayloadV3::from_block_slow(&block);

        let payload = BaseExecutionPayload::V3(v3);
        let envelope = NetworkPayloadEnvelope {
            payload,
            signature: Signature::test_signature(),
            payload_hash: PayloadHash(B256::ZERO),
            parent_beacon_block_root: Some(
                block.header.parent_beacon_block_root.unwrap_or_default(),
            ),
        };

        let msg = envelope.payload_hash.signature_message(8453);
        let signer = envelope.signature.recover_address_from_prehash(&msg).unwrap();
        let (_, unsafe_signer) = tokio::sync::watch::channel(signer);
        let mut handler = BlockHandler::new(
            RollupConfig { l2_chain_id: Chain::base_mainnet(), ..Default::default() },
            unsafe_signer,
        );

        assert!(matches!(handler.block_valid(&envelope), Err(BlockInvalidError::BlobGasUsed)));

        block.header.blob_gas_used = Some(0);
        block.header.excess_blob_gas = Some(1);

        let v3 = ExecutionPayloadV3::from_block_slow(&block);

        let payload = BaseExecutionPayload::V3(v3);
        let envelope = NetworkPayloadEnvelope {
            payload,
            signature: Signature::test_signature(),
            payload_hash: PayloadHash(B256::ZERO),
            parent_beacon_block_root: Some(
                block.header.parent_beacon_block_root.unwrap_or_default(),
            ),
        };

        assert!(matches!(handler.block_valid(&envelope), Err(BlockInvalidError::ExcessBlobGas)));
    }

    #[test]
    fn test_v4_block() {
        let block = v4_valid_block();

        let v3 = ExecutionPayloadV3::from_block_slow(&block);
        let v4 = BaseExecutionPayloadV4::from_v3_with_withdrawals_root(
            v3,
            block.withdrawals_root.unwrap(),
        );

        let payload = BaseExecutionPayload::V4(v4);
        let envelope = NetworkPayloadEnvelope {
            payload,
            signature: Signature::test_signature(),
            payload_hash: PayloadHash(B256::ZERO),
            parent_beacon_block_root: Some(
                block.header.parent_beacon_block_root.unwrap_or_default(),
            ),
        };

        let msg = envelope.payload_hash.signature_message(8453);
        let signer = envelope.signature.recover_address_from_prehash(&msg).unwrap();
        let (_, unsafe_signer) = tokio::sync::watch::channel(signer);
        let mut handler = BlockHandler::new(
            RollupConfig { l2_chain_id: Chain::base_mainnet(), ..Default::default() },
            unsafe_signer,
        );

        assert!(handler.block_valid(&envelope).is_ok());
    }

    #[test]
    #[cfg(feature = "metrics")]
    fn test_metrics_instrumentation() {
        // This test verifies that metrics code compiles and doesn't panic
        // The actual metric values would require a metrics registry setup in a real test
        // environment

        let block = v1_valid_block();
        let v1 = ExecutionPayloadV1::from_block_slow(&block);
        let payload = BaseExecutionPayload::V1(v1);
        let envelope = NetworkPayloadEnvelope {
            payload,
            signature: Signature::test_signature(),
            payload_hash: PayloadHash(B256::ZERO),
            parent_beacon_block_root: None,
        };

        let msg = envelope.payload_hash.signature_message(8453);
        let signer = envelope.signature.recover_address_from_prehash(&msg).unwrap();
        let (_, unsafe_signer) = tokio::sync::watch::channel(signer);
        let mut handler = BlockHandler::new(
            RollupConfig { l2_chain_id: Chain::base_mainnet(), ..Default::default() },
            unsafe_signer,
        );

        // Test successful validation metrics
        assert!(handler.block_valid(&envelope).is_ok());

        // Test failed validation metrics
        let mut invalid_block = v1_valid_block();
        invalid_block.header.timestamp = 0; // Invalid timestamp

        let v1_invalid = ExecutionPayloadV1::from_block_slow(&invalid_block);
        let payload_invalid = BaseExecutionPayload::V1(v1_invalid);
        let envelope_invalid = NetworkPayloadEnvelope {
            payload: payload_invalid,
            signature: Signature::test_signature(),
            payload_hash: PayloadHash(B256::ZERO),
            parent_beacon_block_root: None,
        };

        // This should increment failure metrics
        assert!(handler.block_valid(&envelope_invalid).is_err());
    }

    #[test]
    fn test_metrics_feature_gating() {
        // Verify the code compiles and runs without panics even when metrics feature is disabled
        let block = v1_valid_block();
        let v1 = ExecutionPayloadV1::from_block_slow(&block);
        let payload = BaseExecutionPayload::V1(v1);
        let envelope = NetworkPayloadEnvelope {
            payload,
            signature: Signature::test_signature(),
            payload_hash: PayloadHash(B256::ZERO),
            parent_beacon_block_root: None,
        };

        let msg = envelope.payload_hash.signature_message(8453);
        let signer = envelope.signature.recover_address_from_prehash(&msg).unwrap();
        let (_, unsafe_signer) = tokio::sync::watch::channel(signer);
        let mut handler = BlockHandler::new(
            RollupConfig { l2_chain_id: Chain::base_mainnet(), ..Default::default() },
            unsafe_signer,
        );

        // Should work regardless of metrics feature
        assert!(handler.block_valid(&envelope).is_ok());
    }
}
