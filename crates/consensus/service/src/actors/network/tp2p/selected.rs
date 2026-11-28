use std::collections::{HashSet, VecDeque};

use alloy_primitives::{Address, B256};
use async_trait::async_trait;
use base_common_rpc_types_engine::{BaseExecutionPayloadEnvelope, NetworkPayloadEnvelope};
use base_consensus_gossip::P2pRpcRequest;

use crate::actors::network::{
    NetworkActorError, NetworkBuilder, NetworkConfig, NetworkHandler,
    tp2p::{Tp2pConfig, Tp2pMode, Tp2pStartError, Tp2pTransport},
    transport::GossipTransport,
};

/// Block hashes recently forwarded to the engine, so a payload that arrives over both libp2p and
/// tp2p reaches the engine once.
///
/// Both transports' handlers reject payloads more than [`base_consensus_gossip::BlockHandler::MAX_BLOCK_AGE`]
/// seconds old, so a duplicate can only arrive within that window; the capacity covers it at
/// several blocks per second, with room for several blocks per height.
#[derive(Debug, Default)]
pub struct RecentBlockHashes {
    order: VecDeque<B256>,
    hashes: HashSet<B256>,
}

impl RecentBlockHashes {
    /// Hashes remembered.
    pub const CAPACITY: usize = 2048;

    /// Records a hash; returns whether it was not already recorded.
    pub fn insert(&mut self, hash: B256) -> bool {
        if !self.hashes.insert(hash) {
            return false;
        }
        self.order.push_back(hash);
        if self.order.len() > Self::CAPACITY
            && let Some(oldest) = self.order.pop_front()
        {
            self.hashes.remove(&oldest);
        }
        true
    }
}

/// The gossip transports selected by [`Tp2pMode`].
#[derive(Debug)]
pub enum SelectedGossipTransport {
    /// libp2p only ([`Tp2pMode::Off`]).
    Libp2p(NetworkHandler),
    /// tp2p only ([`Tp2pMode::Only`]).
    Tp2p(Tp2pTransport),
    /// Both, each with its own handler and seen-block state ([`Tp2pMode::Both`]).
    Both {
        /// The libp2p transport; its signer signs payloads for both.
        libp2p: NetworkHandler,
        /// The tp2p transport.
        tp2p: Box<Tp2pTransport>,
        /// Payloads already forwarded to the engine.
        delivered: RecentBlockHashes,
    },
}

impl SelectedGossipTransport {
    /// Starts the transports selected by `tp2p` (libp2p alone without it). `libp2p` is built from
    /// `config`, and is neither built nor started in [`Tp2pMode::Only`].
    pub async fn start(
        libp2p: NetworkBuilder,
        config: &NetworkConfig,
        tp2p: Option<&Tp2pConfig>,
    ) -> Result<Self, NetworkActorError> {
        match tp2p.map(|tp2p| (tp2p.mode, tp2p)) {
            None | Some((Tp2pMode::Off, _)) => Ok(Self::Libp2p(libp2p.build()?.start().await?)),
            Some((Tp2pMode::Both, tp2p)) => {
                let libp2p = libp2p.build()?.start().await?;
                let tp2p = Box::new(Tp2pTransport::start(
                    config.rollup_config.clone(),
                    config.unsafe_block_signer,
                    tp2p,
                    None,
                )?);
                Ok(Self::Both { libp2p, tp2p, delivered: RecentBlockHashes::default() })
            }
            Some((Tp2pMode::Only, tp2p)) => {
                let signer = match config.gossip_signer.clone() {
                    Some(signer) => Some(signer.start().await.map_err(Tp2pStartError::from)?),
                    None => None,
                };
                Ok(Self::Tp2p(Tp2pTransport::start(
                    config.rollup_config.clone(),
                    config.unsafe_block_signer,
                    tp2p,
                    signer,
                )?))
            }
        }
    }
}

#[async_trait]
impl GossipTransport for SelectedGossipTransport {
    type Error = NetworkActorError;

    async fn publish(&mut self, block: BaseExecutionPayloadEnvelope) -> Result<(), Self::Error> {
        match self {
            Self::Libp2p(libp2p) => libp2p.publish(block).await,
            Self::Tp2p(tp2p) => tp2p.publish(block).await,
            Self::Both { libp2p, tp2p, .. } => {
                // Sign once, so both networks carry the same bytes.
                let Some(signer) = libp2p.signer.as_ref() else {
                    warn!(target: "net", "No local signer available to sign the payload");
                    return Ok(());
                };
                let chain_id = libp2p.discovery.chain_id;
                let sender_address = *libp2p.unsafe_block_signer_sender.borrow();
                let payload = signer.sign_payload(block, chain_id, sender_address).await?;
                libp2p.publish_signed(payload.clone());
                tp2p.publish_signed(payload).await;
                Ok(())
            }
        }
    }

    async fn next_unsafe_block(&mut self) -> Option<NetworkPayloadEnvelope> {
        match self {
            Self::Libp2p(libp2p) => libp2p.next_unsafe_block().await,
            Self::Tp2p(tp2p) => tp2p.next_unsafe_block().await,
            Self::Both { libp2p, tp2p, delivered } => loop {
                let (payload, transport) = tokio::select! {
                    payload = libp2p.next_unsafe_block() => (payload?, "libp2p"),
                    payload = tp2p.next_unsafe_block() => (payload?, "tp2p"),
                };
                let block_hash = payload.payload.block_hash();
                if delivered.insert(block_hash) {
                    debug!(target: "net", block_hash = %block_hash, transport, "Unsafe payload arrived first");
                    return Some(payload);
                }
                debug!(target: "net", block_hash = %block_hash, transport, "Unsafe payload already delivered");
            },
        }
    }

    fn set_block_signer(&mut self, address: Address) {
        match self {
            Self::Libp2p(libp2p) => libp2p.set_block_signer(address),
            Self::Tp2p(tp2p) => tp2p.set_block_signer(address),
            Self::Both { libp2p, tp2p, .. } => {
                libp2p.set_block_signer(address);
                tp2p.set_block_signer(address);
            }
        }
    }

    fn clear_pending_connections(&mut self) -> usize {
        match self {
            Self::Libp2p(libp2p) | Self::Both { libp2p, .. } => libp2p.clear_pending_connections(),
            Self::Tp2p(tp2p) => tp2p.clear_pending_connections(),
        }
    }

    fn handle_p2p_rpc(&mut self, request: P2pRpcRequest) {
        match self {
            Self::Libp2p(libp2p) | Self::Both { libp2p, .. } => libp2p.handle_p2p_rpc(request),
            Self::Tp2p(tp2p) => tp2p.handle_p2p_rpc(request),
        }
    }
}
