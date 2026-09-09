//! Handles query execution

use std::{
    collections::VecDeque,
    future::Future,
    num::NonZeroUsize,
    pin::Pin,
    sync::Arc,
    task::{Context, Poll, ready},
    time::Duration,
};

use alloy_primitives::keccak256;
use base_common_runtime_tasks::ratelimit::{Rate, RateLimit};
use data_encoding::BASE32_NOPAD;
use enr::EnrKeyUnambiguous;

use crate::dns::{
    error::{DnsLookupError, DnsLookupResult},
    resolver::DnsLookup,
    sync::DnsResolveKind,
    tree::{DnsEntry, DnsLinkEntry, DnsTreeRootEntry},
};

/// Minimum number of bytes an abbreviated EIP-1459 content hash may contain.
const MIN_HASH_BYTES: usize = 12;
/// Maximum number of bytes an abbreviated EIP-1459 content hash may contain.
const MAX_HASH_BYTES: usize = 32;

/// The `DnsQueryPool` provides an aggregate state machine for driving queries to completion.
pub struct DnsQueryPool<R: DnsLookup, K: EnrKeyUnambiguous> {
    /// The [DnsLookup] that's used to lookup queries.
    resolver: Arc<R>,
    /// Buffered queries
    queued_queries: VecDeque<Query<K>>,
    /// All active queries
    active_queries: Vec<Query<K>>,
    /// buffered results
    queued_outcomes: VecDeque<DnsQueryOutcome<K>>,
    /// Rate limit for DNS requests
    rate_limit: RateLimit,
    /// Timeout for DNS lookups.
    lookup_timeout: Duration,
}

// === impl DnsQueryPool ===

impl<R: DnsLookup, K: EnrKeyUnambiguous> DnsQueryPool<R, K> {
    pub fn new(
        resolver: Arc<R>,
        max_requests_per_sec: NonZeroUsize,
        lookup_timeout: Duration,
    ) -> Self {
        Self {
            resolver,
            queued_queries: Default::default(),
            active_queries: vec![],
            queued_outcomes: Default::default(),
            rate_limit: RateLimit::new(Rate::new(
                max_requests_per_sec.get() as u64,
                Duration::from_secs(1),
            )),
            lookup_timeout,
        }
    }

    /// Resolves the root the link's domain references
    pub fn resolve_root(&mut self, link: DnsLinkEntry<K>) {
        let resolver = Arc::clone(&self.resolver);
        let timeout = self.lookup_timeout;
        self.queued_queries.push_back(Query::Root(Box::pin(resolve_root(resolver, link, timeout))))
    }

    /// Resolves the [`DnsEntry`] for `<hash.domain>`
    pub fn resolve_entry(&mut self, link: DnsLinkEntry<K>, hash: String, kind: DnsResolveKind) {
        let resolver = Arc::clone(&self.resolver);
        let timeout = self.lookup_timeout;
        self.queued_queries
            .push_back(Query::Entry(Box::pin(resolve_entry(resolver, link, hash, kind, timeout))))
    }

    /// Advances the state of the queries
    pub fn poll(&mut self, cx: &mut Context<'_>) -> Poll<DnsQueryOutcome<K>> {
        loop {
            // drain buffered events first
            if let Some(event) = self.queued_outcomes.pop_front() {
                return Poll::Ready(event);
            }

            // queue in new queries if we have capacity
            'queries: while self.active_queries.len() < self.rate_limit.limit() as usize {
                if self.rate_limit.poll_ready(cx).is_ready()
                    && let Some(query) = self.queued_queries.pop_front()
                {
                    self.rate_limit.tick();
                    self.active_queries.push(query);
                    continue 'queries;
                }
                break;
            }

            // advance all queries
            for idx in (0..self.active_queries.len()).rev() {
                let mut query = self.active_queries.swap_remove(idx);
                if let Poll::Ready(outcome) = query.poll(cx) {
                    self.queued_outcomes.push_back(outcome);
                } else {
                    // still pending
                    self.active_queries.push(query);
                }
            }

            if self.queued_outcomes.is_empty() {
                return Poll::Pending;
            }
        }
    }
}

// === Various future/type alias ===

pub struct DnsResolveEntryResult<K: EnrKeyUnambiguous> {
    pub entry: Option<DnsLookupResult<DnsEntry<K>>>,
    pub link: DnsLinkEntry<K>,
    pub hash: String,
    pub kind: DnsResolveKind,
}

pub type DnsResolveRootResult<K> =
    Result<(DnsTreeRootEntry, DnsLinkEntry<K>), (DnsLookupError, DnsLinkEntry<K>)>;

type ResolveRootFuture<K> = Pin<Box<dyn Future<Output = DnsResolveRootResult<K>> + Send>>;

type ResolveEntryFuture<K> = Pin<Box<dyn Future<Output = DnsResolveEntryResult<K>> + Send>>;

enum Query<K: EnrKeyUnambiguous> {
    Root(ResolveRootFuture<K>),
    Entry(ResolveEntryFuture<K>),
}

// === impl Query ===

impl<K: EnrKeyUnambiguous> Query<K> {
    /// Advances the query
    fn poll(&mut self, cx: &mut Context<'_>) -> Poll<DnsQueryOutcome<K>> {
        match self {
            Self::Root(query) => {
                let outcome = ready!(query.as_mut().poll(cx));
                Poll::Ready(DnsQueryOutcome::Root(outcome))
            }
            Self::Entry(query) => {
                let outcome = ready!(query.as_mut().poll(cx));
                Poll::Ready(DnsQueryOutcome::Entry(outcome))
            }
        }
    }
}

/// The output the queries return
pub enum DnsQueryOutcome<K: EnrKeyUnambiguous> {
    Root(DnsResolveRootResult<K>),
    Entry(DnsResolveEntryResult<K>),
}

/// Retrieves the [`DnsEntry`]
async fn resolve_entry<K: EnrKeyUnambiguous, R: DnsLookup>(
    resolver: Arc<R>,
    link: DnsLinkEntry<K>,
    hash: String,
    kind: DnsResolveKind,
    timeout: Duration,
) -> DnsResolveEntryResult<K> {
    let fqn = format!("{hash}.{}", link.domain);
    let mut resp = DnsResolveEntryResult { entry: None, link, hash, kind };
    match lookup_with_timeout::<R>(&resolver, &fqn, timeout).await {
        Ok(Some(entry)) => {
            resp.entry = Some(match verify_entry_hash(&resp.hash, &entry) {
                Ok(()) => entry.parse::<DnsEntry<K>>().map_err(Into::into),
                Err(err) => Err(err),
            })
        }
        Err(err) => resp.entry = Some(Err(err)),
        Ok(None) => {}
    }
    resp
}

/// Verifies that `entry_txt` belongs under the queried
/// [EIP-1459](https://eips.ethereum.org/EIPS/eip-1459) hash label.
///
/// Entries resolved through `<hash>.<domain>` are stored below the base32 encoding of an
/// abbreviated `keccak256` digest of their TXT content. The protocol accepts any prefix length in
/// the valid hash range.
fn verify_entry_hash(hash: &str, entry_txt: &str) -> DnsLookupResult<()> {
    let expected = BASE32_NOPAD
        .decode(hash.as_bytes())
        .map_err(|_| DnsLookupError::HashMismatch(hash.into()))?;
    let actual = keccak256(entry_txt.as_bytes());

    if !(MIN_HASH_BYTES..=MAX_HASH_BYTES).contains(&expected.len()) {
        return Err(DnsLookupError::HashMismatch(hash.into()));
    }

    if actual.as_slice().starts_with(&expected) {
        Ok(())
    } else {
        Err(DnsLookupError::HashMismatch(hash.into()))
    }
}

/// Retrieves the root entry the link points to and returns the verified entry
///
/// Returns an error if the record could be retrieved but is not a root entry or failed to be
/// verified.
async fn resolve_root<K: EnrKeyUnambiguous, R: DnsLookup>(
    resolver: Arc<R>,
    link: DnsLinkEntry<K>,
    timeout: Duration,
) -> DnsResolveRootResult<K> {
    let root = match lookup_with_timeout::<R>(&resolver, &link.domain, timeout).await {
        Ok(Some(root)) => root,
        Ok(_) => return Err((DnsLookupError::EntryNotFound, link)),
        Err(err) => return Err((err, link)),
    };

    match root.parse::<DnsTreeRootEntry>() {
        Ok(root) => {
            if root.verify::<K>(&link.pubkey) {
                Ok((root, link))
            } else {
                Err((DnsLookupError::InvalidRoot(root), link))
            }
        }
        Err(err) => Err((err.into(), link)),
    }
}

async fn lookup_with_timeout<R: DnsLookup>(
    r: &R,
    query: &str,
    timeout: Duration,
) -> DnsLookupResult<Option<String>> {
    tokio::time::timeout(timeout, r.lookup_txt(query))
        .await
        .map_err(|_| DnsLookupError::RequestTimedOut)
}

#[cfg(test)]
mod tests {
    use std::future::poll_fn;

    use super::*;
    use crate::dns::{DnsDiscoveryConfig, DnsMapResolver, DnsTimeoutResolver};

    fn entry_hash(entry_txt: &str) -> String {
        BASE32_NOPAD.encode(&keccak256(entry_txt.as_bytes()).as_slice()[..16])
    }

    #[tokio::test]
    async fn test_rate_limit() {
        let resolver = Arc::new(DnsMapResolver::default());
        let config = DnsDiscoveryConfig::default();
        let mut pool =
            DnsQueryPool::new(resolver, config.max_requests_per_sec, config.lookup_timeout);

        let s = "enrtree://AM5FCQLWIZX2QFPNJAP7VUERCCRNGRHWZG3YYHIUV7BVDQ5FDPRT2@nodes.example.org";
        let entry: DnsLinkEntry = s.parse().unwrap();

        for _n in 0..config.max_requests_per_sec.get() {
            poll_fn(|cx| {
                pool.resolve_root(entry.clone());
                assert_eq!(pool.queued_queries.len(), 1);
                assert!(pool.rate_limit.poll_ready(cx).is_ready());
                let _ = pool.poll(cx);
                assert_eq!(pool.queued_queries.len(), 0);
                Poll::Ready(())
            })
            .await;
        }

        pool.resolve_root(entry.clone());
        assert_eq!(pool.queued_queries.len(), 1);
        poll_fn(|cx| {
            assert!(pool.rate_limit.poll_ready(cx).is_pending());
            let _ = pool.poll(cx);
            assert_eq!(pool.queued_queries.len(), 1);
            Poll::Ready(())
        })
        .await;
    }

    #[tokio::test]
    async fn test_timeouts() {
        let config =
            DnsDiscoveryConfig { lookup_timeout: Duration::from_millis(500), ..Default::default() };
        let resolver = Arc::new(DnsTimeoutResolver(config.lookup_timeout * 2));
        let mut pool =
            DnsQueryPool::new(resolver, config.max_requests_per_sec, config.lookup_timeout);

        let s = "enrtree://AM5FCQLWIZX2QFPNJAP7VUERCCRNGRHWZG3YYHIUV7BVDQ5FDPRT2@nodes.example.org";
        let entry: DnsLinkEntry = s.parse().unwrap();
        pool.resolve_root(entry);

        let outcome = poll_fn(|cx| pool.poll(cx)).await;

        match outcome {
            DnsQueryOutcome::Root(res) => {
                let res = res.unwrap_err().0;
                match res {
                    DnsLookupError::RequestTimedOut => {}
                    _ => unreachable!(),
                }
            }
            DnsQueryOutcome::Entry(_) => {
                unreachable!()
            }
        }
    }

    #[test]
    fn verify_entry_hash_accepts_eip_1459_vectors() {
        let entries = [
            (
                "C7HRFPF3BLGF3YR4DY5KX3SMBE",
                "enrtree://AM5FCQLWIZX2QFPNJAP7VUERCCRNGRHWZG3YYHIUV7BVDQ5FDPRT2@morenodes.example.org",
            ),
            (
                "JWXYDBPXYWG6FX3GMDIBFA6CJ4",
                "enrtree-branch:2XS2367YHAXJFGLZHVAWLQD4ZY,H4FHT4B454P6UXFD7JCYQ5PWDY,MHTDO6TMUBRIA2XWG5LUDACK24",
            ),
            (
                "2XS2367YHAXJFGLZHVAWLQD4ZY",
                "enr:-HW4QOFzoVLaFJnNhbgMoDXPnOvcdVuj7pDpqRvh6BRDO68aVi5ZcjB3vzQRZH2IcLBGHzo8uUN3snqmgTiE56CH3AMBgmlkgnY0iXNlY3AyNTZrMaECC2_24YYkYHEgdzxlSNKQEnHhuNAbNlMlWJxrJxbAFvA",
            ),
        ];

        for (hash, entry) in entries {
            verify_entry_hash(hash, entry).unwrap();
        }
    }

    #[test]
    fn verify_entry_hash_rejects_mismatched_or_invalid_hashes() {
        let entry = "enrtree-branch:YNEGZIWHOM7TOOSUATAPTM";
        let hash = entry_hash(entry);
        verify_entry_hash(&hash, entry).unwrap();

        assert!(matches!(
            verify_entry_hash(&hash, "enrtree-branch:AAAAAAAAAAAAAAAAAAAA"),
            Err(DnsLookupError::HashMismatch(_))
        ));
        assert!(matches!(
            verify_entry_hash("NOT_BASE32!", entry),
            Err(DnsLookupError::HashMismatch(_))
        ));
        assert!(matches!(verify_entry_hash("AAAA", entry), Err(DnsLookupError::HashMismatch(_))));
    }
}
