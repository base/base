//! Contains the [EncryptedRelayExtension] which wires up the encrypted relay RPC modules.

use std::path::Path;
use std::sync::Arc;

use alloy_eips::eip2718::Decodable2718;
use alloy_primitives::{Address, Bytes};
use alloy_provider::ProviderBuilder;
use base_reth_relay::{RelayConfigCache, RelayConfigReaderConfig, SequencerKeypair};
use base_reth_relay::p2p::{
    AckMessage, EncryptedRelayProtoHandler, ForwardRequest, NetworkPeerSender,
    PeerId, ProtocolEvent, ProtocolEventHandler, RelayForwarder, SequencerEventHandler,
    SequencerPeerTracker, SequencerProcessor, TransactionSubmitter, create_forward_channel, create_receive_channel,
};
use base_reth_rpc::{EncryptedRelayApiImpl, EncryptedRelayApiServer};
use parking_lot::Mutex;
use reth::builder::NodeHandleFor;
use reth_network::protocol::IntoRlpxSubProtocol;
use reth_optimism_node::OpNode;
use reth_optimism_primitives::OpTransactionSigned;
use reth_primitives_traits::SignedTransaction;
use reth_transaction_pool::{PoolTransaction, TransactionOrigin, TransactionPool};
use tokio::sync::mpsc;
use tracing::{info, warn, error, debug};

use crate::{
    BaseNodeConfig,
    extensions::{BaseNodeExtension, ConfigurableBaseNodeExtension, OpBuilder},
};

/// Configuration for fetching relay parameters from an on-chain contract.
#[derive(Debug, Clone)]
pub struct RelayFetcherConfig {
    /// L1 RPC URL for fetching relay config.
    pub rpc_url: String,
    /// Address of the EncryptedRelayConfig contract.
    pub contract_address: Address,
    /// Poll interval in seconds.
    pub poll_interval_secs: u64,
}

/// Configuration for the encrypted relay extension.
#[derive(Debug, Clone)]
pub struct EncryptedRelayConfig {
    /// Encryption public key (32 bytes, hex-encoded or raw).
    pub encryption_pubkey: [u8; 32],
    /// Attestation public key (32 bytes).
    pub attestation_pubkey: [u8; 32],
    /// PoW difficulty (leading zero bits).
    pub pow_difficulty: u8,
    /// Optional fetcher config for polling on-chain parameters.
    pub fetcher: Option<RelayFetcherConfig>,
    /// Whether this node runs as sequencer (decrypts and processes transactions).
    pub is_sequencer: bool,
    /// Path to Ed25519 keypair file (required for sequencer mode).
    pub keypair_path: Option<String>,
    /// Hex-encoded Ed25519 keypair (alternative to file, for devnet/testing).
    pub keypair_hex: Option<String>,
    /// Generate a new keypair on startup (for devnet).
    pub keypair_generate: bool,
    /// URL of the sequencer's RPC for HTTP forwarding (relay mode only).
    pub sequencer_url: Option<String>,
}

impl Default for EncryptedRelayConfig {
    fn default() -> Self {
        Self {
            encryption_pubkey: [0u8; 32],
            attestation_pubkey: [0u8; 32],
            pow_difficulty: base_reth_relay::DEFAULT_DIFFICULTY,
            fetcher: None,
            is_sequencer: false,
            keypair_path: None,
            keypair_hex: None,
            keypair_generate: false,
            sequencer_url: None,
        }
    }
}

/// Extension that wires up the encrypted relay RPC endpoints.
pub struct EncryptedRelayExtension {
    /// Shared relay configuration cache.
    pub config_cache: Arc<RelayConfigCache>,
    /// Whether this node is a sequencer.
    pub is_sequencer: bool,
    /// Sequencer keypair (only set if is_sequencer is true).
    pub keypair: Option<Arc<SequencerKeypair>>,
    /// URL for HTTP forwarding to sequencer (relay mode only).
    pub sequencer_url: Option<String>,
    /// Sender for P2P forward requests (relay mode, passed to RPC).
    p2p_forward_tx: Option<mpsc::Sender<ForwardRequest>>,
    /// Receiver for P2P forward requests (relay mode, consumed by on_node_started).
    p2p_forward_rx: Arc<Mutex<Option<mpsc::Receiver<ForwardRequest>>>>,
}

impl std::fmt::Debug for EncryptedRelayExtension {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("EncryptedRelayExtension")
            .field("config_cache", &self.config_cache)
            .field("is_sequencer", &self.is_sequencer)
            .field("keypair", &self.keypair.as_ref().map(|_| "<keypair>"))
            .field("sequencer_url", &self.sequencer_url)
            .field("p2p_forward_tx", &self.p2p_forward_tx.as_ref().map(|_| "<sender>"))
            .finish()
    }
}

/// TransactionSubmitter that submits transactions directly to the pool.
///
/// This decodes the raw transaction bytes, recovers the sender, and submits
/// directly to the transaction pool without going through HTTP RPC.
struct DirectPoolSubmitter<Pool> {
    /// The transaction pool.
    pool: Pool,
    /// Network sender for sending acks back to peers (not currently used).
    #[allow(dead_code)]
    network_sender: Arc<NetworkPeerSender>,
}

impl<Pool> DirectPoolSubmitter<Pool>
where
    Pool: TransactionPool + Clone + 'static,
{
    /// Creates a new direct pool submitter.
    fn new(pool: Pool, network_sender: Arc<NetworkPeerSender>) -> Self {
        Self { pool, network_sender }
    }
}

impl<Pool> TransactionSubmitter for DirectPoolSubmitter<Pool>
where
    Pool: TransactionPool + Clone + Send + Sync + 'static,
    Pool::Transaction: PoolTransaction<Consensus = OpTransactionSigned>,
    <Pool::Transaction as PoolTransaction>::TryFromConsensusError: std::fmt::Display,
{
    async fn submit_transaction(&self, raw_tx: Bytes) -> Result<(), String> {
        // Decode the EIP-2718 transaction
        let signed_tx = OpTransactionSigned::decode_2718_exact(&mut raw_tx.as_ref())
            .map_err(|e| format!("Failed to decode transaction: {e}"))?;

        // Recover sender from signature
        let recovered = signed_tx.try_into_recovered()
            .map_err(|_| "Failed to recover sender from signature".to_string())?;

        // Get the transaction hash for logging
        let tx_hash = alloy_primitives::B256::from(*recovered.tx_hash());

        // Create pool transaction from recovered transaction using try_from_consensus
        let pool_tx = Pool::Transaction::try_from_consensus(recovered)
            .map_err(|e| format!("Failed to convert to pool transaction: {e}"))?;

        // Submit to pool
        match self.pool.add_transaction(TransactionOrigin::External, pool_tx).await {
            Ok(_) => {
                info!(
                    tx_hash = %tx_hash,
                    tx_size = raw_tx.len(),
                    "Submitted P2P transaction to pool (direct)"
                );
                Ok(())
            }
            Err(e) => {
                warn!(
                    error = %e,
                    tx_hash = %tx_hash,
                    tx_size = raw_tx.len(),
                    "Failed to add transaction to pool"
                );
                Err(format!("Pool submission failed: {e}"))
            }
        }
    }

    fn send_ack(&self, peer_id: PeerId, ack: AckMessage) {
        // Convert PeerId (B256/32 bytes) to connection PeerId (64 bytes)
        // Note: This won't work correctly because we lose the connection peer ID mapping
        // For now, just log that we would send an ack
        debug!(
            peer = %peer_id,
            request_id = ack.request_id,
            accepted = ack.accepted,
            "Would send ack to peer (not implemented)"
        );
    }
}

impl EncryptedRelayExtension {
    /// Creates a new relay extension with the given config.
    ///
    /// If `config.fetcher` is set, this will start background polling for
    /// on-chain config updates from L1.
    ///
    /// If `config.is_sequencer` is true, loads or generates a keypair for decryption.
    /// Otherwise, sets up HTTP forwarding if `sequencer_url` is provided.
    pub fn new(config: EncryptedRelayConfig) -> Self {
        // Load or generate keypair if sequencer mode
        let keypair = if config.is_sequencer {
            let kp = if config.keypair_generate {
                info!("Generating new sequencer keypair (devnet mode)");
                let kp = SequencerKeypair::generate();
                info!(
                    x25519_pubkey = %alloy_primitives::hex::encode(kp.x25519_public_key()),
                    ed25519_pubkey = %alloy_primitives::hex::encode(kp.ed25519_public_key()),
                    "Generated sequencer keypair"
                );
                Some(kp)
            } else if let Some(ref hex_key) = config.keypair_hex {
                match SequencerKeypair::from_hex(hex_key) {
                    Ok(kp) => {
                        info!(
                            x25519_pubkey = %alloy_primitives::hex::encode(kp.x25519_public_key()),
                            ed25519_pubkey = %alloy_primitives::hex::encode(kp.ed25519_public_key()),
                            "Loaded sequencer keypair from hex"
                        );
                        Some(kp)
                    }
                    Err(e) => {
                        error!(error = %e, "Failed to parse sequencer keypair from hex");
                        None
                    }
                }
            } else if let Some(ref path) = config.keypair_path {
                match SequencerKeypair::load_from_file(Path::new(path)) {
                    Ok(kp) => {
                        info!(
                            path = %path,
                            x25519_pubkey = %alloy_primitives::hex::encode(kp.x25519_public_key()),
                            ed25519_pubkey = %alloy_primitives::hex::encode(kp.ed25519_public_key()),
                            "Loaded sequencer keypair from file"
                        );
                        Some(kp)
                    }
                    Err(e) => {
                        error!(error = %e, path = %path, "Failed to load sequencer keypair");
                        None
                    }
                }
            } else {
                error!("Sequencer mode requires --relay-keypair, --relay-keypair-hex, or --relay-keypair-generate");
                None
            };
            kp.map(Arc::new)
        } else {
            None
        };

        // Build config cache - use keypair's pubkeys if available
        let (enc_pk, att_pk) = if let Some(ref kp) = keypair {
            (kp.x25519_public_key(), kp.ed25519_public_key())
        } else {
            (config.encryption_pubkey, config.attestation_pubkey)
        };

        let mut cache = RelayConfigCache::with_defaults(enc_pk, att_pk);

        // Update difficulty if non-default
        if config.pow_difficulty != base_reth_relay::DEFAULT_DIFFICULTY {
            let mut params = (*cache.get()).clone();
            params.pow_difficulty = config.pow_difficulty;
            cache.update(params);
        }

        // Start config polling if fetcher is configured
        if let Some(ref fetcher_config) = config.fetcher {
            info!(
                rpc_url = %fetcher_config.rpc_url,
                contract = %fetcher_config.contract_address,
                poll_interval_secs = fetcher_config.poll_interval_secs,
                "Starting relay config polling from L1"
            );

            match fetcher_config.rpc_url.parse() {
                Ok(url) => {
                    let provider = ProviderBuilder::new().connect_http(url);
                    let reader_config = RelayConfigReaderConfig {
                        contract_address: fetcher_config.contract_address,
                        poll_interval_secs: fetcher_config.poll_interval_secs,
                    };
                    cache.start_polling(provider, reader_config);
                }
                Err(e) => {
                    warn!(
                        error = %e,
                        url = %fetcher_config.rpc_url,
                        "Failed to parse relay config RPC URL, polling disabled"
                    );
                }
            }
        }

        // Create P2P forward channel for relay mode (if no HTTP URL specified)
        let (p2p_forward_tx, p2p_forward_rx) = if !config.is_sequencer && config.sequencer_url.is_none() {
            let (tx, rx) = create_forward_channel(1024);
            (Some(tx), Arc::new(Mutex::new(Some(rx))))
        } else {
            (None, Arc::new(Mutex::new(None)))
        };

        Self {
            config_cache: Arc::new(cache),
            is_sequencer: config.is_sequencer,
            keypair,
            sequencer_url: config.sequencer_url,
            p2p_forward_tx,
            p2p_forward_rx,
        }
    }

    /// Creates an extension from an existing config cache.
    pub fn with_cache(config_cache: Arc<RelayConfigCache>) -> Self {
        Self {
            config_cache,
            is_sequencer: false,
            keypair: None,
            sequencer_url: None,
            p2p_forward_tx: None,
            p2p_forward_rx: Arc::new(Mutex::new(None)),
        }
    }

    /// Returns the sequencer keypair if this node is a sequencer.
    pub fn sequencer_keypair(&self) -> Option<&Arc<SequencerKeypair>> {
        self.keypair.as_ref()
    }

    /// Returns the X25519 secret key bytes for decryption (sequencer only).
    pub fn decryption_key(&self) -> Option<[u8; 32]> {
        self.keypair.as_ref().map(|kp| kp.x25519_secret_bytes())
    }
}

impl BaseNodeExtension for EncryptedRelayExtension {
    fn apply(&self, builder: OpBuilder) -> OpBuilder {
        let config_cache = self.config_cache.clone();
        let is_sequencer = self.is_sequencer;
        let sequencer_url = self.sequencer_url.clone();
        let p2p_forward_tx = self.p2p_forward_tx.clone();
        // Extract decryption key before the closure
        let decryption_key = self.decryption_key();

        builder.extend_rpc_modules(move |ctx| {
            if is_sequencer {
                if let Some(key) = decryption_key {
                    info!(message = "Starting Encrypted Relay RPC (sequencer mode with pool submission)");
                    // Use eth_api for direct pool submission
                    let eth_api = ctx.registry.eth_api().clone();
                    let relay_api = EncryptedRelayApiImpl::with_eth_api(config_cache.clone(), key, eth_api);
                    ctx.modules.merge_configured(relay_api.into_rpc())?;
                } else {
                    warn!(message = "Starting Encrypted Relay RPC (sequencer mode WITHOUT decryption - no keypair)");
                    let relay_api = EncryptedRelayApiImpl::new(config_cache.clone());
                    ctx.modules.merge_configured(relay_api.into_rpc())?;
                }
            } else if let Some(url) = sequencer_url.clone() {
                info!(
                    message = "Starting Encrypted Relay RPC (relay mode with HTTP forwarding)",
                    sequencer_url = %url
                );
                let relay_api = EncryptedRelayApiImpl::with_http_forwarder(config_cache.clone(), url);
                ctx.modules.merge_configured(relay_api.into_rpc())?;
            } else if let Some(forward_tx) = p2p_forward_tx.clone() {
                info!(message = "Starting Encrypted Relay RPC (relay mode with P2P forwarding)");
                let relay_api = EncryptedRelayApiImpl::with_p2p_forwarder(config_cache.clone(), forward_tx);
                ctx.modules.merge_configured(relay_api.into_rpc())?;
            } else {
                info!(message = "Starting Encrypted Relay RPC (relay mode, no forwarding)");
                let relay_api = EncryptedRelayApiImpl::new(config_cache.clone());
                ctx.modules.merge_configured(relay_api.into_rpc())?;
            }

            Ok(())
        })
    }

    fn on_node_started(&self, node: &NodeHandleFor<OpNode>) -> eyre::Result<()> {
        use reth_network::NetworkProtocols;

        // Create the protocol event channel
        let (events_tx, events_rx) = mpsc::unbounded_channel::<ProtocolEvent>();

        // Create the protocol handler
        let handler = EncryptedRelayProtoHandler::new(events_tx);

        // Register the subprotocol with the network
        info!(
            protocol = %base_reth_relay::p2p::PROTOCOL_NAME,
            version = base_reth_relay::p2p::PROTOCOL_VERSION,
            "Registering encrypted-relay P2P subprotocol"
        );
        node.node.network.add_rlpx_sub_protocol(handler.into_rlpx_sub_protocol());

        // Create shared components
        let network_sender = Arc::new(NetworkPeerSender::new());
        let sequencer_tracker = Arc::new(SequencerPeerTracker::new(self.config_cache.clone()));

        if self.is_sequencer {
            // Sequencer mode: receive encrypted transactions from relay nodes
            let (incoming_tx, incoming_rx) = create_receive_channel(1024);

            // Spawn the sequencer event handler with keypair for identity announcements
            let event_handler = if let Some(ref keypair) = self.keypair {
                info!("Sequencer will send identity announcements to peers");
                SequencerEventHandler::with_keypair(
                    events_rx,
                    incoming_tx,
                    network_sender.clone(),
                    keypair.clone(),
                )
            } else {
                warn!("Sequencer has no keypair - will not send identity announcements");
                SequencerEventHandler::new(
                    events_rx,
                    incoming_tx,
                    network_sender.clone(),
                )
            };
            node.node.task_executor.spawn_critical(
                "encrypted-relay-sequencer-events",
                Box::pin(event_handler.run()),
            );

            // Spawn the SequencerProcessor to handle incoming P2P transactions
            if let Some(ref keypair) = self.keypair {
                let secret_key = keypair.x25519_secret_bytes();
                let config_cache = self.config_cache.clone();
                let network_sender_clone = network_sender.clone();

                // Create direct pool submitter using the node's transaction pool
                let pool = node.node.pool.clone();
                let submitter = Arc::new(DirectPoolSubmitter::new(pool, network_sender_clone));
                info!("DirectPoolSubmitter configured for P2P transaction submission");

                let processor = SequencerProcessor::new(
                    incoming_rx,
                    secret_key,
                    config_cache,
                    submitter,
                );
                node.node.task_executor.spawn_critical(
                    "encrypted-relay-sequencer-processor",
                    Box::pin(processor.run()),
                );
                info!("Encrypted relay P2P subprotocol registered (sequencer mode with direct pool submission)");
            } else {
                info!("Encrypted relay P2P subprotocol registered (sequencer mode without processor)");
                drop(incoming_rx);
            }
        } else {
            // Relay mode: forward encrypted transactions to sequencer
            let event_handler = ProtocolEventHandler::new(
                events_rx,
                network_sender.clone(),
                sequencer_tracker.clone(),
            );
            node.node.task_executor.spawn_critical(
                "encrypted-relay-events",
                Box::pin(event_handler.run()),
            );

            // Take the forward request receiver and spawn the RelayForwarder
            if let Some(forward_rx) = self.p2p_forward_rx.lock().take() {
                let forwarder = RelayForwarder::new(
                    forward_rx,
                    sequencer_tracker.clone(),
                    network_sender.clone(),
                );
                node.node.task_executor.spawn_critical(
                    "encrypted-relay-forwarder",
                    Box::pin(forwarder.run()),
                );
                info!("Encrypted relay P2P subprotocol registered (relay mode with P2P forwarding)");
            } else {
                info!("Encrypted relay P2P subprotocol registered (relay mode)");
            }
        }

        Ok(())
    }
}

impl ConfigurableBaseNodeExtension for EncryptedRelayExtension {
    fn build(config: &BaseNodeConfig) -> eyre::Result<Self> {
        // For now, use defaults. In a real implementation, this would read from
        // CLI args or config file.
        let relay_config = if let Some(ref relay) = config.relay {
            relay.clone()
        } else {
            EncryptedRelayConfig::default()
        };

        Ok(Self::new(relay_config))
    }
}
