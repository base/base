//! Alt-DA data source that resolves L1 commitments to off-chain batch bytes.

use alloc::{
    boxed::Box,
    string::{String, ToString},
    sync::Arc,
};
use core::fmt::Debug;

use alloy_primitives::{Address, Bytes};
use async_trait::async_trait;
use base_protocol::{BlockInfo, DERIVATION_VERSION_1};
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
        // Both NotFound and transport/server errors map to a temporary error so derivation
        // retries rather than halting. The batcher posts a commitment to L1 only after a
        // successful S3 PUT, so any committed pointer has a backing object; a NotFound at
        // derivation time is therefore transient (read-after-write visibility or a brief DA
        // server outage), not a permanently missing object.
        PipelineError::Provider(err.to_string()).temp()
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
}

impl<D> AltDaDataSource<D> {
    /// Wrap `inner`. A `None` resolver is pass-through; `Some` enables alt-DA-only mode.
    pub const fn new(inner: D, resolver: Option<DynAltDaResolver>) -> Self {
        Self { inner, resolver }
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
        loop {
            let data = self.inner.next(block_ref, batcher_addr).await?;
            let Some(resolver) = &self.resolver else {
                return Ok(data);
            };
            match data.first() {
                Some(&DERIVATION_VERSION_1) => {
                    let bytes = resolver.resolve(&data[1..]).await?;
                    return Ok(bytes);
                }
                // Alt-DA mode ignores inline calldata/blob frames (Some(_)) and any empty item
                // (None); pull the next item. Termination relies on the inner source returning
                // Eof once the block's data is exhausted.
                Some(_) | None => continue,
            }
        }
    }

    fn clear(&mut self) {
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
    async fn resolve_not_found_is_temporary() {
        let resolver: DynAltDaResolver = Arc::new(MockResolver { map: BTreeMap::new() });
        let inner = MockInner { items: VecDeque::from([l1_commitment(&[0xbb; 34])]) };
        let mut src = AltDaDataSource::new(inner, Some(resolver));
        let err = src.next(&BlockInfo::default(), Address::ZERO).await.unwrap_err();
        assert!(matches!(err, PipelineErrorKind::Temporary(_)));
    }
}
