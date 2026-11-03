//! EIP-1459 ENR tree discovery.

use std::{
    collections::VecDeque,
    future::Future,
    pin::Pin,
    sync::Arc,
    task::{Context, Poll, ready},
    time::Duration,
};

use alloy_primitives::keccak256;
use alloy_rlp::Decodable;
use base_consensus_peers::EnrValidation;
use data_encoding::BASE32_NOPAD;
use discv5::Enr;
use futures::{Stream, future::BoxFuture};
use reth_dns_discovery::{
    DnsResolver, Resolver,
    tree::{DnsEntry, LinkEntry, TreeRootEntry},
};
use secp256k1::SecretKey;
use tokio::time::{Instant, Sleep};

/// Syncs one EIP-1459 ENR tree (`enrtree://<pubkey>@<domain>`) and yields the consensus-layer
/// ENRs it publishes.
///
/// The tree is walked with reth's tree types: the root must carry a valid signature from the
/// link's key, and every entry must match the hash it is published under. Walks are bounded:
///
/// - Links to other trees are not followed; only the configured tree is walked.
/// - One lookup is in flight at a time, at most one every [`EnrTreeSync::LOOKUP_INTERVAL`], each
///   bounded by [`EnrTreeSync::LOOKUP_TIMEOUT`].
/// - A walk makes at most `max_lookups` entry lookups and yields at most `max_records` records;
///   the queue of pending entries never exceeds `max_lookups`. A walk that hits a budget stops
///   with a warning.
/// - A failed entry lookup is retried in place, up to [`EnrTreeSync::MAX_ENTRY_ATTEMPTS`] times,
///   without discarding the rest of the walk.
///
/// After a complete walk the root is rechecked every [`EnrTreeSync::RECHECK_INTERVAL`], and the
/// tree is walked again only when its ENR subtree hash changes. A root that fails to resolve or
/// verify, a walk with failed entries, and a walk without valid records are retried with a
/// warning after a backoff that starts at the retry interval and doubles up to
/// [`EnrTreeSync::MAX_RETRY_INTERVAL`].
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
    /// The DNS resolver.
    #[debug(skip)]
    pub resolver: Arc<R>,
    /// Maximum entry lookups in one walk.
    pub max_lookups: usize,
    /// Maximum records yielded by one walk.
    pub max_records: usize,
    /// First delay before retrying a failed or incomplete sync.
    pub retry_interval: Duration,
    /// Delay before the next retry; doubles after each failure up to the maximum.
    pub backoff: Duration,
    /// The ENR subtree hash of the last complete walk. A recheck that finds the same hash does
    /// not walk the tree again.
    pub synced_root: Option<String>,
    /// What the sync is doing now.
    pub state: EnrTreeState,
    /// Fires when the next root lookup is due.
    #[debug(skip)]
    pub timer: Pin<Box<Sleep>>,
    /// Paces entry lookups.
    #[debug(skip)]
    pub pacer: Pin<Box<Sleep>>,
}

/// The phase of an [`EnrTreeSync`].
#[derive(derive_more::Debug)]
pub enum EnrTreeState {
    /// Waiting for the timer before looking up the root.
    Idle,
    /// Looking up the root.
    Root(#[debug(skip)] BoxFuture<'static, Option<String>>),
    /// Walking the ENR subtree under a verified root.
    Walk(EnrTreeWalk),
}

/// One bounded walk of the ENR subtree under a verified root.
#[derive(derive_more::Debug)]
pub struct EnrTreeWalk {
    /// The verified root.
    pub root: TreeRootEntry,
    /// Entry hashes still to look up, each with its number of failed attempts.
    pub pending: VecDeque<(String, u8)>,
    /// The lookup in flight, if any.
    pub lookup: Option<EnrTreeLookup>,
    /// Entry lookups started so far.
    pub lookups: usize,
    /// Valid records yielded so far.
    pub records: usize,
    /// Entries given up on after failed lookups.
    pub failed: usize,
    /// Whether entries were dropped because the walk hit a budget.
    pub truncated: bool,
}

/// An entry lookup in flight.
#[derive(derive_more::Debug)]
pub struct EnrTreeLookup {
    /// The hash of the entry being looked up.
    pub hash: String,
    /// Earlier failed attempts for this entry.
    pub attempts: u8,
    /// The lookup.
    #[debug(skip)]
    pub lookup: BoxFuture<'static, Option<String>>,
}

impl EnrTreeSync {
    /// Default first delay before retrying a failed or incomplete sync.
    pub const DEFAULT_RETRY_INTERVAL: Duration = Duration::from_secs(60);
    /// Longest delay between retries.
    pub const MAX_RETRY_INTERVAL: Duration = Duration::from_secs(30 * 60);
    /// Interval between root rechecks after a complete walk.
    pub const RECHECK_INTERVAL: Duration = Duration::from_secs(30 * 60);
    /// Minimum interval between DNS lookups (three per second).
    pub const LOOKUP_INTERVAL: Duration = Duration::from_millis(333);
    /// Timeout of one DNS lookup.
    pub const LOOKUP_TIMEOUT: Duration = Duration::from_secs(5);
    /// Default maximum entry lookups in one walk.
    pub const MAX_LOOKUPS: usize = 2_000;
    /// Default maximum records yielded by one walk.
    pub const MAX_RECORDS: usize = 500;
    /// Lookups of one entry before it is given up on for the current walk.
    pub const MAX_ENTRY_ATTEMPTS: u8 = 3;
    /// Shortest accepted entry hash label, in decoded bytes.
    pub const MIN_HASH_BYTES: usize = 12;
    /// Longest accepted entry hash label, in decoded bytes.
    pub const MAX_HASH_BYTES: usize = 32;

    /// Creates one sync per tree, sharing a resolver that uses the system DNS configuration.
    ///
    /// If the system DNS configuration cannot be loaded, tree sync is disabled with a warning and
    /// no syncs are returned: ENR trees are an optional discovery source.
    pub fn from_links(links: Vec<LinkEntry>, chain_id: u64) -> Vec<Self> {
        if links.is_empty() {
            return Vec::new();
        }
        let resolver = match DnsResolver::from_system_conf() {
            Ok(resolver) => Arc::new(resolver),
            Err(error) => {
                warn!(target: "discovery::enrtree", error = %error, "Failed to load the system DNS configuration; ENR tree sync is disabled");
                return Vec::new();
            }
        };
        links
            .into_iter()
            .map(|link| {
                info!(target: "discovery::enrtree", tree = %link, "Syncing ENR tree");
                Self::new(link, chain_id, Arc::clone(&resolver), Self::DEFAULT_RETRY_INTERVAL)
            })
            .collect()
    }
}

impl<R: Resolver> EnrTreeSync<R> {
    /// Starts syncing `link` with `resolver`. The first root lookup happens on the first poll.
    pub fn new(link: LinkEntry, chain_id: u64, resolver: Arc<R>, retry_interval: Duration) -> Self {
        Self {
            link,
            chain_id,
            resolver,
            max_lookups: EnrTreeSync::MAX_LOOKUPS,
            max_records: EnrTreeSync::MAX_RECORDS,
            retry_interval,
            backoff: retry_interval,
            synced_root: None,
            state: EnrTreeState::Idle,
            timer: Box::pin(tokio::time::sleep(Duration::ZERO)),
            pacer: Box::pin(tokio::time::sleep(Duration::ZERO)),
        }
    }

    /// Looks up the TXT record at `name`, bounded by [`EnrTreeSync::LOOKUP_TIMEOUT`].
    pub fn lookup(resolver: &Arc<R>, name: String) -> BoxFuture<'static, Option<String>> {
        let resolver = Arc::clone(resolver);
        Box::pin(async move {
            tokio::time::timeout(EnrTreeSync::LOOKUP_TIMEOUT, resolver.lookup_txt(&name))
                .await
                .ok()
                .flatten()
        })
    }

    /// Parses a root TXT record and checks its signature against the link's key.
    pub fn verify_root(&self, txt: Option<String>) -> Result<TreeRootEntry, &'static str> {
        let txt = txt.ok_or("root lookup failed")?;
        let root = txt.parse::<TreeRootEntry>().map_err(|_| "root does not parse")?;
        if !root.verify::<SecretKey>(&self.link.pubkey) {
            return Err("root signature does not match the tree's key");
        }
        Ok(root)
    }

    /// Waits `delay` before the next root lookup.
    pub fn schedule(&mut self, delay: Duration) {
        self.state = EnrTreeState::Idle;
        self.timer.as_mut().reset(Instant::now() + delay);
    }

    /// Schedules a retry after the current backoff, then doubles the backoff.
    pub fn retry_later(&mut self, reason: &'static str) {
        warn!(target: "discovery::enrtree", tree = %self.link, reason, retry_in = ?self.backoff, "ENR tree sync incomplete, retrying later");
        self.schedule(self.backoff);
        self.backoff = (self.backoff * 2).min(EnrTreeSync::MAX_RETRY_INTERVAL);
    }

    /// Records the outcome of a finished walk and schedules the next root lookup.
    pub fn finish_walk(&mut self, walk: EnrTreeWalk) {
        if walk.truncated {
            warn!(target: "discovery::enrtree", tree = %self.link, lookups = walk.lookups, records = walk.records, "ENR tree walk stopped at its budget");
        }
        if walk.records == 0 {
            self.retry_later(if walk.failed > 0 {
                "tree entries failed to resolve"
            } else {
                "tree has no valid records"
            });
        } else if walk.failed > 0 {
            self.retry_later("some tree entries failed to resolve");
        } else {
            info!(target: "discovery::enrtree", tree = %self.link, records = walk.records, "Synced ENR tree");
            self.synced_root = Some(walk.root.enr_root);
            self.backoff = self.retry_interval;
            self.schedule(EnrTreeSync::RECHECK_INTERVAL);
        }
    }
}

impl EnrTreeWalk {
    /// Starts a walk at the root's ENR subtree.
    pub fn new(root: TreeRootEntry) -> Self {
        let pending = VecDeque::from([(root.enr_root.clone(), 0)]);
        Self { root, pending, lookup: None, lookups: 0, records: 0, failed: 0, truncated: false }
    }

    /// Whether the walk has nothing left to do or has hit a budget.
    pub fn is_done(&mut self, max_lookups: usize, max_records: usize) -> bool {
        if self.lookups >= max_lookups || self.records >= max_records {
            self.truncated |= !self.pending.is_empty();
            return true;
        }
        self.pending.is_empty()
    }

    /// Whether `txt` is the entry published under the base32 hash label `hash`.
    pub fn hash_matches(hash: &str, txt: &str) -> bool {
        BASE32_NOPAD.decode(hash.as_bytes()).is_ok_and(|expected| {
            (EnrTreeSync::MIN_HASH_BYTES..=EnrTreeSync::MAX_HASH_BYTES).contains(&expected.len())
                && keccak256(txt.as_bytes()).starts_with(&expected)
        })
    }

    /// Handles the result of an entry lookup and returns a record to yield, if any.
    pub fn handle(
        &mut self,
        link: &LinkEntry,
        chain_id: u64,
        max_lookups: usize,
        done: EnrTreeLookup,
        txt: Option<String>,
    ) -> Option<Enr> {
        let Some(txt) = txt else {
            if done.attempts + 1 < EnrTreeSync::MAX_ENTRY_ATTEMPTS {
                self.pending.push_back((done.hash, done.attempts + 1));
            } else {
                debug!(target: "discovery::enrtree", tree = %link, hash = %done.hash, "Giving up on ENR tree entry that failed to resolve");
                self.failed += 1;
            }
            return None;
        };
        if !Self::hash_matches(&done.hash, &txt) {
            debug!(target: "discovery::enrtree", tree = %link, hash = %done.hash, "Ignoring ENR tree entry that does not match its hash");
            return None;
        }
        match txt.parse::<DnsEntry<SecretKey>>() {
            Ok(DnsEntry::Branch(branch)) => {
                let room = max_lookups.saturating_sub(self.lookups + self.pending.len());
                if branch.children.len() > room {
                    self.truncated = true;
                }
                self.pending.extend(branch.children.into_iter().take(room).map(|hash| (hash, 0)));
                None
            }
            Ok(DnsEntry::Node(node)) => {
                // Re-decoding as a discv5 record re-verifies the signature.
                let encoded = alloy_rlp::encode(&node.enr);
                let enr = match Enr::decode(&mut encoded.as_slice()) {
                    Ok(enr) => enr,
                    Err(error) => {
                        debug!(target: "discovery::enrtree", tree = %link, error = %error, "Ignoring undecodable ENR tree record");
                        return None;
                    }
                };
                let validation = EnrValidation::validate(&enr, chain_id);
                if validation.is_invalid() {
                    debug!(target: "discovery::enrtree", tree = %link, validation = %validation, "Ignoring ENR tree record that is not a valid consensus-layer ENR");
                    return None;
                }
                self.records += 1;
                Some(enr)
            }
            Ok(_) => {
                debug!(target: "discovery::enrtree", tree = %link, hash = %done.hash, "Ignoring root or link entry inside the ENR subtree");
                None
            }
            Err(error) => {
                debug!(target: "discovery::enrtree", tree = %link, hash = %done.hash, error = %error, "Ignoring unparseable ENR tree entry");
                None
            }
        }
    }
}

impl<R: Resolver> Stream for EnrTreeSync<R> {
    type Item = Enr;

    fn poll_next(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        let this = self.get_mut();

        // Each iteration either changes state, starts or finishes one lookup, or returns. Lookups
        // are paced, so one poll does a bounded amount of work.
        loop {
            match &mut this.state {
                EnrTreeState::Idle => {
                    ready!(this.timer.as_mut().poll(cx));
                    this.state =
                        EnrTreeState::Root(Self::lookup(&this.resolver, this.link.domain.clone()));
                }
                EnrTreeState::Root(lookup) => {
                    let txt = ready!(lookup.as_mut().poll(cx));
                    match this.verify_root(txt) {
                        Ok(root) if this.synced_root.as_ref() == Some(&root.enr_root) => {
                            trace!(target: "discovery::enrtree", tree = %this.link, "ENR tree is unchanged");
                            this.schedule(EnrTreeSync::RECHECK_INTERVAL);
                        }
                        Ok(root) => {
                            debug!(target: "discovery::enrtree", tree = %this.link, seq = root.sequence_number, "Walking ENR tree");
                            this.state = EnrTreeState::Walk(EnrTreeWalk::new(root));
                        }
                        Err(reason) => this.retry_later(reason),
                    }
                }
                EnrTreeState::Walk(walk) => {
                    if let Some(inflight) = walk.lookup.as_mut() {
                        let txt = ready!(inflight.lookup.as_mut().poll(cx));
                        let Some(done) = walk.lookup.take() else { continue };
                        if let Some(enr) =
                            walk.handle(&this.link, this.chain_id, this.max_lookups, done, txt)
                        {
                            return Poll::Ready(Some(enr));
                        }
                        continue;
                    }
                    if walk.is_done(this.max_lookups, this.max_records) {
                        let EnrTreeState::Walk(walk) =
                            std::mem::replace(&mut this.state, EnrTreeState::Idle)
                        else {
                            continue;
                        };
                        this.finish_walk(walk);
                        continue;
                    }
                    ready!(this.pacer.as_mut().poll(cx));
                    this.pacer.as_mut().reset(Instant::now() + EnrTreeSync::LOOKUP_INTERVAL);
                    let Some((hash, attempts)) = walk.pending.pop_front() else { continue };
                    walk.lookups += 1;
                    let lookup =
                        Self::lookup(&this.resolver, format!("{hash}.{}", this.link.domain));
                    walk.lookup = Some(EnrTreeLookup { hash, attempts, lookup });
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use std::{
        collections::HashSet,
        net::Ipv4Addr,
        sync::{
            Mutex,
            atomic::{AtomicUsize, Ordering},
        },
    };

    use alloy_rlp::Encodable;
    use base_consensus_peers::BaseEnr;
    use discv5::enr::CombinedKey;
    use futures::StreamExt;
    use reth_dns_discovery::MapResolver;
    use secp256k1::{PublicKey, Secp256k1};

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
    async fn collect<R: Resolver>(sync: &mut EnrTreeSync<R>, window: Duration) -> Vec<Enr> {
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

    #[tokio::test(start_paused = true)]
    async fn walk_stops_at_record_budget() {
        let key = tree_key(0x88);
        let resolver = Arc::new(MapResolver::default());
        let enrs: Vec<Enr> = (0..10).map(|_| enr(Some(CHAIN_ID))).collect();
        publish(&resolver, &key, &enrs);

        let mut sync =
            EnrTreeSync::new(link(&key), CHAIN_ID, resolver, EnrTreeSync::DEFAULT_RETRY_INTERVAL);
        sync.max_records = 3;

        assert_eq!(collect(&mut sync, COLLECT_WINDOW).await.len(), 3);
    }

    #[tokio::test(start_paused = true)]
    async fn walk_stops_at_lookup_budget() {
        let key = tree_key(0x99);
        let resolver = Arc::new(CountingResolver::default());
        let enrs: Vec<Enr> = (0..10).map(|_| enr(Some(CHAIN_ID))).collect();
        publish(&resolver.inner, &key, &enrs);

        let mut sync = EnrTreeSync::new(
            link(&key),
            CHAIN_ID,
            Arc::clone(&resolver),
            EnrTreeSync::DEFAULT_RETRY_INTERVAL,
        );
        sync.max_lookups = 4;

        // One lookup for the branch leaves room for three of the ten leaves.
        assert_eq!(collect(&mut sync, COLLECT_WINDOW).await.len(), 3);
        // The root plus four entry lookups.
        assert_eq!(resolver.lookups.load(Ordering::Relaxed), 5);
    }

    #[tokio::test(start_paused = true)]
    async fn invalid_root_is_retried_with_backoff() {
        let resolver = Arc::new(CountingResolver::default());
        publish(&resolver.inner, &tree_key(0x44), &[enr(Some(CHAIN_ID))]);

        let mut sync = EnrTreeSync::new(
            link(&tree_key(0x45)),
            CHAIN_ID,
            Arc::clone(&resolver),
            EnrTreeSync::DEFAULT_RETRY_INTERVAL,
        );

        assert!(collect(&mut sync, Duration::from_secs(2 * 60 * 60)).await.is_empty());
        // Lookups at 0, 1, 3, 7, 15, 31, 61 and 91 minutes: the delay doubles up to 30 minutes.
        assert_eq!(resolver.root_lookups.load(Ordering::Relaxed), 8);
    }

    #[tokio::test(start_paused = true)]
    async fn rewalks_tree_only_when_its_root_changes() {
        let key = tree_key(0xaa);
        let resolver = Arc::new(MapResolver::default());
        let first = enr(Some(CHAIN_ID));
        publish(&resolver, &key, std::slice::from_ref(&first));

        let mut sync = EnrTreeSync::new(
            link(&key),
            CHAIN_ID,
            Arc::clone(&resolver),
            EnrTreeSync::DEFAULT_RETRY_INTERVAL,
        );
        assert_eq!(collect(&mut sync, COLLECT_WINDOW).await, vec![first]);

        // The recheck finds the same root and does not walk the tree again.
        let recheck = EnrTreeSync::RECHECK_INTERVAL + COLLECT_WINDOW;
        assert!(collect(&mut sync, recheck).await.is_empty());

        let second = enr(Some(CHAIN_ID));
        publish(&resolver, &key, std::slice::from_ref(&second));
        assert_eq!(collect(&mut sync, recheck).await, vec![second]);
    }

    #[tokio::test(start_paused = true)]
    async fn healthy_long_walk_survives_retry_interval() {
        let key = tree_key(0x66);
        let resolver = Arc::new(MapResolver::default());
        let valid = enr(Some(CHAIN_ID));
        let text = valid.to_base64();
        resolver.insert(format!("{}.{DOMAIN}", entry_hash(&text)), text.clone());
        let mut child = entry_hash(&text);
        for _ in 0..200 {
            let branch = format!("enrtree-branch:{child}");
            child = entry_hash(&branch);
            resolver.insert(format!("{child}.{DOMAIN}"), branch);
        }
        let mut root: TreeRootEntry = "enrtree-root:v1 e=QFT4PBCRX4XQCV3VUYJ6BTCEPU l=JGUFMSAGI7KZYB3P7IZW4S5Y3A seq=1 sig=3FmXuVwpa8Y7OstZTx9PIb1mt8FrW7VpDOFv4AaGCsZ2EIHmhraWhe4NxYhQDlw5MjeFXYMbJjsPeKlHzmJREQE".parse().unwrap();
        root.enr_root = child;
        root.sign(&key).unwrap();
        resolver.insert(DOMAIN.to_string(), root.to_string());
        let mut control =
            EnrTreeSync::new(link(&key), CHAIN_ID, Arc::clone(&resolver), Duration::from_secs(300));
        let control_records = collect(&mut control, Duration::from_secs(240)).await;
        assert_eq!(control_records, vec![valid.clone()], "control must traverse the signed tree");
        let mut production =
            EnrTreeSync::new(link(&key), CHAIN_ID, resolver, EnrTreeSync::DEFAULT_RETRY_INTERVAL);
        let production_records = collect(&mut production, Duration::from_secs(240)).await;
        eprintln!(
            "healthy 200-branch tree: control={}, production={}",
            control_records.len(),
            production_records.len()
        );
        assert_eq!(production_records, vec![valid], "healthy walks must not be restarted forever");
    }

    #[tokio::test(start_paused = true)]
    async fn does_not_follow_links_to_other_trees() {
        let key = tree_key(0x77);
        let resolver = Arc::new(CountingResolver::default());
        let valid = enr(Some(CHAIN_ID));
        let text = valid.to_base64();
        let hash = entry_hash(&text);
        for i in 0..1200 {
            let domain = format!("nodes{i}.example.org");
            let next: LinkEntry = LinkEntry {
                domain: format!("nodes{}.example.org", i + 1),
                pubkey: PublicKey::from_secret_key(&Secp256k1::new(), &key),
            };
            let link_text = next.to_string();
            let mut root: TreeRootEntry = "enrtree-root:v1 e=QFT4PBCRX4XQCV3VUYJ6BTCEPU l=JGUFMSAGI7KZYB3P7IZW4S5Y3A seq=1 sig=3FmXuVwpa8Y7OstZTx9PIb1mt8FrW7VpDOFv4AaGCsZ2EIHmhraWhe4NxYhQDlw5MjeFXYMbJjsPeKlHzmJREQE".parse().unwrap();
            root.enr_root = hash.clone();
            root.link_root = entry_hash(&link_text);
            root.sign(&key).unwrap();
            resolver.inner.insert(domain.clone(), root.to_string());
            resolver.inner.insert(format!("{hash}.{domain}"), text.clone());
            resolver.inner.insert(format!("{}.{domain}", root.link_root), link_text);
        }
        let seed: LinkEntry = LinkEntry {
            domain: "nodes0.example.org".to_string(),
            pubkey: PublicKey::from_secret_key(&Secp256k1::new(), &key),
        };
        let mut sync = EnrTreeSync::new(
            seed,
            CHAIN_ID,
            Arc::clone(&resolver),
            EnrTreeSync::DEFAULT_RETRY_INTERVAL,
        );
        let records = collect(&mut sync, Duration::from_secs(1000)).await;
        let roots = resolver.roots.lock().unwrap().clone();
        eprintln!("one seed yielded {} records from {} tree roots", records.len(), roots.len());

        // Links to other trees are not followed: only the configured tree is walked.
        assert_eq!(roots, HashSet::from(["nodes0.example.org".to_string()]));
        assert_eq!(records, vec![valid]);
    }

    /// Wraps [`MapResolver`] and records which tree roots were looked up. Hand-rolled because
    /// reth's `Resolver` trait returns `impl Future`, which `mockall` cannot automock.
    #[derive(Default)]
    struct CountingResolver {
        inner: MapResolver,
        roots: Mutex<HashSet<String>>,
        root_lookups: AtomicUsize,
        lookups: AtomicUsize,
    }

    impl Resolver for CountingResolver {
        async fn lookup_txt(&self, query: &str) -> Option<String> {
            self.lookups.fetch_add(1, Ordering::Relaxed);
            if query.starts_with("nodes") {
                self.root_lookups.fetch_add(1, Ordering::Relaxed);
                self.roots.lock().unwrap().insert(query.to_string());
            }
            self.inner.get(query)
        }
    }
}
