#![doc = include_str!("../README.md")]

mod nat;
pub use nat::{NatResolver, ParseNatResolverError, ResolveNatInterval};

mod net_if;
pub use net_if::{DEFAULT_NET_IF_NAME, NetInterfaceError, NetworkInterface};

mod dns;
pub use dns::{
    DnsBranchEntry, DnsDiscoveryConfig, DnsDiscoveryEvent, DnsDiscoveryHandle, DnsDiscoveryService,
    DnsEntry, DnsLinkEntry, DnsLookup, DnsLookupError, DnsLookupResult, DnsMapResolver,
    DnsNodeEntry, DnsNodeRecordUpdate, DnsParseEntryResult, DnsQueryOutcome, DnsQueryPool,
    DnsRecordText, DnsResolveEntryResult, DnsResolveKind, DnsResolveRootResult, DnsResolver,
    DnsSyncAction, DnsSyncTree, DnsTreeRootEntry, NetError, ParseDnsEntryError, TokioResolver,
};

#[cfg(test)]
pub use dns::DnsTimeoutResolver;

mod discv4;
pub use discv4::Discv4Socket;
pub use discv4::{
    DEFAULT_DISCOVERY_ADDR as DISCV4_DEFAULT_DISCOVERY_ADDR,
    DEFAULT_DISCOVERY_ADDRESS as DISCV4_DEFAULT_DISCOVERY_ADDRESS,
    DEFAULT_DISCOVERY_PORT as DISCV4_DEFAULT_DISCOVERY_PORT,
    DecodePacketError as Discv4DecodePacketError, DiscoveryUpdate as Discv4DiscoveryUpdate, Discv4,
    Discv4Config, Discv4ConfigBuilder, Discv4Error, Discv4Event, Discv4Service,
    EnrRequest as Discv4EnrRequest, EnrResponse as Discv4EnrResponse, FindNode as Discv4FindNode,
    IngressEvent as Discv4IngressEvent, IngressHandler as Discv4IngressHandler,
    IngressReceiver as Discv4IngressReceiver, IngressSender as Discv4IngressSender,
    LookupContext as Discv4LookupContext, Message as Discv4Message, MessageId as Discv4MessageId,
    Neighbours as Discv4Neighbours, NodeEndpoint as Discv4NodeEndpoint, NodeKey as Discv4NodeKey,
    Packet as Discv4Packet, Ping as Discv4Ping, PingReason as Discv4PingReason, Pong as Discv4Pong,
    PongNodeKey as Discv4PongNodeKey, PongTable as Discv4PongTable,
};

#[cfg(any(test, feature = "test-utils"))]
pub use discv4::test_utils as discv4_test_utils;

mod discv5;
pub use discv5::{
    AdvertisedChainMetrics, BootNode, Config as Discv5Config, ConfigBuilder as Discv5ConfigBuilder,
    DEFAULT_COUNT_BOOTSTRAP_LOOKUPS, DEFAULT_DISCOVERY_V5_ADDR, DEFAULT_DISCOVERY_V5_ADDR_IPV6,
    DEFAULT_DISCOVERY_V5_LISTEN_CONFIG, DEFAULT_DISCOVERY_V5_PORT,
    DEFAULT_MIN_TARGET_KBUCKET_INDEX, DEFAULT_SECONDS_BOOTSTRAP_LOOKUP_INTERVAL,
    DEFAULT_SECONDS_LOOKUP_INTERVAL, DiscoveredPeer, DiscoveredPeersMetrics, Discv5, Discv5Metrics,
    EnrCombinedKeyWrapper, Error as Discv5Error, FilterOutcome, MAX_KBUCKET_INDEX, MustIncludeKey,
    MustNotIncludeKeys, NetworkStackId, amend_listen_config_wrt_rlpx, bootstrap, build_local_enr,
    discv4_id_to_discv5_id, discv4_id_to_multiaddr_id, discv5_sockets_wrt_rlpx_addr,
    enr_to_discv4_id, get_lookup_target, ipv4 as discv5_ipv4, ipv6 as discv5_ipv6, lookup,
    spawn_populate_kbuckets_bg,
};
