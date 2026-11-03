//! Bootnodes for consensus network discovery.

use std::time::Duration;

use base_common_chains::ChainConfig;
use derive_more::Deref;

use crate::{BootNode, BootNodeEntry, BootNodeParseError, HostResolveError, HostResolver};

/// Error returned when no configured bootnode is usable.
#[derive(Debug, thiserror::Error)]
pub enum BootNodeResolveError {
    /// Every configured bootnode hostname failed to resolve and nothing else can seed discovery.
    #[error("none of the {0} configured bootnodes could be resolved")]
    AllFailed(usize),
}

/// Bootnodes for Base.
#[derive(Debug, Clone, Deref, PartialEq, Eq, Default, derive_more::From)]
pub struct BootNodes(pub Vec<BootNode>);

impl TryFrom<&ChainConfig> for BootNodes {
    type Error = BootNodeParseError;

    fn try_from(config: &ChainConfig) -> Result<Self, Self::Error> {
        config
            .bootnodes
            .consensus
            .iter()
            .map(|raw| BootNode::parse_bootnode(raw))
            .collect::<Result<Vec<_>, _>>()
            .map(Self)
    }
}

impl BootNodes {
    /// Default bound on a single bootnode hostname lookup.
    pub const DEFAULT_RESOLVE_TIMEOUT: Duration = Duration::from_secs(10);

    /// Resolves configured bootnode entries into [`BootNodes`].
    ///
    /// Hostnames are resolved concurrently, each bounded by `timeout`. A hostname that fails to
    /// resolve is skipped with a warning so the remaining bootnodes can still be used.
    ///
    /// `has_fallback` is `true` when something else can seed discovery (chain default bootnodes
    /// or ENR trees). Without a fallback, an error is returned when entries were configured but
    /// none of them is usable.
    pub async fn resolve<R: HostResolver + ?Sized>(
        entries: Vec<BootNodeEntry>,
        resolver: &R,
        timeout: Duration,
        has_fallback: bool,
    ) -> Result<Self, BootNodeResolveError> {
        let configured = entries.len();
        let mut nodes = Vec::with_capacity(configured);
        let mut hosts = Vec::new();
        for entry in entries {
            match entry {
                BootNodeEntry::Node(node) => nodes.push(node),
                BootNodeEntry::Host(url) => hosts.push(url),
            }
        }

        let lookups = hosts.into_iter().map(|url| async move {
            let result = tokio::time::timeout(timeout, url.resolve(resolver))
                .await
                .unwrap_or(Err(HostResolveError::Timeout(timeout)));
            (url, result)
        });

        for (url, result) in futures::future::join_all(lookups).await {
            let record = match result {
                Ok(record) => record,
                Err(error) => {
                    warn!(target: "p2p::bootnodes", host = %url.host, error = %error, "Skipping bootnode whose hostname did not resolve");
                    continue;
                }
            };
            match BootNode::from_unsigned(record) {
                Ok(node) => {
                    info!(target: "p2p::bootnodes", host = %url.host, address = %record.address, "Resolved bootnode hostname");
                    nodes.push(node);
                }
                Err(error) => {
                    warn!(target: "p2p::bootnodes", host = %url.host, error = %error, "Skipping bootnode with an invalid node id");
                }
            }
        }

        if configured > 0 && nodes.is_empty() && !has_fallback {
            return Err(BootNodeResolveError::AllFailed(configured));
        }
        Ok(Self(nodes))
    }

    /// Returns the bootnodes for the given chain id.
    ///
    /// If the chain id is not recognized, no bootnodes are returned.
    pub fn from_chain_id(id: u64) -> Self {
        ChainConfig::by_chain_id(id)
            .map(|c| Self::try_from(c).expect("hardcoded bootnode should parse"))
            .unwrap_or_default()
    }

    /// Returns the bootnodes for the mainnet.
    pub fn mainnet() -> Self {
        Self::try_from(ChainConfig::mainnet()).expect("hardcoded bootnode should parse")
    }

    /// Returns the bootnodes for the testnet.
    pub fn testnet() -> Self {
        Self::try_from(ChainConfig::sepolia()).expect("hardcoded bootnode should parse")
    }
}

#[cfg(test)]
mod tests {
    use std::{
        io,
        net::{IpAddr, Ipv4Addr},
        str::FromStr,
    };

    use base_common_chains::ChainConfig;

    use super::*;
    use crate::{EnodeUrl, MockHostResolver};

    const ID: &str = "2bd2e657bb3c8efffb8ff6db9071d9eb7be70d7c6d7d980ff80fc93b2629675c5f750bc0a5ef27cd788c2e491b8795a7e9a4a6e72178c14acc6753c0e5d77ae4";
    const RESOLVED_IP: IpAddr = IpAddr::V4(Ipv4Addr::new(203, 0, 113, 7));

    fn entry(host: &str) -> BootNodeEntry {
        BootNodeEntry::from_str(&format!("enode://{ID}@{host}:9222")).unwrap()
    }

    /// Resolves `good.example.org` and fails every other hostname.
    fn resolver() -> MockHostResolver {
        let mut resolver = MockHostResolver::new();
        resolver.expect_lookup_host().returning(|host| match host {
            "good.example.org" => Ok(vec![RESOLVED_IP]),
            _ => Err(io::Error::other("no such host")),
        });
        resolver
    }

    fn expected_node() -> BootNode {
        let url = EnodeUrl::from_str(&format!("enode://{ID}@good.example.org:9222")).unwrap();
        BootNode::from_unsigned(url.with_address(RESOLVED_IP)).unwrap()
    }

    #[tokio::test]
    async fn resolve_skips_failed_hostname_and_keeps_others() {
        let entries = vec![entry("missing.example.org"), entry("good.example.org")];

        let nodes =
            BootNodes::resolve(entries, &resolver(), BootNodes::DEFAULT_RESOLVE_TIMEOUT, false)
                .await
                .expect("one bootnode resolved, so startup must continue");

        assert_eq!(nodes, BootNodes(vec![expected_node()]));
    }

    #[tokio::test]
    async fn resolve_keeps_ip_bootnode_when_hostname_fails() {
        let ip_entry = entry("34.65.205.244");
        let BootNodeEntry::Node(ip_node) = ip_entry.clone() else { panic!("expected node") };
        let entries = vec![entry("missing.example.org"), ip_entry];

        let nodes =
            BootNodes::resolve(entries, &resolver(), BootNodes::DEFAULT_RESOLVE_TIMEOUT, false)
                .await
                .expect("the IP bootnode is usable, so startup must continue");

        assert_eq!(nodes, BootNodes(vec![ip_node]));
    }

    #[tokio::test]
    async fn resolve_continues_with_fallback_when_all_fail() {
        let entries = vec![entry("missing.example.org"), entry("gone.example.org")];

        let nodes =
            BootNodes::resolve(entries, &resolver(), BootNodes::DEFAULT_RESOLVE_TIMEOUT, true)
                .await
                .expect("chain defaults or ENR trees can still seed discovery");

        assert!(nodes.is_empty());
    }

    #[tokio::test]
    async fn resolve_fails_when_all_fail_without_fallback() {
        let entries = vec![entry("missing.example.org"), entry("gone.example.org")];

        let err =
            BootNodes::resolve(entries, &resolver(), BootNodes::DEFAULT_RESOLVE_TIMEOUT, false)
                .await
                .expect_err("nothing can seed discovery");

        assert!(matches!(err, BootNodeResolveError::AllFailed(2)));
    }

    /// A resolver whose lookups never finish. Hand-rolled because a `mockall` expectation returns
    /// a value, so an automock cannot model a lookup that hangs.
    struct HangingResolver;

    #[async_trait::async_trait]
    impl HostResolver for HangingResolver {
        async fn lookup_host(&self, _host: &str) -> io::Result<Vec<IpAddr>> {
            std::future::pending().await
        }
    }

    #[tokio::test(start_paused = true)]
    async fn resolve_treats_timeout_as_failure() {
        let entries = vec![entry("slow.example.org")];

        let err = BootNodes::resolve(entries, &HangingResolver, Duration::from_secs(1), false)
            .await
            .expect_err("a timed-out lookup is a failure");

        assert!(matches!(err, BootNodeResolveError::AllFailed(1)));
    }

    #[tokio::test]
    async fn resolve_without_entries_is_empty() {
        let mut resolver = MockHostResolver::new();
        resolver.expect_lookup_host().never();

        let nodes =
            BootNodes::resolve(vec![], &resolver, BootNodes::DEFAULT_RESOLVE_TIMEOUT, false)
                .await
                .unwrap();

        assert!(nodes.is_empty());
    }

    #[test]
    fn test_validate_bootnode_lens() {
        assert_eq!(ChainConfig::mainnet().bootnodes.execution.len(), 10);
        assert_eq!(ChainConfig::mainnet().bootnodes.consensus.len(), 5);
        assert_eq!(ChainConfig::sepolia().bootnodes.execution.len(), 4);
        assert_eq!(ChainConfig::sepolia().bootnodes.consensus.len(), 2);
        assert_eq!(ChainConfig::zeronet().bootnodes.execution.len(), 2);
        assert_eq!(ChainConfig::zeronet().bootnodes.consensus.len(), 1);
    }

    #[test]
    fn test_parse_raw_bootnodes() {
        for cfg in [ChainConfig::mainnet(), ChainConfig::sepolia(), ChainConfig::zeronet()] {
            for raw in cfg.bootnodes.execution.iter().chain(cfg.bootnodes.consensus.iter()) {
                BootNode::parse_bootnode(raw).expect("hardcoded bootnode should parse");
            }
        }
    }

    #[test]
    fn test_bootnodes_from_chain_id() {
        // `BootNodes::from_chain_id` returns CL bootnodes only.
        let mainnet = BootNodes::from_chain_id(ChainConfig::mainnet().chain_id);
        assert_eq!(mainnet.len(), 5);

        let testnet = BootNodes::from_chain_id(ChainConfig::sepolia().chain_id);
        assert_eq!(testnet.len(), 2);

        let zeronet = BootNodes::from_chain_id(ChainConfig::zeronet().chain_id);
        assert_eq!(zeronet.len(), 1);

        let unknown = BootNodes::from_chain_id(0);
        assert!(unknown.is_empty());
    }

    #[test]
    fn test_bootnodes_len() {
        // `BootNodes::mainnet`/`testnet` return CL bootnodes only.
        let bootnodes = BootNodes::mainnet();
        assert_eq!(bootnodes.len(), 5);

        let bootnodes = BootNodes::testnet();
        assert_eq!(bootnodes.len(), 2);
    }

    #[test]
    fn test_bootnodes_empty() {
        let bootnodes = BootNodes(vec![]);
        assert!(bootnodes.is_empty());

        let bootnodes = BootNodes::from_chain_id(ChainConfig::mainnet().chain_id);
        assert!(!bootnodes.is_empty());
    }
}
