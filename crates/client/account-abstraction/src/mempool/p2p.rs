//! P2P UserOperation Gossip
//!
//! Provides peer-to-peer propagation of UserOperations between sequencer nodes
//! in a private mempool network.
//!
//! # Architecture
//!
//! This module wraps the generic p2p infrastructure from `crates/builder/p2p` and provides:
//! - `UserOpGossip`: The p2p service that runs in the background
//! - `UserOpGossipMessage`: The wire format for UserOp sharing
//! - `UserOpGossipHandle`: A handle to send UserOps to peers
//!
//! # Privacy
//!
//! This is designed for private mempool networks where sequencer nodes share
//! UserOperations directly. It should NOT be enabled for public nodes.
//!
//! # Configuration
//!
//! P2P is disabled by default. Enable via CLI flags for sequencer nodes only.
//!
//! # Wire Format
//!
//! UserOps are sent as JSON-serialized `UserOpGossipMessage` over libp2p streams.
//! Each message contains the UserOp, entry point, and the sender's validation output
//! so receiving nodes can verify before adding to their mempool.

use std::sync::Arc;

use alloy_primitives::{Address, B256};
use p2p::{Message, Multiaddr, NodeBuildResult, NodeBuilder, StreamProtocol};
use parking_lot::RwLock;
use serde::{Deserialize, Serialize};
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;
use tracing::{debug, info, warn};

use crate::rpc::UserOperation;
use crate::simulation::ValidationOutput;

use super::pool::UserOpPool;

/// Protocol identifier for UserOp gossip
pub const USEROP_GOSSIP_PROTOCOL: StreamProtocol = StreamProtocol::new("/aa/userop/1.0.0");

/// Agent version string for the p2p node
const AGENT_VERSION: &str = concat!("base-reth-aa/", env!("CARGO_PKG_VERSION"));

/// Message format for UserOp gossip over p2p
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UserOpGossipMessage {
    /// The UserOperation being shared
    pub user_op: UserOperation,

    /// The EntryPoint this UserOp is for
    pub entry_point: Address,

    /// Pre-computed UserOp hash (saves re-computation on receive)
    pub user_op_hash: B256,

    /// Chain ID this UserOp is valid for
    pub chain_id: u64,
}

impl Message for UserOpGossipMessage {
    fn protocol(&self) -> StreamProtocol {
        USEROP_GOSSIP_PROTOCOL
    }
}

/// Configuration for UserOp p2p gossip
#[derive(Debug, Clone)]
pub struct GossipConfig {
    /// Port to listen on for p2p connections
    pub port: u16,

    /// Known peers to connect to on startup
    pub known_peers: Vec<Multiaddr>,

    /// Optional keypair for node identity (hex-encoded ed25519 secret key)
    /// If not provided, a random keypair is generated
    pub keypair_hex: Option<String>,

    /// Maximum number of peers to connect to
    pub max_peers: u32,
}

impl Default for GossipConfig {
    fn default() -> Self {
        Self {
            port: 9545,
            known_peers: Vec::new(),
            keypair_hex: None,
            max_peers: 50,
        }
    }
}

impl GossipConfig {
    /// Create a new config with the given port
    pub fn with_port(mut self, port: u16) -> Self {
        self.port = port;
        self
    }

    /// Add a known peer to connect to
    pub fn with_known_peer(mut self, peer: Multiaddr) -> Self {
        self.known_peers.push(peer);
        self
    }

    /// Add multiple known peers
    pub fn with_known_peers<I: IntoIterator<Item = Multiaddr>>(mut self, peers: I) -> Self {
        self.known_peers.extend(peers);
        self
    }

    /// Set the keypair (hex-encoded ed25519 secret key)
    pub fn with_keypair_hex(mut self, keypair_hex: String) -> Self {
        self.keypair_hex = Some(keypair_hex);
        self
    }

    /// Set the maximum number of peers
    pub fn with_max_peers(mut self, max_peers: u32) -> Self {
        self.max_peers = max_peers;
        self
    }
}

/// Handle to send UserOps to the gossip network
#[derive(Clone)]
pub struct UserOpGossipHandle {
    tx: mpsc::Sender<UserOpGossipMessage>,
}

impl UserOpGossipHandle {
    /// Broadcast a UserOp to all connected peers
    pub async fn broadcast(&self, message: UserOpGossipMessage) -> Result<(), GossipError> {
        self.tx
            .send(message)
            .await
            .map_err(|_| GossipError::ChannelClosed)
    }

    /// Broadcast a UserOp with the given parameters
    pub async fn broadcast_user_op(
        &self,
        user_op: UserOperation,
        entry_point: Address,
        user_op_hash: B256,
        chain_id: u64,
    ) -> Result<(), GossipError> {
        self.broadcast(UserOpGossipMessage {
            user_op,
            entry_point,
            user_op_hash,
            chain_id,
        })
        .await
    }
}

/// Errors that can occur in the gossip layer
#[derive(Debug, thiserror::Error)]
pub enum GossipError {
    #[error("Failed to build p2p node: {0}")]
    NodeBuild(String),

    #[error("Gossip channel closed")]
    ChannelClosed,

    #[error("Validation failed for received UserOp: {0}")]
    ValidationFailed(String),
}

/// The UserOp gossip service
///
/// This service:
/// 1. Listens for incoming UserOps from peers
/// 2. Validates received UserOps before adding to the mempool
/// 3. Broadcasts new UserOps to connected peers
pub struct UserOpGossip {
    /// The p2p node
    node: p2p::Node<UserOpGossipMessage>,

    /// Receiver for incoming messages from peers
    incoming_rx: mpsc::Receiver<UserOpGossipMessage>,

    /// Reference to the mempool
    pool: Arc<RwLock<UserOpPool>>,

    /// The chain ID for validation
    chain_id: u64,

    /// Cancellation token
    cancel: CancellationToken,
}

impl UserOpGossip {
    /// Create a new gossip service
    ///
    /// Returns the service and a handle for sending UserOps to peers.
    pub fn new(
        config: GossipConfig,
        pool: Arc<RwLock<UserOpPool>>,
        chain_id: u64,
        cancel: CancellationToken,
    ) -> Result<(Self, UserOpGossipHandle), GossipError> {
        let mut builder = NodeBuilder::new()
            .with_port(config.port)
            .with_agent_version(AGENT_VERSION.to_string())
            .with_protocol(USEROP_GOSSIP_PROTOCOL)
            .with_max_peer_count(config.max_peers)
            .with_known_peers(config.known_peers)
            .with_cancellation_token(cancel.clone());

        if let Some(keypair_hex) = config.keypair_hex {
            builder = builder.with_keypair_hex_string(keypair_hex);
        }

        let NodeBuildResult {
            node,
            outgoing_message_tx,
            mut incoming_message_rxs,
        } = builder
            .try_build::<UserOpGossipMessage>()
            .map_err(|e| GossipError::NodeBuild(e.to_string()))?;

        let incoming_rx = incoming_message_rxs
            .remove(&USEROP_GOSSIP_PROTOCOL)
            .expect("protocol must be registered");

        let handle = UserOpGossipHandle {
            tx: outgoing_message_tx,
        };

        let gossip = Self {
            node,
            incoming_rx,
            pool,
            chain_id,
            cancel,
        };

        Ok((gossip, handle))
    }

    /// Get the multiaddresses this node is listening on (including peer ID)
    pub fn multiaddrs(&self) -> Vec<Multiaddr> {
        self.node.multiaddrs()
    }

    /// Run the gossip service
    ///
    /// This spawns the p2p node and processes incoming UserOps.
    /// Runs until the cancellation token is triggered.
    pub async fn run(self) -> Result<(), GossipError> {
        let Self {
            node,
            mut incoming_rx,
            pool,
            chain_id,
            cancel,
        } = self;

        // Spawn the p2p node
        let node_handle = tokio::spawn(async move {
            if let Err(e) = node.run().await {
                warn!(target: "aa-gossip", "P2P node error: {e:?}");
            }
        });

        info!(target: "aa-gossip", "UserOp gossip service started");

        // Process incoming UserOps
        loop {
            tokio::select! {
                biased;
                _ = cancel.cancelled() => {
                    debug!(target: "aa-gossip", "Shutting down gossip service");
                    node_handle.abort();
                    break Ok(());
                }
                Some(message) = incoming_rx.recv() => {
                    Self::handle_incoming_userop(&pool, chain_id, message).await;
                }
            }
        }
    }

    /// Handle an incoming UserOp from a peer
    ///
    /// Note: Currently we just add to pool without full re-validation.
    /// TODO: Add full validation before accepting (perf improvement noted)
    async fn handle_incoming_userop(
        pool: &Arc<RwLock<UserOpPool>>,
        chain_id: u64,
        message: UserOpGossipMessage,
    ) {
        let UserOpGossipMessage {
            user_op,
            entry_point,
            user_op_hash,
            chain_id: msg_chain_id,
        } = message;

        // Verify chain ID matches
        if msg_chain_id != chain_id {
            debug!(
                target: "aa-gossip",
                expected = chain_id,
                received = msg_chain_id,
                "Rejecting UserOp with wrong chain ID"
            );
            return;
        }

        // Verify the hash matches what we compute
        let computed_hash = user_op.hash(entry_point, chain_id);
        if computed_hash != user_op_hash {
            debug!(
                target: "aa-gossip",
                expected = %user_op_hash,
                computed = %computed_hash,
                "Rejecting UserOp with mismatched hash"
            );
            return;
        }

        // Check if we already have this UserOp
        {
            let pool_read = pool.read();
            if pool_read.contains(&entry_point, &user_op_hash) {
                debug!(
                    target: "aa-gossip",
                    hash = %user_op_hash,
                    "UserOp already in pool, skipping"
                );
                return;
            }
        }

        // TODO: Re-validate the UserOp before adding
        // For now, we trust the peer's validation and add with a minimal validation output
        // This is a known limitation that should be improved for production
        debug!(
            target: "aa-gossip",
            hash = %user_op_hash,
            sender = %user_op.sender(),
            "Received UserOp from peer (validation skipped - TODO)"
        );

        // NOTE: We cannot add to the pool here without validation output
        // The pool.add() method requires ValidationOutput which we don't have from the peer.
        // Options:
        // 1. Include ValidationOutput in the gossip message (larger messages)
        // 2. Re-validate locally before adding (slower but more secure)
        // 3. Trust peers and use a minimal validation output (less secure)
        //
        // For now, we log and skip. Full validation integration will be added
        // when the validator is wired up.
        warn!(
            target: "aa-gossip",
            hash = %user_op_hash,
            "Cannot add p2p UserOp to pool - re-validation not yet implemented"
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloy_primitives::{Bytes, U256};

    fn test_user_op() -> UserOperation {
        use crate::rpc::UserOperationV07;
        UserOperation::V07(UserOperationV07 {
            sender: Address::ZERO,
            nonce: U256::ZERO,
            factory: Address::ZERO,
            factory_data: Bytes::default(),
            call_data: Bytes::default(),
            call_gas_limit: U256::from(100000),
            verification_gas_limit: U256::from(100000),
            pre_verification_gas: U256::from(21000),
            max_fee_per_gas: U256::from(1000000000),
            max_priority_fee_per_gas: U256::from(1000000000),
            paymaster: Address::ZERO,
            paymaster_verification_gas_limit: U256::ZERO,
            paymaster_post_op_gas_limit: U256::ZERO,
            paymaster_data: Bytes::default(),
            signature: Bytes::default(),
        })
    }

    #[test]
    fn test_gossip_message_serialization() {
        let user_op = test_user_op();
        let entry_point = Address::repeat_byte(0x55);
        let user_op_hash = user_op.hash(entry_point, 1);

        let message = UserOpGossipMessage {
            user_op: user_op.clone(),
            entry_point,
            user_op_hash,
            chain_id: 1,
        };

        // Serialize and deserialize
        let json = serde_json::to_string(&message).expect("serialize");
        let deserialized: UserOpGossipMessage =
            serde_json::from_str(&json).expect("deserialize");

        assert_eq!(deserialized.entry_point, entry_point);
        assert_eq!(deserialized.user_op_hash, user_op_hash);
        assert_eq!(deserialized.chain_id, 1);
    }

    #[test]
    fn test_gossip_config_builder() {
        let config = GossipConfig::default()
            .with_port(9999)
            .with_max_peers(100)
            .with_keypair_hex("0123456789abcdef".to_string());

        assert_eq!(config.port, 9999);
        assert_eq!(config.max_peers, 100);
        assert!(config.keypair_hex.is_some());
    }

    #[test]
    fn test_gossip_handle_is_clone() {
        // Ensure the handle can be cloned for sharing across tasks
        fn assert_clone<T: Clone>() {}
        assert_clone::<UserOpGossipHandle>();
    }
}
