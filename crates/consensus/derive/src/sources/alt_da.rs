//! Alt-DA data source that resolves L1 commitments to off-chain batch bytes.

use alloc::{
    boxed::Box,
    string::{String, ToString},
    sync::Arc,
};
use core::fmt::Debug;

use alloy_primitives::{Address, Bytes};
use async_trait::async_trait;
use base_protocol::{BlockInfo, DERIVATION_VERSION_1, GENERIC_COMMITMENT_LEN};
use thiserror::Error;

use crate::{DataAvailabilityProvider, PipelineError, PipelineErrorKind, PipelineResult};

/// Error returned by an [`AltDaCommitmentResolver`].
#[derive(Debug, Error)]
pub enum AltDaResolverError {
    /// No object is stored for the commitment.
    #[error("alt-da commitment not found")]
    NotFound,
    /// The resolver failed to fetch the bytes (transport, server, or decode error).
    #[error("alt-da resolve failed: {0}")]
    Resolve(String),
}

impl From<AltDaResolverError> for PipelineErrorKind {
    fn from(err: AltDaResolverError) -> Self {
        let msg = err.to_string();
        match err {
            // A 404 is permanent in our model: the batcher posts a commitment to L1 only after a
            // successful S3 PUT, and S3 is read-after-write consistent, so a missing object means
            // real data loss or corruption. Halt with a diagnostic instead of retrying forever
            // (the pending-commitment buffer would otherwise spin on the same item indefinitely).
            AltDaResolverError::NotFound => PipelineError::Provider(msg).crit(),
            // Transport errors, timeouts, and 5xx responses are transient; retry.
            AltDaResolverError::Resolve(_) => PipelineError::Provider(msg).temp(),
        }
    }
}

/// Resolves a generic alt-DA commitment to the batch bytes stored off-chain (e.g. S3).
///
/// The trait lives in this `no_std` crate; the concrete HTTP client is injected by the node
/// binary, mirroring how [`BlobProvider`](crate::BlobProvider) is supplied.
#[async_trait]
pub trait AltDaCommitmentResolver: Debug + Send + Sync {
    /// Fetch the batch bytes for `commitment`, the 34-byte generic commitment without the
    /// leading derivation-version byte.
    async fn resolve(&self, commitment: &[u8]) -> Result<Bytes, AltDaResolverError>;
}

/// Shared handle to an [`AltDaCommitmentResolver`].
pub type DynAltDaResolver = Arc<dyn AltDaCommitmentResolver>;

/// Wraps an inner [`DataAvailabilityProvider`] to resolve alt-DA commitments.
///
/// With `resolver` `None` this is a transparent pass-through: the inner source drives
/// derivation unchanged (calldata or blobs).
///
/// With `resolver` `Some` the node derives from off-chain DA only. A `DERIVATION_VERSION_1`
/// (`0x01`) commitment is resolved to its stored bytes, and any other item (inline `0x00`
/// calldata or blob frames) is skipped. This is the post-cutover behavior and lets a shadow
/// follower derive purely from S3 during the dual-write window.
#[derive(Debug, Clone)]
pub struct AltDaDataSource<D> {
    inner: D,
    resolver: Option<DynAltDaResolver>,
    /// A commitment popped from the inner source but not yet resolved.
    ///
    /// Resolution happens after the item is consumed from the inner source, so a transient
    /// resolve failure must not drop the batch. The commitment is buffered here and retried at
    /// the top of the next `next()` call before any new inner item is pulled, preserving
    /// at-least-once delivery so the safe head stays in sync with the calldata path.
    pending: Option<Bytes>,
}

impl<D> AltDaDataSource<D> {
    /// Wrap `inner`. A `None` resolver is pass-through; `Some` enables alt-DA-only mode.
    pub const fn new(inner: D, resolver: Option<DynAltDaResolver>) -> Self {
        Self { inner, resolver, pending: None }
    }
}

#[async_trait]
impl<D> DataAvailabilityProvider for AltDaDataSource<D>
where
    D: DataAvailabilityProvider<Item = Bytes> + Send + Sync + Debug,
{
    type Item = Bytes;

    async fn next(
        &mut self,
        block_ref: &BlockInfo,
        batcher_addr: Address,
    ) -> PipelineResult<Self::Item> {
        // Clone the resolver handle (a cheap Arc) so the inner source and `pending` can be
        // borrowed mutably below. No resolver means transparent pass-through.
        let Some(resolver) = self.resolver.clone() else {
            return self.inner.next(block_ref, batcher_addr).await;
        };

        loop {
            // Retry a previously-popped commitment before pulling a new inner item, so a
            // transient resolve failure replays the same commitment instead of dropping the
            // batch (the inner item was already consumed when it was popped).
            let commitment = if let Some(commitment) = self.pending.clone() {
                commitment
            } else {
                let data = self.inner.next(block_ref, batcher_addr).await?;
                match data.first() {
                    Some(&DERIVATION_VERSION_1) => {
                        // A valid commitment tx is the version byte plus the fixed-size generic
                        // commitment. Skip malformed lengths (e.g. a bare 0x01) so they are not
                        // buffered and retried against the DA server forever.
                        if data.len() != 1 + GENERIC_COMMITMENT_LEN {
                            warn!(len = data.len(), "alt-da: skipping malformed commitment tx");
                            continue;
                        }
                        let commitment = Bytes::copy_from_slice(&data[1..]);
                        self.pending = Some(commitment.clone());
                        commitment
                    }
                    // Alt-DA mode ignores inline calldata/blob frames and any empty item; pull
                    // the next item. Termination relies on the inner source returning Eof once
                    // the block's data is exhausted.
                    other => {
                        trace!(first_byte = ?other, "alt-da: skipping non-commitment item");
                        continue;
                    }
                }
            };

            let bytes = resolver.resolve(&commitment).await?;
            self.pending = None;
            return Ok(bytes);
        }
    }

    fn clear(&mut self) {
        self.pending = None;
        self.inner.clear();
    }
}

#[cfg(test)]
mod tests {
    use alloc::{
        collections::{BTreeMap, VecDeque},
        vec,
        vec::Vec,
    };

    use super::*;

    #[derive(Debug, Default)]
    struct MockInner {
        items: VecDeque<Bytes>,
    }

    #[async_trait]
    impl DataAvailabilityProvider for MockInner {
        type Item = Bytes;

        async fn next(&mut self, _: &BlockInfo, _: Address) -> PipelineResult<Bytes> {
            self.items.pop_front().ok_or(PipelineError::Eof.temp())
        }

        fn clear(&mut self) {
            self.items.clear();
        }
    }

    #[derive(Debug)]
    struct MockResolver {
        map: BTreeMap<Vec<u8>, Bytes>,
    }

    #[async_trait]
    impl AltDaCommitmentResolver for MockResolver {
        async fn resolve(&self, commitment: &[u8]) -> Result<Bytes, AltDaResolverError> {
            self.map.get(commitment).cloned().ok_or(AltDaResolverError::NotFound)
        }
    }

    fn l1_commitment(commitment: &[u8]) -> Bytes {
        let mut data = vec![DERIVATION_VERSION_1];
        data.extend_from_slice(commitment);
        Bytes::from(data)
    }

    #[tokio::test]
    async fn passthrough_when_no_resolver() {
        let inner = MockInner { items: VecDeque::from([Bytes::from(vec![0x00, 1, 2, 3])]) };
        let mut src = AltDaDataSource::new(inner, None);
        let out = src.next(&BlockInfo::default(), Address::ZERO).await.unwrap();
        assert_eq!(out.as_ref(), &[0x00, 1, 2, 3]);
    }

    #[tokio::test]
    async fn resolves_commitment_and_skips_calldata() {
        let commitment = vec![0xaa; 34];
        let stored = Bytes::from(vec![0x00, 9, 9, 9]);
        let mut map = BTreeMap::new();
        map.insert(commitment.clone(), stored.clone());
        let resolver: DynAltDaResolver = Arc::new(MockResolver { map });

        let inner = MockInner {
            items: VecDeque::from([Bytes::from(vec![0x00, 7, 7, 7]), l1_commitment(&commitment)]),
        };
        let mut src = AltDaDataSource::new(inner, Some(resolver));
        let out = src.next(&BlockInfo::default(), Address::ZERO).await.unwrap();
        assert_eq!(out, stored);
    }

    #[tokio::test]
    async fn resolve_not_found_is_critical() {
        let resolver: DynAltDaResolver = Arc::new(MockResolver { map: BTreeMap::new() });
        let inner = MockInner { items: VecDeque::from([l1_commitment(&[0xbb; 34])]) };
        let mut src = AltDaDataSource::new(inner, Some(resolver));
        let err = src.next(&BlockInfo::default(), Address::ZERO).await.unwrap_err();
        assert!(matches!(err, PipelineErrorKind::Critical(_)));
    }

    #[tokio::test]
    async fn skips_malformed_commitment_tx() {
        // A bare 0x01 with no commitment payload must be skipped, not buffered and retried.
        let commitment = vec![0xaa; 34];
        let stored = Bytes::from(vec![0x00, 1, 2, 3]);
        let mut map = BTreeMap::new();
        map.insert(commitment.clone(), stored.clone());
        let resolver: DynAltDaResolver = Arc::new(MockResolver { map });
        let inner = MockInner {
            items: VecDeque::from([
                Bytes::from(vec![DERIVATION_VERSION_1]),
                l1_commitment(&commitment),
            ]),
        };
        let mut src = AltDaDataSource::new(inner, Some(resolver));
        let out = src.next(&BlockInfo::default(), Address::ZERO).await.unwrap();
        assert_eq!(out, stored);
    }

    #[derive(Debug)]
    struct FlakyResolver {
        calls: core::sync::atomic::AtomicUsize,
        bytes: Bytes,
    }

    #[async_trait]
    impl AltDaCommitmentResolver for FlakyResolver {
        async fn resolve(&self, _commitment: &[u8]) -> Result<Bytes, AltDaResolverError> {
            let n = self.calls.fetch_add(1, core::sync::atomic::Ordering::SeqCst);
            if n == 0 {
                Err(AltDaResolverError::Resolve("transient".into()))
            } else {
                Ok(self.bytes.clone())
            }
        }
    }

    // A transient resolve failure must retry the same commitment, not drop the batch: the inner
    // item is already consumed when popped, so without buffering the batch would be skipped.
    #[tokio::test]
    async fn retries_same_commitment_after_transient_failure() {
        let stored = Bytes::from(vec![0x00, 5, 5, 5]);
        let resolver: DynAltDaResolver = Arc::new(FlakyResolver {
            calls: core::sync::atomic::AtomicUsize::new(0),
            bytes: stored.clone(),
        });
        // Inner yields a single commitment, then Eof.
        let inner = MockInner { items: VecDeque::from([l1_commitment(&[0xcc; 34])]) };
        let mut src = AltDaDataSource::new(inner, Some(resolver));

        let err = src.next(&BlockInfo::default(), Address::ZERO).await.unwrap_err();
        assert!(matches!(err, PipelineErrorKind::Temporary(_)));

        // The commitment is retried (inner is now empty) and resolves on the second attempt.
        let out = src.next(&BlockInfo::default(), Address::ZERO).await.unwrap();
        assert_eq!(out, stored);
    }
}
