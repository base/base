#![doc = include_str!("../README.md")]
#![doc(
    html_logo_url = "https://avatars.githubusercontent.com/u/16627100?s=200&v=4",
    html_favicon_url = "https://avatars.githubusercontent.com/u/16627100?s=200&v=4",
    issue_tracker_base_url = "https://github.com/base/base/issues/"
)]
#![cfg_attr(not(test), warn(unused_crate_dependencies))]
#![cfg_attr(docsrs, feature(doc_cfg, doc_auto_cfg))]

#[macro_use]
extern crate tracing;

mod id;
pub use id::PeerId;

mod nodes;
pub use nodes::BootNodes;

mod store;
pub use store::{BootStore, BootStoreFile};

mod score;
pub use score::PeerScoreLevel;

mod enr;
pub use enr::{BaseEnr, BaseEnrError, EnrValidation};

mod any;
pub use any::{AnyNode, DialOptsError};

mod boot;
pub use boot::{BootNode, BootNodeParseError};

mod record;
pub use record::{NodeRecord, NodeRecordParseError};

mod utils;
pub use utils::{PeerIdConversionError, PeerUtils};

mod monitoring;
pub use monitoring::PeerMonitoring;

mod secrets;
pub use secrets::{KeypairError, ParseKeyError, SecretKeyLoader};

mod discovery;
pub use discovery::{
    Discv5Builder, Discv5BuilderError, Discv5Driver, Discv5Handler, HandlerRequest, LocalNode,
    Metrics as DiscoveryMetrics,
};

mod gossip;
pub use gossip::{
    Behaviour, BehaviourError, BlockHandler, BlockInvalidError, Connectedness, ConnectionError,
    ConnectionGate, ConnectionGater, ConnectionLimitsConfig, DEFAULT_MAX_ESTABLISHED_CONNECTIONS,
    DEFAULT_MAX_ESTABLISHED_CONNECTIONS_PER_PEER, DEFAULT_MAX_IDENTIFY_PEERSTORE_PEERS,
    DEFAULT_MAX_PENDING_INCOMING_CONNECTIONS, DEFAULT_MAX_PENDING_OUTGOING_CONNECTIONS,
    DEFAULT_MESH_D, DEFAULT_MESH_DHI, DEFAULT_MESH_DLAZY, DEFAULT_MESH_DLO,
    DEFAULT_PENDING_DIAL_TIMEOUT, DialInfo, Direction, DnsResolutionFailure, Event,
    GATER_PRUNE_INTERVAL, GLOBAL_VALIDATE_THROTTLE, GOSSIP_HEARTBEAT, GaterConfig, GossipDriver,
    GossipDriverBuilder, GossipDriverBuilderError, GossipDriverConfig, GossipScores, Handler,
    HandlerEncodeError, MAX_GOSSIP_SIZE, MAX_OUTBOUND_QUEUE, MAX_VALIDATE_QUEUE, MIN_GOSSIP_SIZE,
    Metrics as GossipMetrics, P2pRpcRequest, PEER_SCORE_INSPECT_FREQUENCY,
    PENDING_DIAL_PRUNE_INTERVAL, PeerCount, PeerDump, PeerInfo, PeerScores, PeerStats,
    PublishError, ReqRespScores, SEEN_MESSAGES_TTL, TopicScores, default_config,
    default_config_builder,
};
