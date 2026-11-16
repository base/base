//! Contains the concrete implementation of the [`ChainProvider`] trait for the proof.

use alloc::{boxed::Box, collections::BTreeMap, sync::Arc, vec::Vec};

use alloy_consensus::{Header, Receipt, ReceiptEnvelope, TxEnvelope};
use alloy_eips::eip2718::Decodable2718;
use alloy_primitives::B256;
use alloy_rlp::Decodable;
use async_trait::async_trait;
use base_consensus_derive::ChainProvider;
use base_proof_mpt::{OrderedListWalker, TrieNode, TrieProvider};
use base_proof_preimage::{CommsClient, PreimageKey, PreimageKeyType};
use base_protocol::BlockInfo;

use crate::{HintType, errors::OracleProviderError};

/// The oracle-backed L1 chain provider for the client program.
#[derive(Debug, Clone)]
pub struct OracleL1ChainProvider<T: CommsClient> {
    /// The L1 head hash.
    pub l1_head: B256,
    /// The preimage oracle client.
    pub oracle: Arc<T>,
    /// [`BlockInfo`] by number for every ancestor of [`Self::l1_head`] visited by
    /// [`ChainProvider::block_info_by_number`].
    ///
    /// The chain rooted at `l1_head` is fixed, so entries never need invalidation as long as
    /// `l1_head` is not reassigned after the first lookup. Unbounded: it holds one small entry
    /// per L1 block between the head and the lowest block looked up, which is the same set of
    /// headers the walk-back already decodes once.
    block_info_by_number: BTreeMap<u64, BlockInfo>,
}

impl<T: CommsClient> OracleL1ChainProvider<T> {
    /// Creates a new [`OracleL1ChainProvider`] with the given boot information and oracle client.
    pub const fn new(l1_head: B256, oracle: Arc<T>) -> Self {
        Self { l1_head, oracle, block_info_by_number: BTreeMap::new() }
    }
}

#[async_trait]
impl<T: CommsClient + Sync + Send> ChainProvider for OracleL1ChainProvider<T> {
    type Error = OracleProviderError;

    async fn header_by_hash(&mut self, hash: B256) -> Result<Header, Self::Error> {
        // Fetch the header RLP from the oracle.
        HintType::L1BlockHeader.with_data(&[hash.as_ref()]).send(self.oracle.as_ref()).await?;
        let header_rlp = self.oracle.get(PreimageKey::new_keccak256(*hash)).await?;

        // Decode the header RLP into a Header.
        Header::decode(&mut header_rlp.as_slice()).map_err(OracleProviderError::Rlp)
    }

    async fn block_info_by_number(&mut self, block_number: u64) -> Result<BlockInfo, Self::Error> {
        if let Some(cached) = self.block_info_by_number.get(&block_number) {
            return Ok(*cached);
        }

        // Resume from the closest cached descendant of the target, or from the head if none.
        // Every cached entry is at or below the head, so a target past the head always falls
        // through to the head check below.
        let cached_descendant = block_number
            .checked_add(1)
            .and_then(|n| self.block_info_by_number.range(n..).next().map(|(_, info)| *info));
        let mut current = match cached_descendant {
            Some(info) => info,
            None => {
                let header = self.header_by_hash(self.l1_head).await?;
                if block_number > header.number {
                    return Err(OracleProviderError::BlockNumberPastHead(
                        block_number,
                        header.number,
                    ));
                }
                let info = BlockInfo {
                    hash: self.l1_head,
                    number: header.number,
                    parent_hash: header.parent_hash,
                    timestamp: header.timestamp,
                };
                self.block_info_by_number.insert(info.number, info);
                info
            }
        };

        // Walk back to the target, caching each visited block. The oracle binds keccak preimages
        // to their keys, so the requested hash is the header hash without rehashing it.
        while current.number > block_number {
            let hash = current.parent_hash;
            let header = self.header_by_hash(hash).await?;
            current = BlockInfo {
                hash,
                number: header.number,
                parent_hash: header.parent_hash,
                timestamp: header.timestamp,
            };
            self.block_info_by_number.insert(current.number, current);
        }

        Ok(current)
    }

    async fn receipts_by_hash(&mut self, hash: B256) -> Result<Vec<Receipt>, Self::Error> {
        // Fetch the block header to find the receipts root.
        let header = self.header_by_hash(hash).await?;

        // Send a hint for the block's receipts, and walk through the receipts trie in the header to
        // verify them.
        HintType::L1Receipts.with_data(&[hash.as_ref()]).send(self.oracle.as_ref()).await?;
        let trie_walker = OrderedListWalker::try_new_hydrated(header.receipts_root, self)
            .map_err(OracleProviderError::TrieWalker)?;

        // Decode the receipts within the receipts trie.
        let receipts = trie_walker
            .into_iter()
            .map(|(_, rlp)| {
                let envelope = ReceiptEnvelope::decode_2718(&mut rlp.as_ref())?;
                Ok(envelope.as_receipt().expect("Infallible").clone())
            })
            .collect::<Result<Vec<_>, _>>()
            .map_err(OracleProviderError::Rlp)?;

        Ok(receipts)
    }

    async fn block_info_and_transactions_by_hash(
        &mut self,
        hash: B256,
    ) -> Result<(BlockInfo, Vec<TxEnvelope>), Self::Error> {
        // Fetch the block header to construct the block info.
        let header = self.header_by_hash(hash).await?;
        let block_info = BlockInfo {
            hash,
            number: header.number,
            parent_hash: header.parent_hash,
            timestamp: header.timestamp,
        };

        // Send a hint for the block's transactions, and walk through the transactions trie in the
        // header to verify them.
        HintType::L1Transactions.with_data(&[hash.as_ref()]).send(self.oracle.as_ref()).await?;
        let trie_walker = OrderedListWalker::try_new_hydrated(header.transactions_root, self)
            .map_err(OracleProviderError::TrieWalker)?;

        // Decode the transactions within the transactions trie.
        let transactions = trie_walker
            .into_iter()
            .map(|(_, rlp)| {
                // note: not short-handed for error type coercion w/ `?`.
                let rlp = TxEnvelope::decode_2718(&mut rlp.as_ref())?;
                Ok(rlp)
            })
            .collect::<Result<Vec<_>, _>>()
            .map_err(OracleProviderError::Rlp)?;

        Ok((block_info, transactions))
    }
}

impl<T: CommsClient> TrieProvider for OracleL1ChainProvider<T> {
    type Error = OracleProviderError;

    fn trie_node_by_hash(&self, key: B256) -> Result<TrieNode, Self::Error> {
        // On L1, trie node preimages are stored as keccak preimage types in the oracle. We assume
        // that a hint for these preimages has already been sent, prior to this call.
        crate::block_on(async move {
            TrieNode::decode(
                &mut self
                    .oracle
                    .get(PreimageKey::new(*key, PreimageKeyType::Keccak256))
                    .await
                    .map_err(OracleProviderError::Preimage)?
                    .as_ref(),
            )
            .map_err(OracleProviderError::Rlp)
        })
    }
}

#[cfg(test)]
mod tests {
    use core::sync::atomic::{AtomicUsize, Ordering};

    use alloy_rlp::Encodable;
    use base_proof_preimage::{
        HintWriterClient, PreimageOracleClient,
        errors::{PreimageOracleError, PreimageOracleResult},
    };

    use super::*;

    /// In-memory oracle serving RLP-encoded headers keyed by their hash and counting `get` calls.
    ///
    /// Hand-rolled because `base-proof-preimage` exposes no mocks outside its own crate, and the
    /// tests need real header preimages plus a fetch count shared across clones of the provider.
    #[derive(Clone, Debug, Default)]
    struct HeaderOracle {
        preimages: Arc<BTreeMap<PreimageKey, Vec<u8>>>,
        gets: Arc<AtomicUsize>,
    }

    #[async_trait]
    impl PreimageOracleClient for HeaderOracle {
        async fn get(&self, key: PreimageKey) -> PreimageOracleResult<Vec<u8>> {
            self.gets.fetch_add(1, Ordering::SeqCst);
            self.preimages.get(&key).cloned().ok_or(PreimageOracleError::KeyNotFound)
        }

        async fn get_exact(&self, key: PreimageKey, buf: &mut [u8]) -> PreimageOracleResult<()> {
            buf.copy_from_slice(&self.get(key).await?);
            Ok(())
        }
    }

    #[async_trait]
    impl HintWriterClient for HeaderOracle {
        async fn write(&self, _hint: &str) -> PreimageOracleResult<()> {
            Ok(())
        }
    }

    /// Builds a linear chain of `len` headers (oldest first) and a provider headed at the tip.
    fn chain(len: u64) -> (Vec<Header>, OracleL1ChainProvider<HeaderOracle>, Arc<AtomicUsize>) {
        let mut headers: Vec<Header> = Vec::new();
        for number in 0..len {
            let parent_hash = headers.last().map_or(B256::ZERO, Header::hash_slow);
            headers.push(Header {
                number,
                parent_hash,
                timestamp: 1_000 + number,
                ..Default::default()
            });
        }
        let preimages = headers
            .iter()
            .map(|header| {
                let mut rlp = Vec::new();
                header.encode(&mut rlp);
                (PreimageKey::new_keccak256(*header.hash_slow()), rlp)
            })
            .collect();
        let oracle = HeaderOracle { preimages: Arc::new(preimages), gets: Arc::default() };
        let gets = Arc::clone(&oracle.gets);
        let provider =
            OracleL1ChainProvider::new(headers[len as usize - 1].hash_slow(), Arc::new(oracle));
        (headers, provider, gets)
    }

    /// The [`BlockInfo`] the uncached walk-back derived from the header at `number`.
    fn uncached_block_info(headers: &[Header], number: u64) -> BlockInfo {
        let header = &headers[number as usize];
        BlockInfo {
            hash: header.hash_slow(),
            number: header.number,
            parent_hash: header.parent_hash,
            timestamp: header.timestamp,
        }
    }

    #[tokio::test]
    async fn block_info_by_number_matches_uncached_walk_in_any_order() {
        let (headers, mut provider, _) = chain(32);

        for number in [17, 31, 3, 17, 0, 25, 4, 2, 31, 30, 0, 16, 18] {
            let info = provider.block_info_by_number(number).await.unwrap();
            assert_eq!(info, uncached_block_info(&headers, number), "block {number}");
        }
    }

    #[tokio::test]
    async fn block_info_by_number_does_not_refetch_visited_headers() {
        let (_, mut provider, gets) = chain(20);

        // Head (19) down to 15 is five header fetches.
        provider.block_info_by_number(15).await.unwrap();
        assert_eq!(gets.load(Ordering::SeqCst), 5);

        // Resumes from cached block 15 rather than the head: 14, 13, 12, 11, 10.
        provider.block_info_by_number(10).await.unwrap();
        assert_eq!(gets.load(Ordering::SeqCst), 10);

        // Every block from the head down to 10 is now served without touching the oracle.
        for number in 10..20 {
            provider.block_info_by_number(number).await.unwrap();
        }
        assert_eq!(gets.load(Ordering::SeqCst), 10);
    }

    #[tokio::test]
    async fn block_info_by_number_rejects_numbers_past_head_with_warm_cache() {
        let (_, mut provider, _) = chain(5);
        provider.block_info_by_number(0).await.unwrap();

        for number in [5, u64::MAX] {
            let err = provider.block_info_by_number(number).await.unwrap_err();
            assert!(
                matches!(err, OracleProviderError::BlockNumberPastHead(n, 4) if n == number),
                "block {number}: {err:?}"
            );
        }
    }
}
