//! Gossipsub Config

use std::time::Duration;

use libp2p::{
    connection_limits::ConnectionLimits,
    gossipsub::{Config, ConfigBuilder, Message, MessageId},
};
use openssl::sha::sha256;
use snap::raw::Decoder;

////////////////////////////////////////////////////////////////////////////////////////////////
// GossipSub Constants
////////////////////////////////////////////////////////////////////////////////////////////////

/// The maximum gossip size.
/// Limits the total size of gossip RPC containers as well as decompressed individual messages.
pub const MAX_GOSSIP_SIZE: usize = 10 * (1 << 20);

/// The minimum gossip size.
/// Used to make sure that there is at least some data to validate the signature against.
pub const MIN_GOSSIP_SIZE: usize = 66;

/// The maximum outbound queue.
pub const MAX_OUTBOUND_QUEUE: usize = 256;

/// The maximum validate queue.
pub const MAX_VALIDATE_QUEUE: usize = 256;

/// The global validate throttle.
pub const GLOBAL_VALIDATE_THROTTLE: usize = 512;

/// The default mesh D.
pub const DEFAULT_MESH_D: usize = 8;

/// The default mesh D low.
pub const DEFAULT_MESH_DLO: usize = 6;

/// The default mesh D high.
pub const DEFAULT_MESH_DHI: usize = 12;

/// The default mesh D lazy.
pub const DEFAULT_MESH_DLAZY: usize = 6;

/// The default maximum number of pending inbound connections.
pub const DEFAULT_MAX_PENDING_INCOMING_CONNECTIONS: u32 = 5;

/// The default maximum number of pending outbound connections.
pub const DEFAULT_MAX_PENDING_OUTGOING_CONNECTIONS: u32 = 16;

/// The default maximum number of established libp2p connections.
pub const DEFAULT_MAX_ESTABLISHED_CONNECTIONS: u32 = 30;

/// The default maximum number of established libp2p connections per peer.
pub const DEFAULT_MAX_ESTABLISHED_CONNECTIONS_PER_PEER: u32 = 1;

////////////////////////////////////////////////////////////////////////////////////////////////
// Duration Constants
////////////////////////////////////////////////////////////////////////////////////////////////

/// The gossip heartbeat.
pub const GOSSIP_HEARTBEAT: Duration = Duration::from_millis(500);

/// The seen messages TTL.
/// Limits the duration that message IDs are remembered for gossip deduplication purposes.
pub const SEEN_MESSAGES_TTL: Duration =
    Duration::from_millis(130 * GOSSIP_HEARTBEAT.as_millis() as u64);

/// The peer score inspect frequency.
/// The frequency at which peer scores are inspected.
pub const PEER_SCORE_INSPECT_FREQUENCY: Duration = Duration::from_secs(15);

////////////////////////////////////////////////////////////////////////////////////////////////
// Config Building
////////////////////////////////////////////////////////////////////////////////////////////////

/// Connection limits for the libp2p swarm.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ConnectionLimitsConfig {
    /// Maximum number of pending inbound connections.
    pub max_pending_incoming: u32,
    /// Maximum number of pending outbound connections.
    pub max_pending_outgoing: u32,
    /// Maximum number of established inbound connections.
    pub max_established_incoming: u32,
    /// Maximum number of established outbound connections.
    pub max_established_outgoing: u32,
    /// Maximum number of established connections across all peers and directions.
    pub max_established: u32,
    /// Maximum number of established connections to a single peer.
    pub max_established_per_peer: u32,
}

impl ConnectionLimitsConfig {
    /// Creates a connection limit config using the same cap for inbound, outbound, and total
    /// established connections.
    pub const fn new(max_established: u32) -> Self {
        Self {
            max_pending_incoming: DEFAULT_MAX_PENDING_INCOMING_CONNECTIONS,
            max_pending_outgoing: DEFAULT_MAX_PENDING_OUTGOING_CONNECTIONS,
            max_established_incoming: max_established,
            max_established_outgoing: max_established,
            max_established,
            max_established_per_peer: DEFAULT_MAX_ESTABLISHED_CONNECTIONS_PER_PEER,
        }
    }
}

impl Default for ConnectionLimitsConfig {
    fn default() -> Self {
        Self::new(DEFAULT_MAX_ESTABLISHED_CONNECTIONS)
    }
}

impl From<ConnectionLimitsConfig> for ConnectionLimits {
    fn from(config: ConnectionLimitsConfig) -> Self {
        Self::default()
            .with_max_pending_incoming(Some(config.max_pending_incoming))
            .with_max_pending_outgoing(Some(config.max_pending_outgoing))
            .with_max_established_incoming(Some(config.max_established_incoming))
            .with_max_established_outgoing(Some(config.max_established_outgoing))
            .with_max_established(Some(config.max_established))
            .with_max_established_per_peer(Some(config.max_established_per_peer))
    }
}

/// Builds the default gossipsub configuration.
///
/// Notable defaults:
/// - `flood_publish`: false (call `.flood_publish(true)` on the [`ConfigBuilder`] to enable)
/// - `backoff_slack`: 1
/// - heart beat interval: 1 second
/// - peer exchange is disabled
/// - maximum byte size for gossip messages: 2048 bytes
///
/// # Returns
///
/// A [`ConfigBuilder`] with the default gossipsub configuration already set.
/// Call `.build()` on the returned builder to get the final [`libp2p::gossipsub::Config`].
pub fn default_config_builder() -> ConfigBuilder {
    let mut builder = ConfigBuilder::default();
    builder
        .mesh_n(DEFAULT_MESH_D)
        .mesh_n_low(DEFAULT_MESH_DLO)
        .mesh_n_high(DEFAULT_MESH_DHI)
        .gossip_lazy(DEFAULT_MESH_DLAZY)
        .heartbeat_interval(GOSSIP_HEARTBEAT)
        .fanout_ttl(Duration::from_secs(60))
        .history_length(12)
        .history_gossip(3)
        .flood_publish(false)
        .support_floodsub()
        .max_transmit_size(MAX_GOSSIP_SIZE)
        .duplicate_cache_time(Duration::from_secs(120))
        .connection_handler_queue_len(MAX_OUTBOUND_QUEUE)
        .validation_mode(libp2p::gossipsub::ValidationMode::None)
        .validate_messages()
        .message_id_fn(compute_message_id);

    builder
}

/// Returns the default [Config] for gossipsub.
pub fn default_config() -> Config {
    default_config_builder().build().expect("default gossipsub config must be valid")
}

/// Computes the [`MessageId`] of a `gossipsub` message.
///
/// Reject oversized snappy frames before allocating: `snap::raw::decompress_len`
/// parses only the varu32 preamble and never allocates. Frames whose declared
/// decoded size exceeds [`MAX_GOSSIP_SIZE`] are hashed under the invalid-snappy
/// domain, identical to malformed snappy input. This prevents an anonymous peer
/// from forcing the gossipsub task to allocate hundreds of `MiB` per packet.
fn compute_message_id(msg: &Message) -> MessageId {
    let id = match snap::raw::decompress_len(&msg.data) {
        Ok(declared) if declared > MAX_GOSSIP_SIZE => {
            warn!(target: "cfg", declared, max = MAX_GOSSIP_SIZE, "Rejecting oversized snappy message");
            invalid_snappy_id(&msg.data)
        }
        Ok(_) => {
            let mut decoder = Decoder::new();
            decoder.decompress_vec(&msg.data).map_or_else(
                |_| {
                    warn!(target: "cfg", "Failed to decompress message, using invalid snappy");
                    invalid_snappy_id(&msg.data)
                },
                |data| {
                    let domain_valid_snappy: Vec<u8> = vec![0x1, 0x0, 0x0, 0x0];
                    sha256([domain_valid_snappy.as_slice(), data.as_slice()].concat().as_slice())
                        [..20]
                        .to_vec()
                },
            )
        }
        Err(_) => {
            warn!(target: "cfg", "Failed to read snappy preamble, using invalid snappy");
            invalid_snappy_id(&msg.data)
        }
    };

    MessageId(id)
}

/// Hash `data` under the invalid-snappy domain tag.
fn invalid_snappy_id(data: &[u8]) -> Vec<u8> {
    let domain_invalid_snappy: Vec<u8> = vec![0x0, 0x0, 0x0, 0x0];
    sha256([domain_invalid_snappy.as_slice(), data].concat().as_slice())[..20].to_vec()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_constructs_default_config() {
        let cfg = default_config();
        assert_eq!(cfg.mesh_n(), DEFAULT_MESH_D);
        assert_eq!(cfg.mesh_n_low(), DEFAULT_MESH_DLO);
        assert_eq!(cfg.mesh_n_high(), DEFAULT_MESH_DHI);
    }

    #[test]
    fn test_constructs_default_connection_limits_config() {
        let cfg = ConnectionLimitsConfig::default();
        assert_eq!(cfg.max_pending_incoming, DEFAULT_MAX_PENDING_INCOMING_CONNECTIONS);
        assert_eq!(cfg.max_pending_outgoing, DEFAULT_MAX_PENDING_OUTGOING_CONNECTIONS);
        assert_eq!(cfg.max_established_incoming, DEFAULT_MAX_ESTABLISHED_CONNECTIONS);
        assert_eq!(cfg.max_established_outgoing, DEFAULT_MAX_ESTABLISHED_CONNECTIONS);
        assert_eq!(cfg.max_established, DEFAULT_MAX_ESTABLISHED_CONNECTIONS);
        assert_eq!(cfg.max_established_per_peer, DEFAULT_MAX_ESTABLISHED_CONNECTIONS_PER_PEER);
    }

    #[test]
    fn test_compute_message_id_invalid_snappy() {
        let msg = Message {
            source: None,
            data: vec![1, 2, 3, 4, 5],
            sequence_number: None,
            topic: libp2p::gossipsub::TopicHash::from_raw("test"),
        };

        let id = compute_message_id(&msg);
        let hashed = sha256(&[&[0x0, 0x0, 0x0, 0x0], [1, 2, 3, 4, 5].as_slice()].concat());
        assert_eq!(id.0, hashed[..20].to_vec());
    }

    #[test]
    fn test_compute_message_id_valid_snappy() {
        let compressed = snap::raw::Encoder::new().compress_vec(&[1, 2, 3, 4, 5]).unwrap();
        let msg = Message {
            source: None,
            data: compressed,
            sequence_number: None,
            topic: libp2p::gossipsub::TopicHash::from_raw("test"),
        };

        let id = compute_message_id(&msg);
        let hashed = sha256(&[&[0x1, 0x0, 0x0, 0x0], [1, 2, 3, 4, 5].as_slice()].concat());
        assert_eq!(id.0, hashed[..20].to_vec());
    }

    #[test]
    fn test_compute_message_id_rejects_oversized_snappy_bomb() {
        let huge = vec![0u8; MAX_GOSSIP_SIZE + 1];
        let bomb = snap::raw::Encoder::new().compress_vec(&huge).unwrap();
        assert!(bomb.len() < MAX_GOSSIP_SIZE, "wire size must pass max_transmit_size");
        assert!(snap::raw::decompress_len(&bomb).unwrap() > MAX_GOSSIP_SIZE);

        let msg = Message {
            source: None,
            data: bomb.clone(),
            sequence_number: None,
            topic: libp2p::gossipsub::TopicHash::from_raw("test"),
        };

        let id = compute_message_id(&msg);
        let expected = sha256(&[&[0x0, 0x0, 0x0, 0x0], bomb.as_slice()].concat())[..20].to_vec();
        assert_eq!(id.0, expected, "oversized bomb must hash under the invalid-snappy domain");
    }
}
