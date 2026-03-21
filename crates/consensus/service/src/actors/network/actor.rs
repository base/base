use alloy_primitives::Address;
use async_trait::async_trait;
use base_alloy_rpc_types_engine::OpExecutionPayloadEnvelope;
use base_consensus_gossip::P2pRpcRequest;
use base_consensus_rpc::NetworkAdminQuery;
use base_consensus_sources::BlockSignerError;
use libp2p::TransportError;
use thiserror::Error;
use tokio::{self, select, sync::mpsc};
use tokio_util::sync::{CancellationToken, WaitForCancellationFuture};

use crate::{
    CancellableContext, NetworkEngineClient, NodeActor,
    actors::network::{
        builder::NetworkBuilder, driver::NetworkDriverError, error::NetworkBuilderError,
        handler::NetworkHandler, transport::GossipTransport,
    },
};

/// The network actor handles two core networking components of the rollup node:
/// - *discovery*: Peer discovery over UDP using discv5.
/// - *gossip*: Block gossip over TCP using libp2p.
///
/// `NetworkActor` is generic over the gossip transport backend via [`GossipTransport`]. The
/// production transport is [`NetworkHandler`], which opens real TCP and UDP sockets. Test
/// environments can supply an in-process channel-based transport to avoid binding ports.
///
/// ## Example
///
/// ```rust,ignore
/// use base_consensus_gossip::NetworkDriver;
/// use std::net::{IpAddr, Ipv4Addr, SocketAddr};
///
/// let chain_id = 10;
/// let signer = Address::random();
/// let socket = SocketAddr::new(IpAddr::V4(Ipv4Addr::UNSPECIFIED), 9099);
///
/// // Construct the `Network` using the builder, then wrap it in a `NetworkActor`.
/// // let (inbound_data, actor) = NetworkActor::new(engine_client, token, builder).await?;
/// ```
#[derive(Debug)]
pub struct NetworkActor<E: NetworkEngineClient, T: GossipTransport> {
    /// Gossip and discovery backend.
    pub(super) transport: T,
    /// The cancellation token, shared between all tasks.
    pub(super) cancellation_token: CancellationToken,
    /// A channel to receive the unsafe block signer address.
    pub(super) signer: mpsc::Receiver<Address>,
    /// Handler for p2p RPC Requests.
    pub(super) p2p_rpc: mpsc::Receiver<P2pRpcRequest>,
    /// A channel to receive admin rpc requests.
    pub(super) admin_rpc: mpsc::Receiver<NetworkAdminQuery>,
    /// A channel to receive unsafe blocks and send them through the gossip layer.
    pub(super) publish_rx: mpsc::Receiver<OpExecutionPayloadEnvelope>,
    /// A channel to use to interact with the engine actor.
    pub(super) engine_client: E,
}

/// The inbound data for the network actor.
#[derive(Debug)]
pub struct NetworkInboundData {
    /// A channel to send the unsafe block signer address to the network actor.
    pub signer: mpsc::Sender<Address>,
    /// Handler for p2p RPC Requests sent to the network actor.
    pub p2p_rpc: mpsc::Sender<P2pRpcRequest>,
    /// Handler for admin RPC Requests.
    pub admin_rpc: mpsc::Sender<NetworkAdminQuery>,
    /// A channel to send unsafe blocks to the network actor.
    /// This channel should only be used by the sequencer actor/admin RPC api to forward their
    /// newly produced unsafe blocks to the network actor.
    pub gossip_payload_tx: mpsc::Sender<OpExecutionPayloadEnvelope>,
}

impl<E: NetworkEngineClient> NetworkActor<E, NetworkHandler> {
    /// Constructs a new production [`NetworkActor`] by building a [`NetworkHandler`] from the
    /// provided [`NetworkBuilder`].
    ///
    /// This is the standard constructor for production use. It performs the async startup of the
    /// libp2p swarm and discv5 discovery service before returning.
    pub async fn new(
        engine_client: E,
        cancellation_token: CancellationToken,
        builder: NetworkBuilder,
    ) -> Result<(NetworkInboundData, Self), NetworkActorError> {
        let transport = builder.build()?.start().await?;
        Ok(Self::with_transport(engine_client, cancellation_token, transport))
    }
}

impl<E: NetworkEngineClient, T: GossipTransport> NetworkActor<E, T> {
    /// Constructs a new [`NetworkActor`] with a pre-built [`GossipTransport`].
    ///
    /// Use this constructor when supplying a custom transport, such as a channel-based test
    /// transport that does not bind real network ports.
    pub fn with_transport(
        engine_client: E,
        cancellation_token: CancellationToken,
        transport: T,
    ) -> (NetworkInboundData, Self) {
        let (signer_tx, signer_rx) = mpsc::channel(16);
        let (rpc_tx, rpc_rx) = mpsc::channel(1024);
        let (admin_rpc_tx, admin_rpc_rx) = mpsc::channel(1024);
        let (publish_tx, publish_rx) = mpsc::channel(256);
        let actor = Self {
            transport,
            cancellation_token,
            signer: signer_rx,
            p2p_rpc: rpc_rx,
            admin_rpc: admin_rpc_rx,
            publish_rx,
            engine_client,
        };
        let inbound_data = NetworkInboundData {
            signer: signer_tx,
            p2p_rpc: rpc_tx,
            admin_rpc: admin_rpc_tx,
            gossip_payload_tx: publish_tx,
        };
        (inbound_data, actor)
    }
}

impl<E: NetworkEngineClient, T: GossipTransport> CancellableContext for NetworkActor<E, T> {
    fn cancelled(&self) -> WaitForCancellationFuture<'_> {
        self.cancellation_token.cancelled()
    }
}

/// An error from the network actor.
#[derive(Debug, Error)]
pub enum NetworkActorError {
    /// Network builder error.
    #[error(transparent)]
    NetworkBuilder(#[from] NetworkBuilderError),
    /// Network driver error.
    #[error(transparent)]
    NetworkDriver(#[from] NetworkDriverError),
    /// Driver startup failed.
    #[error(transparent)]
    DriverStartup(#[from] TransportError<std::io::Error>),
    /// The network driver was missing its unsafe block receiver.
    #[error("Missing unsafe block receiver in network driver")]
    MissingUnsafeBlockReceiver,
    /// The network driver was missing its unsafe block signer sender.
    #[error("Missing unsafe block signer in network driver")]
    MissingUnsafeBlockSigner,
    /// Channel closed unexpectedly.
    #[error("Channel closed unexpectedly")]
    ChannelClosed,
    /// Failed to sign the payload.
    #[error("Failed to sign the payload: {0}")]
    FailedToSignPayload(#[from] BlockSignerError),
}

impl From<std::convert::Infallible> for NetworkActorError {
    fn from(e: std::convert::Infallible) -> Self {
        match e {}
    }
}

#[async_trait]
impl<E: NetworkEngineClient + 'static, T: GossipTransport + 'static> NodeActor
    for NetworkActor<E, T>
where
    T::Error: Into<NetworkActorError>,
{
    type Error = NetworkActorError;
    type StartData = ();

    async fn start(mut self, _: Self::StartData) -> Result<(), Self::Error> {
        loop {
            select! {
                _ = self.cancellation_token.cancelled() => {
                    info!(
                        target: "network",
                        "Received shutdown signal. Exiting network task."
                    );
                    return Ok(());
                }
                signer = self.signer.recv() => {
                    let Some(signer) = signer else {
                        warn!(
                            target: "network",
                            "Found no unsafe block signer on receive"
                        );
                        return Err(NetworkActorError::ChannelClosed);
                    };
                    self.transport.set_block_signer(signer);
                }
                Some(block) = self.publish_rx.recv(), if !self.publish_rx.is_closed() => {
                    self.transport.publish(block).await.map_err(Into::into)?;
                }
                block = self.transport.next_unsafe_block() => {
                    let Some(block) = block else {
                        error!(target: "node::p2p", "The gossip transport has closed");
                        return Err(NetworkActorError::ChannelClosed);
                    };
                    if self.engine_client.send_unsafe_block(block.into()).await.is_err() {
                        warn!(target: "network", "Failed to forward unsafe block to engine");
                        return Err(NetworkActorError::ChannelClosed);
                    }
                }
                Some(NetworkAdminQuery::PostUnsafePayload { payload }) = self.admin_rpc.recv(), if !self.admin_rpc.is_closed() => {
                    debug!(target: "node::p2p", "Forwarding unsafe payload from admin api to engine");
                    if self.engine_client.send_unsafe_block(payload).await.is_err() {
                        warn!(target: "node::p2p", "Failed to forward admin api unsafe block to engine");
                    }
                }
                Some(req) = self.p2p_rpc.recv(), if !self.p2p_rpc.is_closed() => {
                    self.transport.handle_p2p_rpc(req);
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use alloy_primitives::B256;
    use alloy_rpc_types_engine::{ExecutionPayloadV1, ExecutionPayloadV3};
    use alloy_signer::SignerSync;
    use alloy_signer_local::PrivateKeySigner;
    use arbitrary::Arbitrary;
    use base_alloy_rpc_types_engine::OpExecutionPayload;
    use rand::Rng;

    use super::*;

    #[test]
    fn test_payload_signature_roundtrip_v1() {
        let mut bytes = [0u8; 4096];
        rand::rng().fill(bytes.as_mut_slice());

        let pubkey = PrivateKeySigner::random();
        let expected_address = pubkey.address();
        const CHAIN_ID: u64 = 1337;

        let block = OpExecutionPayloadEnvelope {
            execution_payload: OpExecutionPayload::V1(
                ExecutionPayloadV1::arbitrary(&mut arbitrary::Unstructured::new(&bytes)).unwrap(),
            ),
            parent_beacon_block_root: None,
        };

        let payload_hash = block.payload_hash();
        let signature = pubkey.sign_hash_sync(&payload_hash.signature_message(CHAIN_ID)).unwrap();
        let payload = base_alloy_rpc_types_engine::OpNetworkPayloadEnvelope {
            payload: block.execution_payload,
            parent_beacon_block_root: block.parent_beacon_block_root,
            signature,
            payload_hash,
        };
        let encoded_payload = payload.encode_v1().unwrap();

        let decoded_payload =
            base_alloy_rpc_types_engine::OpNetworkPayloadEnvelope::decode_v1(&encoded_payload)
                .unwrap();

        let msg = decoded_payload.payload_hash.signature_message(CHAIN_ID);
        let msg_signer = decoded_payload.signature.recover_address_from_prehash(&msg).unwrap();

        assert_eq!(expected_address, msg_signer);
    }

    #[test]
    fn test_payload_signature_roundtrip_v3() {
        let mut bytes = [0u8; 4096];
        rand::rng().fill(bytes.as_mut_slice());

        let pubkey = PrivateKeySigner::random();
        let expected_address = pubkey.address();
        const CHAIN_ID: u64 = 1337;

        let block = OpExecutionPayloadEnvelope {
            execution_payload: OpExecutionPayload::V3(
                ExecutionPayloadV3::arbitrary(&mut arbitrary::Unstructured::new(&bytes)).unwrap(),
            ),
            parent_beacon_block_root: Some(B256::random()),
        };

        let payload_hash = block.payload_hash();
        let signature = pubkey.sign_hash_sync(&payload_hash.signature_message(CHAIN_ID)).unwrap();
        let payload = base_alloy_rpc_types_engine::OpNetworkPayloadEnvelope {
            payload: block.execution_payload,
            parent_beacon_block_root: block.parent_beacon_block_root,
            signature,
            payload_hash,
        };
        let encoded_payload = payload.encode_v3().unwrap();

        let decoded_payload =
            base_alloy_rpc_types_engine::OpNetworkPayloadEnvelope::decode_v3(&encoded_payload)
                .unwrap();

        let msg = decoded_payload.payload_hash.signature_message(CHAIN_ID);
        let msg_signer = decoded_payload.signature.recover_address_from_prehash(&msg).unwrap();

        assert_eq!(expected_address, msg_signer);
    }
}
