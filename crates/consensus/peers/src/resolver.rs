//! DNS hostname resolution for bootnode addresses.

use std::{io, net::IpAddr, time::Duration};

use async_trait::async_trait;

/// Resolves DNS hostnames to IP addresses.
///
/// Abstracted so bootnode resolution can be tested without network access.
#[cfg_attr(test, mockall::automock)]
#[async_trait]
pub trait HostResolver: Send + Sync {
    /// Returns the addresses `host` resolves to, in the order the resolver returned them.
    async fn lookup_host(&self, host: &str) -> io::Result<Vec<IpAddr>>;
}

/// [`HostResolver`] backed by the operating system resolver (`getaddrinfo`).
#[derive(Debug, Clone, Copy, Default)]
pub struct SystemHostResolver;

#[async_trait]
impl HostResolver for SystemHostResolver {
    async fn lookup_host(&self, host: &str) -> io::Result<Vec<IpAddr>> {
        Ok(tokio::net::lookup_host((host, 0)).await?.map(|addr| addr.ip()).collect())
    }
}

/// Errors from resolving a bootnode hostname.
#[derive(Debug, thiserror::Error)]
pub enum HostResolveError {
    /// The resolver returned an error.
    #[error("lookup failed: {0}")]
    Lookup(#[source] io::Error),
    /// The resolver returned no addresses.
    #[error("lookup returned no addresses")]
    NoAddresses,
    /// The lookup did not finish in time.
    #[error("lookup timed out after {0:?}")]
    Timeout(Duration),
}
