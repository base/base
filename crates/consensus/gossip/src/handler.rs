//! Block Handler

use std::{
    collections::{BTreeMap, HashSet},
    time::SystemTime,
};

use alloy_primitives::{Address, B256};
use base_common_genesis::RollupConfig;
use base_common_rpc_types_engine::{NetworkPayloadEnvelope, PayloadEnvelopeError};
use libp2p::gossipsub::{IdentTopic, Message, MessageAcceptance, TopicHash};
use tokio::sync::watch::Receiver;
use tracing::instrument;

use crate::{HandlerEncodeError, Metrics};

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
        let decode: fn(&[u8]) -> Result<NetworkPayloadEnvelope, PayloadEnvelopeError> =
            if msg.topic == self.blocks_v1_topic.hash() {
                NetworkPayloadEnvelope::decode_v1
            } else if msg.topic == self.blocks_v2_topic.hash() {
                NetworkPayloadEnvelope::decode_v2
            } else if msg.topic == self.blocks_v3_topic.hash() {
                NetworkPayloadEnvelope::decode_v3
            } else if msg.topic == self.blocks_v4_topic.hash() {
                NetworkPayloadEnvelope::decode_v4
            } else {
                warn!(target: "gossip", topic = ?msg.topic, "Received block with unknown topic");
                return (MessageAcceptance::Reject, None);
            };

        // Do not decompress or SSZ-decode queued messages from a retired topic.
        if !self.topics().contains(&msg.topic) {
            if let Some(version) = self.topic_version(&msg.topic) {
                Metrics::block_topic_blocked_total(version, "inbound").increment(1);
            }
            trace!(target: "gossip", topic = %msg.topic, "Ignoring retired block topic");
            return (MessageAcceptance::Ignore, None);
        }

        match decode(&msg.data) {
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

    /// Topics not yet retired according to the local clock and configured forks.
    fn topics(&self) -> Vec<TopicHash> {
        let timestamp =
            SystemTime::now().duration_since(SystemTime::UNIX_EPOCH).unwrap_or_default().as_secs();
        self.topics_at(timestamp)
    }
}

impl BlockHandler {
    /// Maximum age of a gossip block, in seconds.
    ///
    /// Also defines the topic retirement grace period: after a replacement fork
    /// has been active this long, every block from the old version is too old.
    pub const MAX_BLOCK_AGE: u64 = 60;

    /// Returns a bounded version label for a known block topic.
    /// Unknown peer-supplied topics must not become metric labels.
    pub fn topic_version(&self, topic: &TopicHash) -> Option<&'static str> {
        [
            ("v1", &self.blocks_v1_topic),
            ("v2", &self.blocks_v2_topic),
            ("v3", &self.blocks_v3_topic),
            ("v4", &self.blocks_v4_topic),
        ]
        .into_iter()
        .find_map(|(version, known)| (known.hash() == *topic).then_some(version))
    }

    /// Returns the non-retired gossip topics at a local Unix timestamp.
    ///
    /// Future topics remain subscribed ahead of activation so the mesh is ready
    /// for a fork. An old topic retires once it can no longer carry a block in
    /// the accepted timestamp window. This uses the local clock, never a peer's
    /// claimed block timestamp, and respects custom fork configurations.
    pub fn topics_at(&self, timestamp: u64) -> Vec<TopicHash> {
        let oldest_topic = self.topic(timestamp.saturating_sub(Self::MAX_BLOCK_AGE)).hash();
        [
            self.blocks_v1_topic.hash(),
            self.blocks_v2_topic.hash(),
            self.blocks_v3_topic.hash(),
            self.blocks_v4_topic.hash(),
        ]
        .into_iter()
        .skip_while(|topic| *topic != oldest_topic)
        .collect()
    }

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
    ///
    /// Retired and unknown topics are rejected. Historical payloads can still
    /// be encoded directly through the [`NetworkPayloadEnvelope`] codecs.
    /// The local-clock cutoff applies even before the driver's next subscription
    /// sweep. The driver separately checks subscription state so a clock rollback
    /// cannot re-enable publishing on an already-unsubscribed topic.
    pub fn encode(
        &self,
        topic: IdentTopic,
        envelope: NetworkPayloadEnvelope,
    ) -> Result<Vec<u8>, HandlerEncodeError> {
        if !self.topics().contains(&topic.hash()) {
            if let Some(version) = self.topic_version(&topic.hash()) {
                Metrics::block_topic_blocked_total(version, "outbound").increment(1);
            }
            return Err(HandlerEncodeError::UnknownTopic(topic.hash()));
        }
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
    use alloy_chains::Chain;
    use alloy_consensus::proofs::calculate_transaction_root;
    use alloy_eips::eip7685::EMPTY_REQUESTS_HASH;
    use alloy_primitives::{B256, Signature};
    use alloy_rpc_types_engine::{ExecutionPayloadV2, ExecutionPayloadV3};
    use base_common_consensus::{BaseTxEnvelope, TxDeposit};
    use base_common_genesis::{BaseUpgradeConfig, ChainGenesis, UpgradeConfig};
    use base_common_rpc_types_engine::{BaseExecutionPayload, BaseExecutionPayloadV4, PayloadHash};
    use base_protocol::BaseTimeUpdateTx;
    #[cfg(feature = "metrics")]
    use metrics_exporter_prometheus::PrometheusBuilder;

    use super::*;
    use crate::{v2_valid_block, v3_valid_block, v4_valid_block};

    #[test]
    fn topics_retire_after_each_forks_gossip_age_window() {
        let (_, signer) = tokio::sync::watch::channel(Address::ZERO);
        let handler = BlockHandler::new(
            RollupConfig {
                upgrades: UpgradeConfig {
                    canyon_time: Some(100),
                    ecotone_time: Some(200),
                    isthmus_time: Some(300),
                    ..Default::default()
                },
                ..Default::default()
            },
            signer,
        );
        let all = [
            handler.blocks_v1_topic.hash(),
            handler.blocks_v2_topic.hash(),
            handler.blocks_v3_topic.hash(),
            handler.blocks_v4_topic.hash(),
        ];

        // At fork + 59, a block from the preceding second is still within the
        // 60-second age window. At fork + 60, every pre-fork block is too old.
        for (timestamp, first_topic) in [
            (0, 0),
            (99, 0),
            (100, 0),
            (159, 0),
            (160, 1),
            (259, 1),
            (260, 2),
            (359, 2),
            (360, 3),
            (u64::MAX, 3),
        ] {
            assert_eq!(handler.topics_at(timestamp), all[first_topic..], "timestamp {timestamp}");
        }
    }

    #[test]
    fn topics_respect_unscheduled_simultaneous_and_genesis_forks() {
        let (_, signer) = tokio::sync::watch::channel(Address::ZERO);
        let mut handler = BlockHandler::new(RollupConfig::default(), signer);
        let all = handler.topics_at(0);
        assert_eq!(all.len(), 4);
        assert_eq!(handler.topics_at(u64::MAX), all);

        // Later forks imply their predecessors, even if the earlier timestamps
        // are omitted by a custom chain configuration.
        handler.rollup_config.upgrades.isthmus_time = Some(100);
        assert_eq!(handler.topics_at(159), all);
        assert_eq!(handler.topics_at(160), [handler.blocks_v4_topic.hash()]);

        handler.rollup_config.upgrades.isthmus_time = Some(0);
        assert_eq!(handler.topics_at(0), [handler.blocks_v4_topic.hash()]);
    }

    #[test]
    fn retired_topics_are_ignored_before_decode() {
        #[cfg(feature = "metrics")]
        let recorder = PrometheusBuilder::new().build_recorder();
        #[cfg(feature = "metrics")]
        let _metrics_guard = metrics::set_default_local_recorder(&recorder);

        let (_, signer) = tokio::sync::watch::channel(Address::ZERO);
        let mut handler = BlockHandler::new(
            RollupConfig {
                upgrades: UpgradeConfig { isthmus_time: Some(0), ..Default::default() },
                ..Default::default()
            },
            signer,
        );

        for topic in [
            handler.blocks_v1_topic.hash(),
            handler.blocks_v2_topic.hash(),
            handler.blocks_v3_topic.hash(),
        ] {
            let message = Message {
                source: None,
                sequence_number: None,
                topic,
                // Invalid Snappy would be rejected, not ignored, if decoded.
                data: vec![0xff],
            };
            assert!(matches!(handler.handle(message), (MessageAcceptance::Ignore, None)));
        }
        for topic in [handler.blocks_v4_topic.hash(), IdentTopic::new("unknown").hash()] {
            let message = Message { source: None, sequence_number: None, topic, data: vec![0xff] };
            assert!(matches!(handler.handle(message), (MessageAcceptance::Reject, None)));
        }
        #[cfg(feature = "metrics")]
        {
            let output = recorder.handle().render();
            for version in ["v1", "v2", "v3"] {
                assert!(output.contains(&format!(
                    "base_node_block_topic_blocked_total{{version=\"{version}\",direction=\"inbound\"}} 1"
                )), "{output}");
            }
            assert!(!output.contains("unknown"), "unknown topics must not create metric labels");
        }
    }

    #[test]
    fn retired_topics_cannot_be_encoded_for_gossip() {
        #[cfg(feature = "metrics")]
        let recorder = PrometheusBuilder::new().build_recorder();
        #[cfg(feature = "metrics")]
        let _metrics_guard = metrics::set_default_local_recorder(&recorder);

        let (_, signer) = tokio::sync::watch::channel(Address::ZERO);
        let handler = BlockHandler::new(
            RollupConfig {
                upgrades: UpgradeConfig { isthmus_time: Some(0), ..Default::default() },
                ..Default::default()
            },
            signer,
        );
        let v2 = ExecutionPayloadV2::from_block_slow(&v2_valid_block());
        let envelope = NetworkPayloadEnvelope {
            payload: BaseExecutionPayload::V2(v2),
            signature: Signature::test_signature(),
            payload_hash: PayloadHash(B256::ZERO),
            parent_beacon_block_root: None,
        };
        assert!(matches!(
            handler.encode(handler.blocks_v2_topic.clone(), envelope),
            Err(HandlerEncodeError::UnknownTopic(topic)) if topic == handler.blocks_v2_topic.hash()
        ));
        #[cfg(feature = "metrics")]
        assert!(recorder.handle().render().contains(
            "base_node_block_topic_blocked_total{version=\"v2\",direction=\"outbound\"} 1"
        ));
    }

    #[test]
    fn denim_schedules_are_checked_before_gossip_acceptance() {
        let template = v4_valid_block();
        let activation = template.header.timestamp;
        let config = RollupConfig {
            l2_chain_id: Chain::base_mainnet(),
            block_time: 2,
            genesis: ChainGenesis { l2_time: activation - 2, ..Default::default() },
            upgrades: UpgradeConfig {
                isthmus_time: Some(0),
                base: BaseUpgradeConfig { denim: Some(activation), ..Default::default() },
                ..Default::default()
            },
            ..Default::default()
        };

        for (number, timestamp, millis, valid) in [
            (0, activation - 2, None, true),
            (1, activation, Some(0), true),
            (2, activation, Some(200), true),
            (3, activation, Some(400), true),
            (4, activation, Some(600), true),
            (5, activation, Some(800), true),
            (6, activation + 1, Some(0), true),
            (2, activation + 1, Some(200), false),
            (2, activation - 1, Some(200), false),
            (2, activation, Some(400), false),
            (1, activation, None, false),
            (2, activation, None, false),
        ] {
            let mut block = template.clone();
            block.header.number = number;
            block.header.timestamp = timestamp;
            block.header.requests_hash = Some(EMPTY_REQUESTS_HASH);
            block.body.transactions = vec![BaseTxEnvelope::from(TxDeposit::default())];
            if let Some(millis) = millis {
                block
                    .body
                    .transactions
                    .push(BaseTimeUpdateTx::new(millis).unwrap().into_deposit_tx(number).into());
            }
            block.header.transactions_root = calculate_transaction_root(&block.body.transactions);

            let envelope = NetworkPayloadEnvelope {
                payload: BaseExecutionPayload::V4(
                    BaseExecutionPayloadV4::from_v3_with_withdrawals_root(
                        ExecutionPayloadV3::from_block_slow(&block),
                        block.header.withdrawals_root.unwrap(),
                    ),
                ),
                signature: Signature::test_signature(),
                payload_hash: PayloadHash(B256::ZERO),
                parent_beacon_block_root: block.header.parent_beacon_block_root,
            };
            let encoded = envelope.encode_v4().unwrap();
            let decoded = NetworkPayloadEnvelope::decode_v4(&encoded).unwrap();
            let msg = decoded.payload_hash.signature_message(config.l2_chain_id.id());
            let signer = decoded.signature.recover_address_from_prehash(&msg).unwrap();
            let (_, signer_recv) = tokio::sync::watch::channel(signer);
            let mut handler = BlockHandler::new(config.clone(), signer_recv);
            let message = Message {
                source: None,
                sequence_number: None,
                topic: handler.blocks_v4_topic.clone().into(),
                data: encoded,
            };

            let (acceptance, forwarded) = handler.handle(message.clone());
            assert!(
                matches!(
                    (valid, acceptance),
                    (true, MessageAcceptance::Accept) | (false, MessageAcceptance::Reject)
                ),
                "block {number}, timestamp {timestamp}, millis {millis:?}"
            );
            assert_eq!(forwarded.is_some(), valid);

            // Only accepted blocks become seen; invalid schedules must remain rejected on replay.
            let (acceptance, forwarded) = handler.handle(message);
            assert!(matches!(
                (valid, acceptance),
                (true, MessageAcceptance::Ignore) | (false, MessageAcceptance::Reject)
            ));
            assert!(forwarded.is_none());
        }
    }

    #[test]
    fn test_valid_decode() {
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

        // TRICK: Since the decode method recomputes the payload hash, we need to change the unsafe
        // signer in the handler to ensure that the payload won't be rejected for invalid
        // signature.
        let encoded = handler.encode(handler.blocks_v2_topic.clone(), envelope).unwrap();
        let decoded = NetworkPayloadEnvelope::decode_v2(&encoded).unwrap();

        let msg = decoded.payload_hash.signature_message(8453);
        let signer = decoded.signature.recover_address_from_prehash(&msg).unwrap();
        let (_, unsafe_signer) = tokio::sync::watch::channel(signer);
        handler.signer_recv = unsafe_signer;

        // Let's try to encode a message.
        let message = Message {
            source: None,
            sequence_number: None,
            topic: handler.blocks_v2_topic.clone().into(),
            data: encoded,
        };

        assert!(matches!(handler.handle(message).0, MessageAcceptance::Accept));
    }

    /// This payload has a wrong hash so the signature won't be valid.
    #[test]
    fn test_invalid_decode_payload_hash() {
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

        // Let's try to encode a message.
        let message = Message {
            source: None,
            sequence_number: None,
            topic: handler.blocks_v2_topic.clone().into(),
            data: handler.encode(handler.blocks_v2_topic.clone(), envelope).unwrap(),
        };

        assert!(matches!(handler.handle(message).0, MessageAcceptance::Reject));
    }

    /// The message contains a wrong version so the payload won't be properly decoded.
    #[test]
    fn test_invalid_decode_version_mismatch() {
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

        let encoded = handler.encode(handler.blocks_v2_topic.clone(), envelope).unwrap();

        // Let's try to encode a message.
        let message = Message {
            source: None,
            sequence_number: None,
            // Version mismatch!
            topic: handler.blocks_v1_topic.clone().into(),
            data: encoded,
        };

        assert!(matches!(handler.handle(message).0, MessageAcceptance::Reject));
    }

    /// The message contains a wrong version so the payload won't be properly decoded.
    #[test]
    fn test_invalid_decode_version_mismatch_v3_with_v2() {
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

        let encoded = handler.encode(handler.blocks_v3_topic.clone(), envelope).unwrap();

        // Let's try to encode a message.
        let message = Message {
            source: None,
            sequence_number: None,
            // Version mismatch!
            topic: handler.blocks_v2_topic.clone().into(),
            data: encoded,
        };

        assert!(matches!(handler.handle(message).0, MessageAcceptance::Reject));
    }

    /// The message contains a wrong version so the payload won't be properly decoded.
    #[test]
    fn test_invalid_decode_version_mismatch_v2_with_v3() {
        let block = v2_valid_block();

        let v2 = ExecutionPayloadV2::from_block_slow(&block);

        let payload = BaseExecutionPayload::V2(v2);
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

        let encoded = handler.encode(handler.blocks_v2_topic.clone(), envelope).unwrap();

        // Let's try to encode a message.
        let message = Message {
            source: None,
            sequence_number: None,
            // Version mismatch!
            topic: handler.blocks_v3_topic.clone().into(),
            data: encoded,
        };

        assert!(matches!(handler.handle(message).0, MessageAcceptance::Reject));
    }

    /// The message contains a wrong version so the payload won't be properly decoded.
    #[test]
    fn test_invalid_decode_version_mismatch_v4_with_v3() {
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

        let encoded = handler.encode(handler.blocks_v4_topic.clone(), envelope).unwrap();

        // Let's try to encode a message.
        let message = Message {
            source: None,
            sequence_number: None,
            // Version mismatch!
            topic: handler.blocks_v3_topic.clone().into(),
            data: encoded,
        };

        assert!(matches!(handler.handle(message).0, MessageAcceptance::Reject));
    }

    #[test]
    fn test_valid_decode_v4() {
        let mut block = v4_valid_block();
        block.header.requests_hash = Some(EMPTY_REQUESTS_HASH);

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
            RollupConfig {
                l2_chain_id: Chain::base_mainnet(),
                upgrades: UpgradeConfig { isthmus_time: Some(0), ..Default::default() },
                ..Default::default()
            },
            unsafe_signer,
        );

        // TRICK: Since the decode method recomputes the payload hash, we need to change the unsafe
        // signer in the handler to ensure that the payload won't be rejected for invalid
        // signature.
        let encoded = handler.encode(handler.blocks_v4_topic.clone(), envelope).unwrap();
        let decoded = NetworkPayloadEnvelope::decode_v4(&encoded).unwrap();

        let msg = decoded.payload_hash.signature_message(8453);
        let signer = decoded.signature.recover_address_from_prehash(&msg).unwrap();
        let (_, unsafe_signer) = tokio::sync::watch::channel(signer);
        handler.signer_recv = unsafe_signer;

        // Let's try to encode a message.
        let message = Message {
            source: None,
            sequence_number: None,
            topic: handler.blocks_v4_topic.clone().into(),
            data: encoded,
        };

        assert!(matches!(handler.handle(message).0, MessageAcceptance::Accept));
    }

    #[test]
    fn test_valid_decode_v3() {
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

        // TRICK: Since the decode method recomputes the payload hash, we need to change the unsafe
        // signer in the handler to ensure that the payload won't be rejected for invalid
        // signature.
        let encoded = handler.encode(handler.blocks_v3_topic.clone(), envelope).unwrap();
        let decoded = NetworkPayloadEnvelope::decode_v3(&encoded).unwrap();

        let msg = decoded.payload_hash.signature_message(8453);
        let signer = decoded.signature.recover_address_from_prehash(&msg).unwrap();
        let (_, unsafe_signer) = tokio::sync::watch::channel(signer);
        handler.signer_recv = unsafe_signer;

        // Let's try to encode a message.
        let message = Message {
            source: None,
            sequence_number: None,
            topic: handler.blocks_v3_topic.clone().into(),
            data: encoded,
        };

        assert!(matches!(handler.handle(message).0, MessageAcceptance::Accept));
    }
}
