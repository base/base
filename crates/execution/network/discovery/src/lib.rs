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
