//! EIP-1459 ENR tree discovery.

use std::{
    pin::Pin,
    sync::Arc,
    task::{Context, Poll},
    time::Duration,
};

use alloy_rlp::Decodable;
use base_consensus_peers::EnrValidation;
use discv5::Enr;
use futures::{Stream, StreamExt, stream::SelectAll};
use reth_dns_discovery::{
    DnsDiscoveryConfig, DnsDiscoveryEvent, DnsDiscoveryService, DnsResolver, Resolver,
    tree::LinkEntry,
};
use tokio::time::{Instant, Interval, MissedTickBehavior};

/// Syncs one EIP-1459 ENR tree (`enrtree://<pubkey>@<domain>`) and yields the consensus-layer
/// ENRs it publishes.
///
/// Tree traversal, root signature checks, sequence-number updates and the periodic recheck
/// (every 30 minutes) are handled by reth's [`DnsDiscoveryService`]. That service drops a tree
/// whose first root lookup fails, and never walks a tree again while its root is unchanged. So
/// until the tree yields its first record, this stream restarts the sync with a fresh service on
/// every retry interval. That covers a root that failed to resolve and a root that resolved
/// while its records did not. Once a record has arrived, records that failed to resolve are
/// retried only after the tree's root changes, as in reth.
///
/// Only records that decode as valid ENRs and pass [`EnrValidation`] for the configured chain ID
/// are yielded. The stream never ends and owns no background task: it stops when it is dropped
/// (with the discovery driver task).
#[derive(derive_more::Debug)]
pub struct EnrTreeSync<R: Resolver = DnsResolver> {
    /// The tree being synced.
    pub link: LinkEntry,
    /// The chain ID that yielded ENRs must advertise.
    pub chain_id: u64,
    /// The DNS resolver, kept to restart the sync.
    #[debug(skip)]
    pub resolver: Arc<R>,
    /// The reth DNS discovery service syncing this tree.
    #[debug(skip)]
    pub service: DnsDiscoveryService<R>,
    /// Timer for restarting the sync until the tree has been reached.
    pub retry: Interval,
    /// Whether the tree has yielded any record, which means its root was resolved.
    pub reached: bool,
}

impl EnrTreeSync {
    /// Default interval between root lookups until a tree is first reached.
    pub const DEFAULT_RETRY_INTERVAL: Duration = Duration::from_secs(60);

    /// Creates one sync per tree, using the system DNS configuration, merged into one stream.
    ///
    /// If the system DNS configuration cannot be loaded, tree sync is disabled with a warning and
    /// an empty stream is returned: ENR trees are an optional discovery source.
    pub fn from_links(links: Vec<LinkEntry>, chain_id: u64) -> SelectAll<Self> {
        if links.is_empty() {
            return SelectAll::new();
        }
        let resolver = match DnsResolver::from_system_conf() {
            Ok(resolver) => Arc::new(resolver),
            Err(error) => {
                warn!(target: "discovery::enrtree", error = %error, "Failed to load the system DNS configuration; ENR tree sync is disabled");
                return SelectAll::new();
            }
        };
        futures::stream::select_all(links.into_iter().map(|link| {
            info!(target: "discovery::enrtree", tree = %link, "Syncing ENR tree");
            Self::new(link, chain_id, Arc::clone(&resolver), Self::DEFAULT_RETRY_INTERVAL)
        }))
    }
}

impl<R: Resolver> EnrTreeSync<R> {
    /// Starts syncing `link` with `resolver`.
    pub fn new(link: LinkEntry, chain_id: u64, resolver: Arc<R>, retry_interval: Duration) -> Self {
        let service = Self::start_service(&resolver, &link);
        let mut retry = tokio::time::interval_at(Instant::now() + retry_interval, retry_interval);
        retry.set_missed_tick_behavior(MissedTickBehavior::Delay);
        Self { link, chain_id, resolver, service, retry, reached: false }
    }

    /// Creates a DNS discovery service that starts syncing `link`.
    pub fn start_service(resolver: &Arc<R>, link: &LinkEntry) -> DnsDiscoveryService<R> {
        let mut service =
            DnsDiscoveryService::new(Arc::clone(resolver), DnsDiscoveryConfig::default());
        service.sync_tree_with_link(link.clone());
        service
    }
}

impl<R: Resolver> Stream for EnrTreeSync<R> {
    type Item = Enr;

    fn poll_next(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        let this = self.get_mut();

        if !this.reached {
            while this.retry.poll_tick(cx).is_ready() {
                debug!(target: "discovery::enrtree", tree = %this.link, "Retrying ENR tree sync");
                this.service = Self::start_service(&this.resolver, &this.link);
            }
        }

        while let Poll::Ready(event) = this.service.poll_next_unpin(cx) {
            let Some(DnsDiscoveryEvent::Enr(record)) = event else {
                return Poll::Ready(None);
            };
            this.reached = true;

            // Re-decoding as a discv5 record re-verifies the signature.
            let encoded = alloy_rlp::encode(&record);
            let enr = match Enr::decode(&mut encoded.as_slice()) {
                Ok(enr) => enr,
                Err(error) => {
                    debug!(target: "discovery::enrtree", tree = %this.link, error = %error, "Ignoring undecodable ENR tree record");
                    continue;
                }
            };
            let validation = EnrValidation::validate(&enr, this.chain_id);
            if validation.is_invalid() {
                debug!(target: "discovery::enrtree", tree = %this.link, validation = %validation, "Ignoring ENR tree record that is not a valid consensus-layer ENR");
                continue;
            }
            return Poll::Ready(Some(enr));
        }
        Poll::Pending
    }
}

#[cfg(test)]
mod tests {
    use std::net::Ipv4Addr;

    use alloy_primitives::keccak256;
    use alloy_rlp::Encodable;
    use base_consensus_peers::BaseEnr;
    use data_encoding::BASE32_NOPAD;
    use discv5::enr::CombinedKey;
    use reth_dns_discovery::{MapResolver, tree::TreeRootEntry};
    use secp256k1::{PublicKey, Secp256k1, SecretKey};

    use super::*;

    const CHAIN_ID: u64 = 8453;
    const DOMAIN: &str = "nodes.example.org";
    /// Collection window: long enough for every lookup in a small tree under paused time, and
    /// shorter than the retry interval.
    const COLLECT_WINDOW: Duration = Duration::from_secs(30);

    fn tree_key(byte: u8) -> SecretKey {
        SecretKey::from_slice(&[byte; 32]).expect("valid secret key")
    }

    fn link(key: &SecretKey) -> LinkEntry {
        LinkEntry {
            domain: DOMAIN.to_string(),
            pubkey: PublicKey::from_secret_key(&Secp256k1::new(), key),
        }
    }

    /// An ENR with an `opstack` entry for `chain_id`, or none when `chain_id` is `None`.
    fn enr(chain_id: Option<u64>) -> Enr {
        let mut builder = Enr::builder();
        builder.ip4(Ipv4Addr::new(203, 0, 113, 9)).udp4(9222).tcp4(9222);
        if let Some(chain_id) = chain_id {
            let mut data = Vec::new();
            BaseEnr::from_chain_id(chain_id).encode(&mut data);
            builder.add_value_rlp(BaseEnr::OPSTACK_ENR_KEY, data.into());
        }
        builder.build(&CombinedKey::generate_secp256k1()).expect("test enr should build")
    }

    fn entry_hash(text: &str) -> String {
        BASE32_NOPAD.encode(&keccak256(text.as_bytes())[..16])
    }

    /// Publishes `enrs` as an EIP-1459 tree at [`DOMAIN`], with the root signed by `signer`.
    fn publish(resolver: &MapResolver, signer: &SecretKey, enrs: &[Enr]) {
        let texts: Vec<String> = enrs.iter().map(Enr::to_base64).collect();
        for text in &texts {
            resolver.insert(format!("{}.{DOMAIN}", entry_hash(text)), text.clone());
        }
        let branch = format!(
            "enrtree-branch:{}",
            texts.iter().map(|text| entry_hash(text)).collect::<Vec<_>>().join(",")
        );
        resolver.insert(format!("{}.{DOMAIN}", entry_hash(&branch)), branch.clone());

        // An empty link subtree: its root hash is never published, so the lookup finds nothing.
        let mut root: TreeRootEntry = "enrtree-root:v1 e=QFT4PBCRX4XQCV3VUYJ6BTCEPU l=JGUFMSAGI7KZYB3P7IZW4S5Y3A seq=1 sig=3FmXuVwpa8Y7OstZTx9PIb1mt8FrW7VpDOFv4AaGCsZ2EIHmhraWhe4NxYhQDlw5MjeFXYMbJjsPeKlHzmJREQE"
            .parse()
            .expect("sample root parses");
        root.enr_root = entry_hash(&branch);
        root.sign(signer).expect("root signs");
        resolver.insert(DOMAIN.to_string(), root.to_string());
    }

    /// Collects everything the stream yields within `window`.
    async fn collect(sync: &mut EnrTreeSync<MapResolver>, window: Duration) -> Vec<Enr> {
        let deadline = Instant::now() + window;
        let mut out = Vec::new();
        while let Ok(Some(enr)) = tokio::time::timeout_at(deadline, sync.next()).await {
            out.push(enr);
        }
        out
    }

    #[tokio::test(start_paused = true)]
    async fn yields_only_valid_consensus_enrs() {
        let key = tree_key(0x11);
        let resolver = Arc::new(MapResolver::default());
        let valid = enr(Some(CHAIN_ID));
        publish(&resolver, &key, &[valid.clone(), enr(Some(1)), enr(None)]);

        let mut sync =
            EnrTreeSync::new(link(&key), CHAIN_ID, resolver, EnrTreeSync::DEFAULT_RETRY_INTERVAL);

        assert_eq!(collect(&mut sync, COLLECT_WINDOW).await, vec![valid]);
    }

    #[tokio::test(start_paused = true)]
    async fn retries_root_until_tree_is_reachable() {
        let key = tree_key(0x22);
        let resolver = Arc::new(MapResolver::default());
        let mut sync = EnrTreeSync::new(
            link(&key),
            CHAIN_ID,
            Arc::clone(&resolver),
            EnrTreeSync::DEFAULT_RETRY_INTERVAL,
        );

        // The root is not published yet, so the first lookup fails.
        assert!(collect(&mut sync, COLLECT_WINDOW).await.is_empty());

        let valid = enr(Some(CHAIN_ID));
        publish(&resolver, &key, std::slice::from_ref(&valid));

        let found = collect(&mut sync, EnrTreeSync::DEFAULT_RETRY_INTERVAL * 2).await;
        assert_eq!(found, vec![valid]);
    }

    #[tokio::test(start_paused = true)]
    async fn retries_records_when_root_resolved_without_them() {
        let key = tree_key(0x55);
        let resolver = Arc::new(MapResolver::default());
        let valid = enr(Some(CHAIN_ID));
        publish(&resolver, &key, std::slice::from_ref(&valid));
        let leaf = format!("{}.{DOMAIN}", entry_hash(&valid.to_base64()));
        let leaf_text = resolver.remove(&leaf).expect("leaf was published");

        let mut sync = EnrTreeSync::new(
            link(&key),
            CHAIN_ID,
            Arc::clone(&resolver),
            EnrTreeSync::DEFAULT_RETRY_INTERVAL,
        );

        // The root and branch resolve, but the record does not.
        assert!(collect(&mut sync, COLLECT_WINDOW).await.is_empty());

        resolver.insert(leaf, leaf_text);

        let found = collect(&mut sync, EnrTreeSync::DEFAULT_RETRY_INTERVAL * 2).await;
        assert_eq!(found, vec![valid]);
    }

    #[tokio::test(start_paused = true)]
    async fn ignores_tree_signed_by_another_key() {
        let resolver = Arc::new(MapResolver::default());
        publish(&resolver, &tree_key(0x33), &[enr(Some(CHAIN_ID))]);

        let mut sync = EnrTreeSync::new(
            link(&tree_key(0x44)),
            CHAIN_ID,
            resolver,
            EnrTreeSync::DEFAULT_RETRY_INTERVAL,
        );

        assert!(collect(&mut sync, COLLECT_WINDOW).await.is_empty());
    }
}
