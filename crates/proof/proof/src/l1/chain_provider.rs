//! Contains the concrete implementation of the [`ChainProvider`] trait for the proof.

use alloc::{boxed::Box, sync::Arc, vec::Vec};

use alloy_consensus::{Header, Receipt, ReceiptEnvelope, TxEnvelope};
use alloy_eips::eip2718::Decodable2718;
use alloy_primitives::B256;
use alloy_rlp::Decodable;
use async_trait::async_trait;
use base_consensus_derive::ChainProvider;
use base_proof_mpt::{OrderedListWalker, TrieNode, TrieProvider};
use base_proof_preimage::{CommsClient, PreimageKey, PreimageKeyType};
use base_protocol::BlockInfo;
use spin::Mutex;

use crate::{HintType, errors::OracleProviderError};

/// The oracle-backed L1 chain provider for the client program.
#[derive(Debug, Clone)]
pub struct OracleL1ChainProvider<T: CommsClient> {
    /// The L1 head hash.
    pub l1_head: B256,
    /// The preimage oracle client.
    pub oracle: Arc<T>,
    l1_header_range_hint_coverage: Arc<Mutex<Option<(u64, u64)>>>,
}

impl<T: CommsClient> OracleL1ChainProvider<T> {
    /// Creates a new [`OracleL1ChainProvider`] with the given boot information and oracle client.
    pub fn new(l1_head: B256, oracle: Arc<T>) -> Self {
        Self { l1_head, oracle, l1_header_range_hint_coverage: Arc::new(Mutex::new(None)) }
    }

    async fn send_l1_header_range_hint(
        &self,
        start_number: u64,
        end_number: u64,
    ) -> Result<(), OracleProviderError> {
        if self.l1_header_range_hint_covers(start_number, end_number) {
            return Ok(());
        }

        let start_number_bytes = start_number.to_be_bytes();
        let end_number_bytes = end_number.to_be_bytes();
        let result = HintType::L1BlockHeaderRange
            .with_data(&[&start_number_bytes, &end_number_bytes, self.l1_head.as_ref()])
            .send(self.oracle.as_ref())
            .await;
        if result.is_ok() {
            self.record_l1_header_range_hint(start_number, end_number);
        }

        result
    }

    fn l1_header_range_hint_covers(&self, start_number: u64, end_number: u64) -> bool {
        match *self.l1_header_range_hint_coverage.lock() {
            Some((covered_start, covered_end)) => {
                start_number >= covered_start && end_number <= covered_end
            }
            None => false,
        }
    }

    fn record_l1_header_range_hint(&self, start_number: u64, end_number: u64) {
        let mut coverage = self.l1_header_range_hint_coverage.lock();
        *coverage = Some(match *coverage {
            Some((covered_start, covered_end)) => {
                (covered_start.min(start_number), covered_end.max(end_number))
            }
            None => (start_number, end_number),
        });
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
        // Fetch the starting block header.
        let mut header = self.header_by_hash(self.l1_head).await?;

        // Check if the block number is in range. If not, we can fail early.
        if block_number > header.number {
            return Err(OracleProviderError::BlockNumberPastHead(block_number, header.number));
        }

        if block_number < header.number {
            self.send_l1_header_range_hint(block_number, header.number).await?;
        }

        // Walk back the block headers to the desired block number.
        while header.number > block_number {
            header = self.header_by_hash(header.parent_hash).await?;
        }

        Ok(BlockInfo {
            hash: header.hash_slow(),
            number: header.number,
            parent_hash: header.parent_hash,
            timestamp: header.timestamp,
        })
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
    use alloc::{
        string::{String, ToString},
        sync::Arc,
        vec::Vec,
    };

    use alloy_consensus::Header;
    use alloy_rlp::Encodable;
    use async_trait::async_trait;
    use base_consensus_derive::ChainProvider;
    use base_proof_preimage::{
        HintWriterClient, PreimageKey, PreimageOracleClient,
        errors::{PreimageOracleError, PreimageOracleResult},
    };
    use spin::Mutex;

    use super::*;

    #[derive(Clone, Default)]
    struct MockOracle {
        preimages: Arc<Mutex<Vec<(PreimageKey, Vec<u8>)>>>,
        hints: Arc<Mutex<Vec<String>>>,
    }

    impl MockOracle {
        fn insert_header(&self, header: &Header) -> B256 {
            let hash = header.hash_slow();
            let mut encoded_header = Vec::new();
            header.encode(&mut encoded_header);
            self.preimages.lock().push((PreimageKey::new_keccak256(*hash), encoded_header));
            hash
        }

        fn hints(&self) -> Vec<String> {
            self.hints.lock().clone()
        }
    }

    #[async_trait]
    impl PreimageOracleClient for MockOracle {
        async fn get(&self, key: PreimageKey) -> PreimageOracleResult<Vec<u8>> {
            self.preimages
                .lock()
                .iter()
                .find_map(|(entry_key, value)| (*entry_key == key).then(|| value.clone()))
                .ok_or(PreimageOracleError::KeyNotFound)
        }

        async fn get_exact(&self, key: PreimageKey, buf: &mut [u8]) -> PreimageOracleResult<()> {
            let value = self.get(key).await?;
            if value.len() != buf.len() {
                return Err(PreimageOracleError::BufferLengthMismatch(buf.len(), value.len()));
            }

            buf.copy_from_slice(&value);
            Ok(())
        }
    }

    #[async_trait]
    impl HintWriterClient for MockOracle {
        async fn write(&self, hint: &str) -> PreimageOracleResult<()> {
            self.hints.lock().push(hint.to_string());
            Ok(())
        }
    }

    #[tokio::test]
    async fn test_l1_header_range_hint_reuses_covered_range_across_clones() {
        let oracle = MockOracle::default();
        let block_1 = Header { number: 1, ..Default::default() };
        let block_1_hash = oracle.insert_header(&block_1);
        let block_2 = Header { number: 2, parent_hash: block_1_hash, ..Default::default() };
        let block_2_hash = oracle.insert_header(&block_2);
        let head = Header { number: 3, parent_hash: block_2_hash, ..Default::default() };
        let l1_head = oracle.insert_header(&head);
        let mut provider = OracleL1ChainProvider::new(l1_head, Arc::new(oracle.clone()));
        let mut cloned_provider = provider.clone();

        provider.block_info_by_number(1).await.unwrap();
        cloned_provider.block_info_by_number(2).await.unwrap();

        let range_hints =
            oracle.hints().iter().filter(|hint| hint.starts_with("l1-block-header-range ")).count();
        assert_eq!(range_hints, 1);
    }

    #[tokio::test]
    async fn test_l1_header_range_hint_sends_wider_range_after_recent_lookup() {
        let oracle = MockOracle::default();
        let block_1 = Header { number: 1, ..Default::default() };
        let block_1_hash = oracle.insert_header(&block_1);
        let block_2 = Header { number: 2, parent_hash: block_1_hash, ..Default::default() };
        let block_2_hash = oracle.insert_header(&block_2);
        let head = Header { number: 3, parent_hash: block_2_hash, ..Default::default() };
        let l1_head = oracle.insert_header(&head);
        let mut provider = OracleL1ChainProvider::new(l1_head, Arc::new(oracle.clone()));
        let mut cloned_provider = provider.clone();

        provider.block_info_by_number(2).await.unwrap();
        cloned_provider.block_info_by_number(1).await.unwrap();

        let range_hints =
            oracle.hints().iter().filter(|hint| hint.starts_with("l1-block-header-range ")).count();
        assert_eq!(range_hints, 2);
    }
}
