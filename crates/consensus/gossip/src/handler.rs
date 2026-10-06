//! Block Handler

use std::collections::{BTreeMap, HashSet};

use alloy_primitives::{Address, B256};
use base_common_genesis::RollupConfig;
use base_common_rpc_types_engine::NetworkPayloadEnvelope;
use libp2p::gossipsub::{IdentTopic, Message, MessageAcceptance, TopicHash};
use tokio::sync::watch::Receiver;
use tracing::instrument;

use crate::HandlerEncodeError;

/// This trait defines the functionality required to process incoming messages
/// and determine their acceptance within the network.
///
/// Implementors of this trait can specify how messages are handled and which
/// topics they are interested in.
pub trait Handler: Send {
    /// Manages validation and further processing of messages
    /// This is a stateful method, because the handler needs to keep track of seen hashes.
    fn handle(&mut self, msg: Message) -> (MessageAcceptance, Option<NetworkPayloadEnvelope>);

    /// Specifies which topics the handler is interested in
    fn topics(&self) -> Vec<TopicHash>;
}

/// Responsible for managing blocks received via p2p gossip
#[derive(Debug, Clone)]
pub struct BlockHandler {
    /// The rollup config used to validate the block.
    pub rollup_config: RollupConfig,
    /// A [`Receiver`] to monitor changes to the unsafe block signer.
    pub signer_recv: Receiver<Address>,
    /// The libp2p topic for pre Canyon/Shangai blocks.
    pub blocks_v1_topic: IdentTopic,
    /// The libp2p topic for Canyon/Delta blocks.
    pub blocks_v2_topic: IdentTopic,
    /// The libp2p topic for Ecotone V3 blocks.
    pub blocks_v3_topic: IdentTopic,
    /// The libp2p topic for V4 blocks.
    pub blocks_v4_topic: IdentTopic,
    /// A map of seen block height to block hash set.
    /// This map is pruned when it contains more than [`Self::SEEN_HASH_CACHE_SIZE`] entries.
    pub seen_hashes: BTreeMap<u64, HashSet<B256>>,
}

impl Handler for BlockHandler {
    /// Checks validity of a [`NetworkPayloadEnvelope`] received over P2P gossip.
    /// If valid, sends the [`NetworkPayloadEnvelope`] to the block update channel.
    #[instrument(
        skip_all,
        fields(
            topic = %msg.topic,
            block_hash = tracing::field::Empty,
            block_number = tracing::field::Empty
        )
    )]
    fn handle(&mut self, msg: Message) -> (MessageAcceptance, Option<NetworkPayloadEnvelope>) {
        let decoded = if msg.topic == self.blocks_v1_topic.hash() {
            NetworkPayloadEnvelope::decode_v1(&msg.data)
        } else if msg.topic == self.blocks_v2_topic.hash() {
            NetworkPayloadEnvelope::decode_v2(&msg.data)
        } else if msg.topic == self.blocks_v3_topic.hash() {
            NetworkPayloadEnvelope::decode_v3(&msg.data)
        } else if msg.topic == self.blocks_v4_topic.hash() {
            NetworkPayloadEnvelope::decode_v4(&msg.data)
        } else {
            warn!(target: "gossip", topic = ?msg.topic, "Received block with unknown topic");
            return (MessageAcceptance::Reject, None);
        };

        match decoded {
            Ok(envelope) => {
                tracing::Span::current()
                    .record("block_hash", tracing::field::display(envelope.payload.block_hash()));
                tracing::Span::current().record("block_number", envelope.payload.block_number());
                match self.block_valid(&envelope) {
                    Ok(()) => (MessageAcceptance::Accept, Some(envelope)),
                    Err(err) => {
                        warn!(target: "gossip", ?err, hash = ?envelope.payload_hash, "Received invalid block");
                        (err.into(), None)
                    }
                }
            }
            Err(err) => {
                warn!(target: "gossip", ?err, "Failed to decode block");
                (MessageAcceptance::Reject, None)
            }
        }
    }

    /// The gossip topics accepted for new blocks
    fn topics(&self) -> Vec<TopicHash> {
        vec![
            self.blocks_v1_topic.hash(),
            self.blocks_v2_topic.hash(),
            self.blocks_v3_topic.hash(),
            self.blocks_v4_topic.hash(),
        ]
    }
}

impl BlockHandler {
    /// Creates a new [`BlockHandler`].
    ///
    /// Requires the chain ID and a receiver channel for the unsafe block signer.
    pub fn new(rollup_config: RollupConfig, signer_recv: Receiver<Address>) -> Self {
        let chain_id = rollup_config.l2_chain_id.id();
        Self {
            rollup_config,
            signer_recv,
            blocks_v1_topic: IdentTopic::new(format!("/optimism/{chain_id}/0/blocks")),
            blocks_v2_topic: IdentTopic::new(format!("/optimism/{chain_id}/1/blocks")),
            blocks_v3_topic: IdentTopic::new(format!("/optimism/{chain_id}/2/blocks")),
            blocks_v4_topic: IdentTopic::new(format!("/optimism/{chain_id}/3/blocks")),
            seen_hashes: BTreeMap::new(),
        }
    }

    /// Returns the topic using the specified timestamp and optional [`RollupConfig`].
    ///
    /// Reference: <https://github.com/ethereum-optimism/optimism/blob/0bc5fe8d16155dc68bcdf1fa5733abc58689a618/op-node/p2p/gossip.go#L604C1-L612C3>
    pub fn topic(&self, timestamp: u64) -> IdentTopic {
        if self.rollup_config.is_isthmus_active(timestamp) {
            self.blocks_v4_topic.clone()
        } else if self.rollup_config.is_ecotone_active(timestamp) {
            self.blocks_v3_topic.clone()
        } else if self.rollup_config.is_canyon_active(timestamp) {
            self.blocks_v2_topic.clone()
        } else {
            self.blocks_v1_topic.clone()
        }
    }

    /// Encodes a [`NetworkPayloadEnvelope`] into a byte array
    /// based on the specified topic.
    pub fn encode(
        &self,
        topic: IdentTopic,
        envelope: NetworkPayloadEnvelope,
    ) -> Result<Vec<u8>, HandlerEncodeError> {
        let encoded = match topic.hash() {
            hash if hash == self.blocks_v1_topic.hash() => envelope.encode_v1()?,
            hash if hash == self.blocks_v2_topic.hash() => envelope.encode_v2()?,
            hash if hash == self.blocks_v3_topic.hash() => envelope.encode_v3()?,
            hash if hash == self.blocks_v4_topic.hash() => envelope.encode_v4()?,
            hash => return Err(HandlerEncodeError::UnknownTopic(hash)),
        };
        Ok(encoded)
    }
}

#[cfg(test)]
mod tests {
    use rstest::rstest;

    use super::*;
    use crate::{block_handler, envelope};

    #[rstest]
    #[case::valid_v2(2, 2, true, MessageAcceptance::Accept)]
    #[case::valid_v3(3, 3, true, MessageAcceptance::Accept)]
    #[case::valid_v4(4, 4, true, MessageAcceptance::Accept)]
    #[case::v2_on_v1_topic(2, 1, true, MessageAcceptance::Reject)]
    #[case::v3_on_v2_topic(3, 2, true, MessageAcceptance::Reject)]
    #[case::v2_on_v3_topic(2, 3, true, MessageAcceptance::Reject)]
    #[case::v4_on_v3_topic(4, 3, true, MessageAcceptance::Reject)]
    #[case::invalid_payload_hash(2, 2, false, MessageAcceptance::Reject)]
    fn test_decode(
        #[case] version: u8,
        #[case] topic_version: usize,
        #[case] use_decoded_signer: bool,
        #[case] expected: MessageAcceptance,
        #[with(version)] envelope: NetworkPayloadEnvelope,
        #[from(block_handler)] mut handler: BlockHandler,
    ) {
        let topics = handler.topics();
        let encoded = handler
            .encode(IdentTopic::new(topics[usize::from(version) - 1].as_str()), envelope)
            .unwrap();
        if use_decoded_signer {
            let decoded = match version {
                2 => NetworkPayloadEnvelope::decode_v2(&encoded),
                3 => NetworkPayloadEnvelope::decode_v3(&encoded),
                4 => NetworkPayloadEnvelope::decode_v4(&encoded),
                _ => unreachable!(),
            }
            .unwrap();
            let msg = decoded.payload_hash.signature_message(8453);
            let signer = decoded.signature.recover_address_from_prehash(&msg).unwrap();
            let (_, unsafe_signer) = tokio::sync::watch::channel(signer);
            handler.signer_recv = unsafe_signer;
        }
        let message = Message {
            source: None,
            sequence_number: None,
            topic: topics[topic_version - 1].clone(),
            data: encoded,
        };
        let (acceptance, envelope) = handler.handle(message);
        assert_eq!(std::mem::discriminant(&acceptance), std::mem::discriminant(&expected));
        assert_eq!(envelope.is_some(), matches!(expected, MessageAcceptance::Accept));
    }
}
