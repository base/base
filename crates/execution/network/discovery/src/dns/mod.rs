//! EIP-1459 DNS tree discovery, validation, and lookup scheduling.

mod error;
pub use error::{DnsLookupError, DnsLookupResult, DnsParseEntryResult, ParseDnsEntryError};

mod resolver;
pub use resolver::{DnsLookup, DnsMapResolver, DnsResolver};

mod service;
pub use service::{
    DnsDiscoveryEvent, DnsDiscoveryHandle, DnsDiscoveryService, DnsNodeRecordUpdate,
};

mod config;
pub use config::DnsDiscoveryConfig;

mod tree;
pub use tree::{
    DnsBranchEntry, DnsEntry, DnsLinkEntry, DnsNodeEntry, DnsRecordText, DnsTreeRootEntry,
};

mod sync;
pub use sync::{DnsResolveKind, DnsSyncAction, DnsSyncTree};

mod query;
pub use query::{DnsQueryOutcome, DnsQueryPool, DnsResolveEntryResult, DnsResolveRootResult};
#[cfg(test)]
pub use resolver::DnsTimeoutResolver;
pub use resolver::{NetError, TokioResolver};
