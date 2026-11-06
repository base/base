use std::{
    collections::HashMap,
    io::{Cursor, Read, Write},
    net::Ipv4Addr,
    num::NonZeroUsize,
    path::PathBuf,
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    },
    time::Duration,
};

use alloy_eips::eip4844::{c_kzg, env_settings::EnvKzgSettings, kzg_to_versioned_hash};
use alloy_primitives::{B256, Bytes, FixedBytes, hex};
use base_consensus_peers::PeerUtils;
use discv5::{
    ConfigBuilder, Discv5, Enr, ListenConfig,
    enr::{CombinedKey, NodeId},
};
use futures::{AsyncReadExt, AsyncWriteExt, StreamExt};
use libp2p::{
    Multiaddr, PeerId, StreamProtocol, Swarm, SwarmBuilder, gossipsub, identify,
    identity::{Keypair, secp256k1},
    multiaddr::Protocol,
    noise,
    swarm::{
        NetworkBehaviour, SwarmEvent,
        dial_opts::{DialOpts, PeerCondition},
    },
    tcp, yamux,
};
use libp2p_stream::{Control, IncomingStreams, OpenStreamError};
use lru::LruCache;
use parking_lot::Mutex;
use sha2::{Digest, Sha256};
use ssz::{Decode, Encode};
use tokio::{sync::mpsc, time::Instant};
use tracing::{debug, info};

use crate::{
    BeaconNetwork, ColumnCustody, DataColumnSidecar, DataColumnsByRange, DataColumnsByRoot,
    LightClient, LightClientBootstrap, LightClientUpdate, LightClientUpdateKind,
};

/// Errors returned by [`BeaconPeers`].
#[derive(Debug, thiserror::Error)]
pub enum BeaconPeersError {
    /// Setting up discovery or the swarm failed.
    #[error("beacon p2p setup failed: {0}")]
    Setup(String),
    /// No peer finished the status handshake in time.
    #[error("no beacon peers")]
    NoPeers,
    /// A request did not complete in time.
    #[error("beacon request timed out")]
    Timeout,
    /// The peer does not speak the protocol.
    #[error(transparent)]
    OpenStream(#[from] OpenStreamError),
    /// Reading or writing the stream failed, or the response was malformed.
    #[error(transparent)]
    Io(#[from] std::io::Error),
    /// The peer answered with an error code.
    #[error("beacon peer returned error {code}: {message}")]
    Remote {
        /// Response code.
        code: u8,
        /// Error message sent by the peer.
        message: String,
    },
    /// The peer answered with no response chunks.
    #[error("beacon peer returned an empty response")]
    Empty,
    /// The response was tagged with a fork digest this network does not have.
    #[error("unknown fork digest {0}")]
    UnknownForkDigest(FixedBytes<4>),
    /// The response payload is not valid SSZ.
    #[error("invalid SSZ response: {0}")]
    Decode(String),
    /// The data columns do not rebuild the requested blobs.
    #[error("invalid data columns: {0}")]
    Columns(String),
    /// The block has no blob with this versioned hash.
    #[error("blob {0} not in block")]
    MissingBlob(B256),
}

/// One `ssz_snappy` response chunk: the context fork digest, if the protocol has one, and the
/// SSZ payload.
pub type ResponseChunk = (Option<FixedBytes<4>>, Vec<u8>);

/// Swarm behaviour of [`BeaconPeers`].
#[derive(NetworkBehaviour, derive_more::Debug)]
#[behaviour(to_swarm = "BeaconEvent")]
pub struct BeaconBehaviour {
    /// Req/resp streams.
    #[debug(skip)]
    pub stream: libp2p_stream::Behaviour,
    /// Identify, which consensus clients use to classify peers.
    #[debug(skip)]
    pub identify: identify::Behaviour,
    /// Gossipsub, subscribed to the light-client update topics.
    pub gossipsub: gossipsub::Behaviour,
}

/// Swarm event of [`BeaconBehaviour`].
#[derive(Debug)]
pub enum BeaconEvent {
    /// Identify exchanged info with a peer.
    Identify(Box<identify::Event>),
    /// A gossipsub event.
    Gossip(Box<gossipsub::Event>),
    /// A stream event, which nothing consumes.
    Ignored,
}

impl From<identify::Event> for BeaconEvent {
    fn from(event: identify::Event) -> Self {
        Self::Identify(Box::new(event))
    }
}

impl From<()> for BeaconEvent {
    fn from((): ()) -> Self {
        Self::Ignored
    }
}

impl From<gossipsub::Event> for BeaconEvent {
    fn from(event: gossipsub::Event) -> Self {
        Self::Gossip(Box::new(event))
    }
}

/// Fetches light-client data and blobs from beacon-chain peers over libp2p, without a beacon API.
///
/// Peers are found with discovery v5 and kept only once they pass the status handshake.
/// Requests rotate across them; the light client verifies every response, and blobs are checked
/// against their KZG commitments.
///
/// Metadata advertises the minimum custody of 4 column groups, which Lighthouse requires, and
/// [`ColumnCustody`] serves those columns.
///
/// Light-client updates also arrive over gossip and are handed to the sync loop.
///
/// ponytail: outbound-only. Our ENR carries no `eth2` field so peers do not dial us, and custody
/// columns are polled by req/resp instead of subscribing to the column subnets. Gossiped updates
/// are never forwarded, so we add nothing to the mesh but cannot be penalized for relaying one
/// the light client rejects; forwarding would need its verdict sent back to the swarm.
#[derive(Clone, derive_more::Debug)]
pub struct BeaconPeers {
    /// Network constants.
    pub network: Arc<BeaconNetwork>,
    /// Our custody columns.
    pub custody: ColumnCustody,
    /// Opens and accepts req/resp streams.
    #[debug(skip)]
    pub control: Control,
    /// Peers that passed the status handshake.
    pub ready: Arc<Mutex<Vec<PeerId>>>,
    /// Ready peers that custody every data column.
    pub supernodes: Arc<Mutex<Vec<PeerId>>>,
    /// Ready peers not yet seen failing a light-client request, other than lacking a finality
    /// update.
    ///
    /// ponytail: one failure drops a peer until it reconnects. Lighthouse before v8.4 answers
    /// every Gloas light-client request with an error and Prysm has no light-client protocols.
    pub light_client: Arc<Mutex<Vec<PeerId>>>,
    /// Round-robin cursor into a peer list.
    pub next: Arc<AtomicUsize>,
    /// Rebuilt blobs by versioned hash.
    #[debug(skip)]
    pub blob_cache: Arc<Mutex<LruCache<B256, Bytes>>>,
}

impl BeaconPeers {
    /// Peers kept for requests.
    pub const TARGET_PEERS: usize = 16;
    /// Light-client peers kept for updates and gossip.
    pub const TARGET_LIGHT_CLIENT: usize = 4;
    /// Supernodes kept for blob requests.
    pub const TARGET_SUPERNODES: usize = 4;
    /// Data columns per block, `NUMBER_OF_COLUMNS` in the Fulu spec.
    pub const NUMBER_OF_COLUMNS: u64 = 128;
    /// Blobs kept in memory, about 40 blocks of a busy batcher.
    pub const BLOB_CACHE_SIZE: NonZeroUsize = NonZeroUsize::new(256).unwrap();
    /// Peers tried per request.
    pub const ATTEMPTS: usize = 8;
    /// Timeout of a single request, including opening the stream.
    pub const REQUEST_TIMEOUT: Duration = Duration::from_secs(10);
    /// How long a request waits for a first peer.
    pub const PEER_WAIT: Duration = Duration::from_secs(10);
    /// Slots a peer's head may trail the clock and still serve the light client.
    pub const MAX_HEAD_LAG: u64 = 2 * LightClient::SLOTS_PER_EPOCH;
    /// Largest response read, `MAX_PAYLOAD_SIZE` in the p2p spec.
    pub const MAX_RESPONSE: usize = 10 * 1024 * 1024;

    const STATUS_V1: &str = "/eth2/beacon_chain/req/status/1/ssz_snappy";
    const STATUS_V2: &str = "/eth2/beacon_chain/req/status/2/ssz_snappy";
    const METADATA_V2: &str = "/eth2/beacon_chain/req/metadata/2/ssz_snappy";
    const METADATA_V3: &str = "/eth2/beacon_chain/req/metadata/3/ssz_snappy";
    const PING: &str = "/eth2/beacon_chain/req/ping/1/ssz_snappy";
    const GOODBYE: &str = "/eth2/beacon_chain/req/goodbye/1/ssz_snappy";
    const BOOTSTRAP: &str = "/eth2/beacon_chain/req/light_client_bootstrap/1/ssz_snappy";
    const UPDATES: &str = "/eth2/beacon_chain/req/light_client_updates_by_range/1/ssz_snappy";
    const FINALITY: &str = "/eth2/beacon_chain/req/light_client_finality_update/1/ssz_snappy";
    const OPTIMISTIC: &str = "/eth2/beacon_chain/req/light_client_optimistic_update/1/ssz_snappy";
    /// `data_column_sidecars_by_range` protocol.
    pub const COLUMNS_BY_RANGE: &str =
        "/eth2/beacon_chain/req/data_column_sidecars_by_range/1/ssz_snappy";
    /// `data_column_sidecars_by_root` protocol.
    pub const COLUMNS_BY_ROOT: &str =
        "/eth2/beacon_chain/req/data_column_sidecars_by_root/1/ssz_snappy";
    /// Gossip topic names of light-client updates.
    pub const GOSSIP_TOPICS: [(&str, LightClientUpdateKind); 2] = [
        ("light_client_finality_update", LightClientUpdateKind::Finality),
        ("light_client_optimistic_update", LightClientUpdateKind::Optimistic),
    ];
    /// Gossiped updates queued for the sync loop; newer ones are dropped while it is full.
    pub const GOSSIP_QUEUE: usize = 16;

    /// Starts discovery and the swarm for `network`, keeping the node key and custody columns in
    /// `dir`. Returns the peers and the light-client updates they gossip.
    ///
    /// ponytail: a random discovery port on every start.
    pub async fn spawn(
        network: BeaconNetwork,
        dir: PathBuf,
    ) -> Result<(Self, mpsc::Receiver<LightClientUpdate>), BeaconPeersError> {
        let setup = |error: &dyn std::fmt::Display| BeaconPeersError::Setup(error.to_string());
        let mut secret = reth_cli_util::get_secret_key(&dir.join("beacon-node.key"))
            .map_err(|error| setup(&error))?
            .secret_bytes();
        let keypair = Keypair::from(secp256k1::Keypair::from(
            secp256k1::SecretKey::try_from_bytes(secret).map_err(|error| setup(&error))?,
        ));
        let key = CombinedKey::secp256k1_from_bytes(&mut secret).map_err(|error| setup(&error))?;
        let enr = Enr::builder().build(&key).map_err(|error| setup(&error))?;
        let custody =
            ColumnCustody::open(dir.join("columns"), enr.node_id().raw(), network.current_slot())
                .map_err(|error| setup(&error))?;
        info!(target: "l1", columns = ?custody.columns, "custodying data columns");
        let listen = ListenConfig::Ipv4 { ip: Ipv4Addr::UNSPECIFIED, port: 0 };
        let mut discv5 = Discv5::new(enr, key, ConfigBuilder::new(listen).build())
            .map_err(|error| setup(&error))?;
        for bootnode in &network.bootnodes {
            let enr = bootnode.parse::<Enr>().map_err(|error| setup(&error))?;
            discv5.add_enr(enr).map_err(|error| setup(&error))?;
        }
        discv5.start().await.map_err(|error| setup(&error))?;

        let swarm = SwarmBuilder::with_existing_identity(keypair)
            .with_tokio()
            .with_tcp(
                tcp::Config::default().nodelay(true),
                noise::Config::new,
                yamux::Config::default,
            )
            .map_err(|error| setup(&error))?
            .with_behaviour(|key| {
                Ok(BeaconBehaviour {
                    stream: libp2p_stream::Behaviour::new(),
                    identify: identify::Behaviour::new(
                        identify::Config::new("eth2/1.0.0".into(), key.public())
                            .with_agent_version("base".into()),
                    ),
                    gossipsub: gossipsub::Behaviour::new(
                        gossipsub::MessageAuthenticity::Anonymous,
                        gossipsub::ConfigBuilder::default()
                            .validation_mode(gossipsub::ValidationMode::Anonymous)
                            .validate_messages()
                            .message_id_fn(Self::message_id)
                            .build()?,
                    )?,
                })
            })
            .map_err(|error| setup(&error))?
            .with_swarm_config(|config| {
                config.with_idle_connection_timeout(Duration::from_secs(300))
            })
            .build();

        let mut control = swarm.behaviour().stream.new_control();
        let peers = Self {
            network: Arc::new(network),
            custody,
            control: control.clone(),
            ready: Arc::default(),
            supernodes: Arc::default(),
            light_client: Arc::default(),
            next: Arc::default(),
            blob_cache: Arc::new(Mutex::new(LruCache::new(Self::BLOB_CACHE_SIZE))),
        };
        for protocol in [
            Self::STATUS_V1,
            Self::STATUS_V2,
            Self::METADATA_V2,
            Self::METADATA_V3,
            Self::PING,
            Self::GOODBYE,
            Self::COLUMNS_BY_RANGE,
            Self::COLUMNS_BY_ROOT,
        ] {
            let incoming =
                control.accept(StreamProtocol::new(protocol)).map_err(|error| setup(&error))?;
            tokio::spawn(peers.clone().serve(incoming, protocol));
        }
        let (dials, dial_rx) = mpsc::channel(64);
        let (updates, updates_rx) = mpsc::channel(Self::GOSSIP_QUEUE);
        tokio::spawn(peers.clone().drive(swarm, dial_rx, updates));
        tokio::spawn(peers.clone().discover(discv5, dials));
        info!(target: "l1", bootnodes = peers.network.bootnodes.len(), "beacon p2p started");
        Ok((peers, updates_rx))
    }

    /// Finds peers on the current fork digest until `TARGET_PEERS` are ready and
    /// `TARGET_LIGHT_CLIENT` serve the light client, then only supernodes until `TARGET_SUPERNODES`
    /// are.
    async fn discover(self, discv5: Discv5, dials: mpsc::Sender<Multiaddr>) {
        loop {
            let any = self.ready.lock().len() < Self::TARGET_PEERS
                || self.light_client.lock().len() < Self::TARGET_LIGHT_CLIENT;
            let supernodes = self.supernodes.lock().len() < Self::TARGET_SUPERNODES;
            if any || supernodes {
                let network = Arc::clone(&self.network);
                let predicate = Box::new(move |enr: &Enr| {
                    let digest = network.fork_digest(network.current_epoch());
                    enr.tcp4_socket().is_some()
                        && enr
                            .get_raw_rlp("eth2")
                            .is_some_and(|eth2| eth2.get(1..5) == Some(digest.as_slice()))
                        && (any
                            || enr.get_decodable::<u64>("cgc").and_then(Result::ok)
                                == Some(Self::NUMBER_OF_COLUMNS))
                });
                match discv5
                    .find_node_predicate(NodeId::random(), predicate, Self::TARGET_PEERS)
                    .await
                {
                    Ok(enrs) => {
                        for addr in enrs.iter().filter_map(PeerUtils::enr_to_multiaddr) {
                            if dials.send(addr).await.is_err() {
                                return;
                            }
                        }
                    }
                    Err(error) => {
                        debug!(target: "l1", error = ?error, "beacon discovery query failed")
                    }
                }
            }
            tokio::time::sleep(Duration::from_secs(5)).await;
        }
    }

    /// Drives the swarm: dials discovered peers, tracks disconnects, keeps the light-client topics
    /// of the current fork subscribed and sends their updates to `updates`.
    async fn drive(
        self,
        mut swarm: Swarm<BeaconBehaviour>,
        mut dials: mpsc::Receiver<Multiaddr>,
        updates: mpsc::Sender<LightClientUpdate>,
    ) {
        let mut subscribe =
            tokio::time::interval(Duration::from_secs(self.network.schedule.seconds_per_slot));
        loop {
            tokio::select! {
                _ = subscribe.tick() => {
                    let digest = hex::encode(self.network.fork_digest(self.network.current_epoch()));
                    for (name, _) in Self::GOSSIP_TOPICS {
                        let topic = gossipsub::IdentTopic::new(format!("/eth2/{digest}/{name}/ssz_snappy"));
                        if let Err(error) = swarm.behaviour_mut().gossipsub.subscribe(&topic) {
                            debug!(target: "l1", error = %error, topic = %topic, "gossip subscription failed");
                        }
                    }
                }
                Some(addr) = dials.recv() => {
                    let Some(Protocol::P2p(peer)) = addr.iter().last() else { continue };
                    let opts = DialOpts::peer_id(peer)
                        .addresses(vec![addr])
                        .condition(PeerCondition::DisconnectedAndNotDialing)
                        .build();
                    if let Err(error) = swarm.dial(opts) {
                        debug!(target: "l1", error = %error, peer = %peer, "beacon dial failed");
                    }
                }
                event = swarm.select_next_some() => match event {
                    SwarmEvent::ConnectionEstablished { peer_id, endpoint, num_established, .. }
                        if endpoint.is_dialer() && num_established.get() == 1 =>
                    {
                        tokio::spawn(self.clone().handshake(peer_id));
                    }
                    SwarmEvent::ConnectionClosed { peer_id, num_established: 0, .. } => {
                        self.forget(peer_id);
                    }
                    SwarmEvent::Behaviour(BeaconEvent::Identify(event)) => {
                        if let identify::Event::Received { peer_id, info, .. } = *event {
                            debug!(target: "l1", peer = %peer_id, agent = %info.agent_version, "beacon peer identified");
                        }
                    }
                    SwarmEvent::Behaviour(BeaconEvent::Gossip(event)) => {
                        if let gossipsub::Event::Message { propagation_source, message_id, message } = *event {
                            swarm.behaviour_mut().gossipsub.report_message_validation_result(
                                &message_id,
                                &propagation_source,
                                gossipsub::MessageAcceptance::Ignore,
                            );
                            match self.gossiped(&message) {
                                Ok(update) => {
                                    let _ = updates.try_send(update);
                                }
                                Err(error) => {
                                    debug!(target: "l1", error = %error, peer = %propagation_source, "ignored gossiped light-client update");
                                }
                            }
                        }
                    }
                    _ => {}
                },
            }
        }
    }

    /// Exchanges status with a newly dialed peer and keeps it if it is on our fork, for the light
    /// client too if its head is recent, and as a supernode too if its metadata says it custodies
    /// every column.
    async fn handshake(self, peer: PeerId) {
        let status = self.status(true);
        match self.request_from(peer, Self::STATUS_V2, Some(&status), false).await {
            Ok(chunks)
                if chunks.first().is_some_and(|(_, theirs)| theirs.get(..4) == status.get(..4)) =>
            {
                let head_slot = chunks[0].1.get(76..84).and_then(|slot| slot.try_into().ok());
                let synced = head_slot
                    .map(u64::from_le_bytes)
                    .is_some_and(|slot| slot + Self::MAX_HEAD_LAG >= self.network.current_slot());
                let mut ready = self.ready.lock();
                if !ready.contains(&peer) {
                    ready.push(peer);
                    if synced {
                        self.light_client.lock().push(peer);
                    }
                    debug!(target: "l1", peer = %peer, ready = ready.len(), synced, "beacon peer ready");
                }
            }
            Ok(_) => {
                debug!(target: "l1", peer = %peer, "beacon peer on another fork");
                return;
            }
            Err(error) => {
                debug!(target: "l1", error = %error, peer = %peer, "beacon status handshake failed");
                return;
            }
        }
        let custody = Self::NUMBER_OF_COLUMNS.to_le_bytes();
        if let Ok(chunks) = self.request_from(peer, Self::METADATA_V3, None, false).await
            && chunks.first().and_then(|(_, metadata)| metadata.get(17..25)) == Some(&custody[..])
        {
            let mut supernodes = self.supernodes.lock();
            if !supernodes.contains(&peer) {
                supernodes.push(peer);
                debug!(target: "l1", peer = %peer, supernodes = supernodes.len(), "beacon supernode ready");
            }
        }
    }

    /// Drops a disconnected peer from every peer list.
    fn forget(&self, peer: PeerId) {
        self.ready.lock().retain(|ready| *ready != peer);
        self.supernodes.lock().retain(|ready| *ready != peer);
        self.light_client.lock().retain(|ready| *ready != peer);
    }

    /// Answers inbound requests for `protocol`.
    async fn serve(self, mut incoming: IncomingStreams, protocol: &'static str) {
        while let Some((peer, mut stream)) = incoming.next().await {
            let peers = self.clone();
            tokio::spawn(async move {
                let served = tokio::time::timeout(Self::REQUEST_TIMEOUT, async {
                    let mut request = Vec::new();
                    (&mut stream).take(Self::MAX_RESPONSE as u64).read_to_end(&mut request).await?;
                    if !request.is_empty() {
                        request = Self::decode_payload(
                            &mut Cursor::new(&request[..]),
                            &mut { Self::MAX_RESPONSE },
                        )?;
                    }
                    if protocol == Self::GOODBYE {
                        peers.forget(peer);
                        let reason = u64::from_le_bytes(request.try_into().unwrap_or_default());
                        debug!(target: "l1", peer = %peer, reason, "beacon peer said goodbye");
                        return Ok(());
                    }
                    let chunks = peers.response(protocol, &request);
                    if protocol == Self::COLUMNS_BY_RANGE || protocol == Self::COLUMNS_BY_ROOT {
                        debug!(target: "l1", peer = %peer, protocol, sidecars = chunks.len(), "serving data columns");
                    }
                    for (digest, ssz) in chunks {
                        let mut chunk = vec![0];
                        if let Some(digest) = digest {
                            chunk.extend(digest);
                        }
                        chunk.extend(Self::encode_payload(&ssz));
                        stream.write_all(&chunk).await?;
                    }
                    stream.close().await
                })
                .await
                .unwrap_or_else(|_| Err(std::io::ErrorKind::TimedOut.into()));
                if let Err(error) = served {
                    debug!(target: "l1", error = %error, peer = %peer, protocol, "failed to answer beacon request");
                }
            });
        }
    }

    /// Returns our response chunks to an inbound `protocol` request.
    ///
    /// ponytail: a malformed column request gets an empty response instead of `InvalidRequest`,
    /// and stored columns are read with blocking file I/O, at most 512 small files per request.
    fn response(&self, protocol: &str, request: &[u8]) -> Vec<ResponseChunk> {
        let sidecars = match protocol {
            Self::STATUS_V1 => return vec![(None, self.status(false))],
            Self::STATUS_V2 => return vec![(None, self.status(true))],
            Self::METADATA_V2 => return vec![(None, Self::metadata(false))],
            Self::METADATA_V3 => return vec![(None, Self::metadata(true))],
            Self::PING => return vec![(None, 1u64.to_le_bytes().to_vec())],
            Self::COLUMNS_BY_RANGE => DataColumnsByRange::from_ssz_bytes(request)
                .map(|request| self.custody.by_range(&request)),
            Self::COLUMNS_BY_ROOT => Vec::<DataColumnsByRoot>::from_ssz_bytes(request)
                .map(|request| self.custody.by_root(&request)),
            _ => return Vec::new(),
        };
        sidecars
            .unwrap_or_default()
            .into_iter()
            .map(|(slot, ssz)| {
                (Some(self.network.fork_digest(slot / LightClient::SLOTS_PER_EPOCH)), ssz)
            })
            .collect()
    }

    /// Returns our SSZ `Status`: the current fork digest and a genesis checkpoint and head, which
    /// tells peers we have no blocks to sync from, and for v2 the first slot of our custody.
    fn status(&self, v2: bool) -> Vec<u8> {
        let mut status = self.network.fork_digest(self.network.current_epoch()).to_vec();
        status.resize(4 + 32 + 8 + 32 + 8, 0);
        if v2 {
            let cutoff = self.network.current_slot().saturating_sub(ColumnCustody::RETENTION_SLOTS);
            status.extend(self.custody.earliest_slot.max(cutoff).to_le_bytes());
        }
        status
    }

    /// Returns our SSZ `MetaData`: sequence 1, no subnets and, for v3, our custody group count.
    fn metadata(v3: bool) -> Vec<u8> {
        let mut metadata = 1u64.to_le_bytes().to_vec();
        metadata.extend([0; 8 + 1]);
        if v3 {
            metadata.extend(ColumnCustody::GROUPS.to_le_bytes());
        }
        metadata
    }

    /// Sends one request to `peer` and returns its response chunks.
    async fn request_from(
        &self,
        peer: PeerId,
        protocol: &'static str,
        request: Option<&[u8]>,
        context: bool,
    ) -> Result<Vec<ResponseChunk>, BeaconPeersError> {
        let mut control = self.control.clone();
        tokio::time::timeout(Self::REQUEST_TIMEOUT, async move {
            let mut stream = control.open_stream(peer, StreamProtocol::new(protocol)).await?;
            if let Some(ssz) = request {
                stream.write_all(&Self::encode_payload(ssz)).await?;
            }
            stream.close().await?;
            let mut response = Vec::new();
            stream.take(Self::MAX_RESPONSE as u64).read_to_end(&mut response).await?;
            Self::decode_chunks(&response, context)
        })
        .await
        .map_err(|_| BeaconPeersError::Timeout)?
    }

    /// Sends a request to `peers` in turn until one returns chunks that `decode` accepts.
    pub async fn request<T>(
        &self,
        peers: &Mutex<Vec<PeerId>>,
        protocol: &'static str,
        request: Option<&[u8]>,
        decode: impl Fn(Vec<ResponseChunk>) -> Result<T, BeaconPeersError>,
    ) -> Result<T, BeaconPeersError> {
        let mut error = BeaconPeersError::NoPeers;
        let mut tried = Vec::with_capacity(Self::ATTEMPTS);
        for _ in 0..Self::ATTEMPTS {
            let Some(peer) = self.next_peer(peers, &tried).await else { break };
            tried.push(peer);
            match self.request_from(peer, protocol, request, true).await {
                Ok(chunks) if chunks.is_empty() => error = BeaconPeersError::Empty,
                Ok(chunks) => match decode(chunks) {
                    Ok(value) => return Ok(value),
                    Err(decode_error) => error = decode_error,
                },
                Err(BeaconPeersError::OpenStream(OpenStreamError::UnsupportedProtocol(_))) => {
                    debug!(target: "l1", peer = %peer, protocol, "beacon peer does not serve protocol");
                    peers.lock().retain(|ready| *ready != peer);
                    continue;
                }
                Err(request_error) => error = request_error,
            }
            debug!(target: "l1", error = %error, peer = %peer, protocol, "beacon request failed");
            // Peers without a finality update may still serve optimistic and range updates.
            let drop_light_client =
                matches!(protocol, Self::BOOTSTRAP | Self::UPDATES | Self::OPTIMISTIC)
                    || (protocol == Self::FINALITY
                        && (matches!(error, BeaconPeersError::Decode(_))
                            || matches!(
                                &error,
                                BeaconPeersError::Remote { message, .. }
                                    if message.contains("GloasNotImplemented")
                            )));
            if drop_light_client {
                self.light_client.lock().retain(|ready| *ready != peer);
            }
        }
        Err(error)
    }

    /// Returns the next peer of `peers` not in `tried`, waiting up to `PEER_WAIT` for a first peer.
    async fn next_peer(&self, peers: &Mutex<Vec<PeerId>>, tried: &[PeerId]) -> Option<PeerId> {
        let deadline = Instant::now() + Self::PEER_WAIT;
        loop {
            {
                let ready = peers.lock();
                let start = self.next.fetch_add(1, Ordering::Relaxed);
                if let Some(peer) = (0..ready.len())
                    .map(|offset| ready[(start + offset) % ready.len()])
                    .find(|peer| !tried.contains(peer))
                {
                    return Some(peer);
                }
            }
            if !tried.is_empty() || Instant::now() >= deadline {
                return None;
            }
            tokio::time::sleep(Duration::from_millis(250)).await;
        }
    }

    /// Returns whether a chunk's fork digest selects the Gloas layout.
    fn is_gloas(&self, digest: Option<FixedBytes<4>>) -> Result<bool, BeaconPeersError> {
        let digest = digest.unwrap_or_default();
        self.network.is_gloas_digest(digest).ok_or(BeaconPeersError::UnknownForkDigest(digest))
    }

    /// Decodes a single update chunk of `kind`. Latest-update kinds signed more than
    /// `MAX_HEAD_LAG` slots ago are rejected, so the request moves on from peers serving stale
    /// updates.
    fn update(
        &self,
        (digest, ssz): ResponseChunk,
        kind: LightClientUpdateKind,
    ) -> Result<LightClientUpdate, BeaconPeersError> {
        let update = LightClientUpdate::from_ssz_bytes(&ssz, self.is_gloas(digest)?, kind)
            .map_err(|error| BeaconPeersError::Decode(format!("{error:?}")))?;
        if kind != LightClientUpdateKind::Full
            && update.signature_slot + Self::MAX_HEAD_LAG < self.network.current_slot()
        {
            return Err(BeaconPeersError::Decode(format!(
                "stale update signed at slot {}",
                update.signature_slot
            )));
        }
        Ok(update)
    }

    /// Decodes a message on one of the `GOSSIP_TOPICS`.
    pub fn gossiped(
        &self,
        message: &gossipsub::Message,
    ) -> Result<LightClientUpdate, BeaconPeersError> {
        let invalid = || BeaconPeersError::Decode("unexpected gossip message".into());
        let mut topic = message.topic.as_str().split('/').skip(2);
        let digest = topic.next().and_then(|digest| digest.parse().ok()).ok_or_else(invalid)?;
        let kind = topic
            .next()
            .and_then(|name| Self::GOSSIP_TOPICS.iter().find(|(topic, _)| *topic == name))
            .ok_or_else(invalid)?
            .1;
        let ssz = Self::decompress(&message.data).ok_or_else(invalid)?;
        self.update((Some(digest), ssz), kind)
    }

    /// Returns the Altair gossip message id: the first 20 bytes of the SHA-256 of a validity
    /// domain, the topic and the decompressed data.
    pub fn message_id(message: &gossipsub::Message) -> gossipsub::MessageId {
        let topic = message.topic.as_str().as_bytes();
        let (domain, data) = Self::decompress(&message.data)
            .map_or_else(|| ([0; 4], message.data.clone()), |data| ([1, 0, 0, 0], data));
        let hash = Sha256::new()
            .chain_update(domain)
            .chain_update((topic.len() as u64).to_le_bytes())
            .chain_update(topic)
            .chain_update(data)
            .finalize();
        gossipsub::MessageId::from(&hash[..20])
    }

    /// Decompresses a raw snappy block of at most `MAX_RESPONSE` bytes.
    fn decompress(data: &[u8]) -> Option<Vec<u8>> {
        snap::raw::decompress_len(data).ok().filter(|len| *len <= Self::MAX_RESPONSE)?;
        snap::raw::Decoder::new().decompress_vec(data).ok()
    }

    /// Fetches the blobs with versioned `hashes`, in order, from the block at `slot`.
    ///
    /// ponytail: asks supernodes for columns `0..64`, whose cells are the blobs themselves, so no
    /// erasure recovery is needed. Peers custodying 64 to 127 columns could serve too, through
    /// `recover_cells_and_kzg_proofs` on whichever 64 they hold.
    pub async fn blobs(&self, slot: u64, hashes: &[B256]) -> Result<Vec<Bytes>, BeaconPeersError> {
        let cached = {
            let mut cache = self.blob_cache.lock();
            hashes.iter().map(|hash| cache.get(hash).cloned()).collect::<Option<Vec<_>>>()
        };
        if let Some(blobs) = cached {
            return Ok(blobs);
        }
        let request = DataColumnsByRange {
            start_slot: slot,
            count: 1,
            columns: (0..Self::NUMBER_OF_COLUMNS / 2).collect(),
        }
        .as_ssz_bytes();
        let blobs = self
            .request(&self.supernodes, Self::COLUMNS_BY_RANGE, Some(&request), |chunks| {
                let columns = chunks
                    .into_iter()
                    .map(|(digest, ssz)| {
                        DataColumnSidecar::from_ssz_bytes(&ssz, self.is_gloas(digest)?)
                            .map_err(|error| BeaconPeersError::Decode(format!("{error:?}")))
                    })
                    .collect::<Result<_, _>>()?;
                Self::blobs_from_columns(columns, slot, hashes)
            })
            .await?;
        let mut cache = self.blob_cache.lock();
        for (hash, blob) in hashes.iter().zip(&blobs) {
            cache.put(*hash, blob.clone());
        }
        Ok(blobs)
    }

    /// Rebuilds the blobs with versioned `hashes`, in order, from columns `0..64` of the block at
    /// `slot`. Each blob is identified by the versioned hash of its recomputed KZG commitment;
    /// before Gloas the sidecars' commitments also skip unrequested blobs.
    pub fn blobs_from_columns(
        mut columns: Vec<DataColumnSidecar>,
        slot: u64,
        hashes: &[B256],
    ) -> Result<Vec<Bytes>, BeaconPeersError> {
        let half = Self::NUMBER_OF_COLUMNS as usize / 2;
        columns.sort_by_key(|column| column.index);
        columns.dedup_by_key(|column| column.index);
        let complete = columns.len() >= half
            && columns[..half].iter().enumerate().all(|(index, column)| {
                column.index == index as u64
                    && column.slot == slot
                    && column.cells.len() == columns[0].cells.len()
                    && column.kzg_commitments == columns[0].kzg_commitments
            });
        if !complete {
            return Err(BeaconPeersError::Columns(format!(
                "expected columns 0..{half} of slot {slot}"
            )));
        }
        let commitments = &columns[0].kzg_commitments;
        let cell = DataColumnSidecar::CELL_LEN;
        let kzg_error = |error: c_kzg::Error| BeaconPeersError::Columns(error.to_string());
        let mut found = HashMap::new();
        for index in 0..columns[0].blob_count() {
            if found.len() == hashes.len() {
                break;
            }
            if commitments.get(index).is_some_and(|commitment| {
                !hashes.contains(&kzg_to_versioned_hash(commitment.as_slice()))
            }) {
                continue;
            }
            let bytes: Vec<u8> = columns[..half]
                .iter()
                .flat_map(|column| &column.cells[index * cell..(index + 1) * cell])
                .copied()
                .collect();
            let commitment = EnvKzgSettings::Default
                .get()
                .blob_to_kzg_commitment(&c_kzg::Blob::from_bytes(&bytes).map_err(kzg_error)?)
                .map_err(kzg_error)?;
            if commitments
                .get(index)
                .is_some_and(|expected| expected.as_slice() != commitment.as_slice())
            {
                return Err(BeaconPeersError::Columns(format!(
                    "blob {index} of slot {slot} does not match its commitment"
                )));
            }
            let hash = kzg_to_versioned_hash(commitment.as_slice());
            if hashes.contains(&hash) {
                found.insert(hash, Bytes::from(bytes));
            }
        }
        hashes
            .iter()
            .map(|hash| found.get(hash).cloned().ok_or(BeaconPeersError::MissingBlob(*hash)))
            .collect()
    }

    /// Encodes `ssz` as a length-prefixed snappy frame stream.
    pub fn encode_payload(ssz: &[u8]) -> Vec<u8> {
        let mut length = unsigned_varint::encode::usize_buffer();
        let mut encoder = snap::write::FrameEncoder::new(
            unsigned_varint::encode::usize(ssz.len(), &mut length).to_vec(),
        );
        encoder.write_all(ssz).expect("writing to a Vec cannot fail");
        encoder.into_inner().expect("flushing to a Vec cannot fail")
    }

    /// Splits a response into chunks, failing on the first error chunk.
    pub fn decode_chunks(
        bytes: &[u8],
        context: bool,
    ) -> Result<Vec<ResponseChunk>, BeaconPeersError> {
        let mut cursor = Cursor::new(bytes);
        let mut budget = Self::MAX_RESPONSE;
        let mut chunks = Vec::new();
        while (cursor.position() as usize) < bytes.len() {
            let mut code = [0];
            cursor.read_exact(&mut code)?;
            let digest = if context && code[0] == 0 {
                let mut digest = FixedBytes::<4>::ZERO;
                cursor.read_exact(digest.as_mut_slice())?;
                Some(digest)
            } else {
                None
            };
            let payload = Self::decode_payload(&mut cursor, &mut budget)?;
            if code[0] != 0 {
                let message = String::from_utf8_lossy(&payload).into_owned();
                return Err(BeaconPeersError::Remote { code: code[0], message });
            }
            chunks.push((digest, payload));
        }
        Ok(chunks)
    }

    /// Reads one length-prefixed snappy frame stream of at most `budget` uncompressed bytes,
    /// leaving `cursor` after its last frame.
    fn decode_payload(cursor: &mut Cursor<&[u8]>, budget: &mut usize) -> std::io::Result<Vec<u8>> {
        let invalid = |message| std::io::Error::new(std::io::ErrorKind::InvalidData, message);
        let rest = &cursor.get_ref()[cursor.position() as usize..];
        let (len, tail) =
            unsigned_varint::decode::usize(rest).map_err(|_| invalid("invalid length prefix"))?;
        *budget = budget.checked_sub(len).ok_or_else(|| invalid("payload too large"))?;
        cursor.set_position(cursor.position() + (rest.len() - tail.len()) as u64);
        let mut payload = vec![0; len];
        snap::read::FrameDecoder::new(&mut *cursor).read_exact(&mut payload)?;
        Ok(payload)
    }

    /// Fetches the bootstrap for the beacon block `root`.
    pub async fn bootstrap(&self, root: B256) -> Result<LightClientBootstrap, BeaconPeersError> {
        self.request(&self.light_client, Self::BOOTSTRAP, Some(root.as_slice()), |mut chunks| {
            let (digest, ssz) = chunks.swap_remove(0);
            LightClientBootstrap::from_ssz_bytes(&ssz, self.is_gloas(digest)?)
                .map_err(|error| BeaconPeersError::Decode(format!("{error:?}")))
        })
        .await
    }

    /// Fetches up to `count` sync committee updates starting at `start_period`.
    pub async fn updates(
        &self,
        start_period: u64,
        count: u64,
    ) -> Result<Vec<LightClientUpdate>, BeaconPeersError> {
        let mut request = start_period.to_le_bytes().to_vec();
        request.extend(count.to_le_bytes());
        self.request(&self.light_client, Self::UPDATES, Some(&request), |chunks| {
            let updates: Vec<LightClientUpdate> = chunks
                .into_iter()
                .map(|chunk| self.update(chunk, LightClientUpdateKind::Full))
                .collect::<Result<_, _>>()?;
            if updates.iter().zip(start_period..).any(|(update, period)| {
                LightClient::period(update.attested_header.beacon.slot) != period
            }) {
                return Err(BeaconPeersError::Decode(format!(
                    "updates do not run consecutively from period {start_period}"
                )));
            }
            Ok(updates)
        })
        .await
    }

    /// Fetches the latest finality update.
    pub async fn finality_update(&self) -> Result<LightClientUpdate, BeaconPeersError> {
        self.request(&self.light_client, Self::FINALITY, None, |mut chunks| {
            self.update(chunks.swap_remove(0), LightClientUpdateKind::Finality)
        })
        .await
    }

    /// Fetches the latest optimistic update.
    pub async fn optimistic_update(&self) -> Result<LightClientUpdate, BeaconPeersError> {
        self.request(&self.light_client, Self::OPTIMISTIC, None, |mut chunks| {
            self.update(chunks.swap_remove(0), LightClientUpdateKind::Optimistic)
        })
        .await
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn response_chunks_round_trip() {
        let digest = FixedBytes::<4>::repeat_byte(7);
        let first = vec![1u8; 70_000];
        let second = b"second".to_vec();
        let mut response = vec![0];
        response.extend(digest);
        response.extend(BeaconPeers::encode_payload(&first));
        response.push(0);
        response.extend(digest);
        response.extend(BeaconPeers::encode_payload(&second));

        let chunks = BeaconPeers::decode_chunks(&response, true).unwrap();
        assert_eq!(chunks, vec![(Some(digest), first), (Some(digest), second)]);

        let mut error = vec![3];
        error.extend(BeaconPeers::encode_payload(b"unavailable"));
        let error = BeaconPeers::decode_chunks(&error, true).unwrap_err();
        assert!(
            matches!(error, BeaconPeersError::Remote { code: 3, message } if message == "unavailable")
        );

        let half = BeaconPeers::encode_payload(&vec![0u8; BeaconPeers::MAX_RESPONSE / 2 + 1]);
        let mut oversized = vec![0];
        oversized.extend(&half);
        oversized.push(0);
        oversized.extend(&half);
        assert!(matches!(
            BeaconPeers::decode_chunks(&oversized, false),
            Err(BeaconPeersError::Io(_))
        ));
    }

    /// SSZ of a sidecar with zero proofs and a zero header apart from `slot`: Fulu with
    /// `commitments`, Gloas without.
    fn sidecar(index: u64, cells: &[u8], commitments: Option<&[u8]>, slot: u64) -> Vec<u8> {
        let proofs = vec![0; cells.len() / DataColumnSidecar::CELL_LEN * 48];
        let mut ssz = index.to_le_bytes().to_vec();
        match commitments {
            Some(commitments) => {
                let fixed = 8 + 3 * 4 + 208 + 128;
                for offset in [fixed, fixed + cells.len(), fixed + cells.len() + commitments.len()]
                {
                    ssz.extend((offset as u32).to_le_bytes());
                }
                ssz.extend(slot.to_le_bytes());
                ssz.extend([0; 200 + 128]);
                ssz.extend(cells);
                ssz.extend(commitments);
                ssz.extend(proofs);
            }
            None => {
                let fixed = 8 + 2 * 4 + 8 + 32;
                for offset in [fixed, fixed + cells.len()] {
                    ssz.extend((offset as u32).to_le_bytes());
                }
                ssz.extend(slot.to_le_bytes());
                ssz.extend([0; 32]);
                ssz.extend(cells);
                ssz.extend(proofs);
            }
        }
        ssz
    }

    #[test]
    fn blobs_rebuild_from_first_half_of_columns() {
        let blob: Vec<u8> =
            (0..131_072).map(|i| if i % 32 == 0 { 0 } else { (i * 7 % 251) as u8 }).collect();
        let kzg = EnvKzgSettings::Default.get();
        let kzg_blob = c_kzg::Blob::from_bytes(&blob).unwrap();
        let commitment = kzg.blob_to_kzg_commitment(&kzg_blob).unwrap();
        let cells = kzg.compute_cells(&kzg_blob).unwrap();
        let hash = kzg_to_versioned_hash(commitment.as_slice());

        for gloas in [false, true] {
            let commitments = (!gloas).then_some(commitment.as_slice());
            let mut ssz: Vec<Vec<u8>> = (0..64)
                .rev()
                .map(|i| sidecar(i, &cells[i as usize].to_bytes(), commitments, 9))
                .collect();
            let rebuild = |ssz: &[Vec<u8>], slot, hash| {
                let columns = ssz
                    .iter()
                    .map(|ssz| DataColumnSidecar::from_ssz_bytes(ssz, gloas).unwrap())
                    .collect();
                BeaconPeers::blobs_from_columns(columns, slot, &[hash])
            };

            assert_eq!(rebuild(&ssz, 9, hash).unwrap(), vec![Bytes::from(blob.clone())]);
            assert!(matches!(rebuild(&ssz, 10, hash), Err(BeaconPeersError::Columns(_))));
            assert!(matches!(rebuild(&ssz[1..], 9, hash), Err(BeaconPeersError::Columns(_))));
            assert!(matches!(rebuild(&ssz, 9, B256::ZERO), Err(BeaconPeersError::MissingBlob(_))));

            let cells_start = if gloas { 8 + 2 * 4 + 8 + 32 } else { 8 + 3 * 4 + 208 + 128 };
            ssz[0][cells_start + 1] ^= 1;
            let tampered = rebuild(&ssz, 9, hash).unwrap_err();
            if gloas {
                assert!(matches!(tampered, BeaconPeersError::MissingBlob(_)));
            } else {
                assert!(matches!(tampered, BeaconPeersError::Columns(_)));
            }
        }
    }
}
