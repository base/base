//! Contains the concrete implementation of the [`ChainProvider`] trait for the proof.

use alloc::{boxed::Box, sync::Arc, vec::Vec};

use alloy_consensus::{Header, Receipt, ReceiptEnvelope, TxEnvelope};
use alloy_eips::eip2718::Decodable2718;
use alloy_primitives::B256;
use alloy_rlp::Decodable;
use async_trait::async_trait;
use base_common_consensus::{BaseReceiptEnvelope, BaseTxEnvelope};
use base_common_genesis::L1TxFormat;
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
    /// L1 format for tx and receipt decoding.
    pub l1_tx_format: L1TxFormat,
}

impl<T: CommsClient> OracleL1ChainProvider<T> {
    /// Creates a new [`OracleL1ChainProvider`] with the given boot information and oracle client.
    pub const fn new(l1_head: B256, oracle: Arc<T>, l1_tx_format: L1TxFormat) -> Self {
        Self { l1_head, oracle, l1_tx_format }
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
            .map(|(_, rlp)| match self.l1_tx_format {
                L1TxFormat::Base => {
                    let envelope = BaseReceiptEnvelope::decode_2718(&mut rlp.as_ref())?;
                    Ok(Receipt::from(envelope))
                }
                L1TxFormat::Ethereum => {
                    let envelope = ReceiptEnvelope::decode_2718(&mut rlp.as_ref())?;
                    Ok(envelope.as_receipt().expect("Infallible").clone())
                }
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
            .map(|(_, rlp)| match self.l1_tx_format {
                L1TxFormat::Base => {
                    let envelope = BaseTxEnvelope::decode_2718(&mut rlp.as_ref())?;
                    Ok(TxEnvelope::try_from(envelope).ok())
                }
                L1TxFormat::Ethereum => {
                    let envelope = TxEnvelope::decode_2718(&mut rlp.as_ref())?;
                    Ok(Some(envelope))
                }
            })
            .filter_map(Result::transpose)
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
    use alloc::{boxed::Box, sync::Arc, vec, vec::Vec};

    use alloy_consensus::{Signed, TxEip1559};
    use alloy_eips::eip2718::Encodable2718;
    use alloy_primitives::{Address, Bytes, Sealable, Signature, TxKind, U256, keccak256};
    use alloy_rlp::Encodable;
    use base_common_consensus::{
        BaseReceiptEnvelope, BaseTxEnvelope, Eip8130Signed, OpTxType, TxDeposit, TxEip8130,
    };
    use base_proof_mpt::ordered_trie_with_encoder;
    use base_proof_preimage::{
        PreimageKey,
        errors::{PreimageOracleError, PreimageOracleResult},
    };

    use super::*;

    #[derive(Clone)]
    struct MockOracle {
        data: Arc<Vec<(PreimageKey, Vec<u8>)>>,
    }

    impl MockOracle {
        fn new(data: Vec<(PreimageKey, Vec<u8>)>) -> Self {
            Self { data: Arc::new(data) }
        }
    }

    #[async_trait]
    impl base_proof_preimage::PreimageOracleClient for MockOracle {
        async fn get(&self, key: PreimageKey) -> PreimageOracleResult<Vec<u8>> {
            self.data
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
    impl base_proof_preimage::HintWriterClient for MockOracle {
        async fn write(&self, _hint: &str) -> PreimageOracleResult<()> {
            Ok(())
        }
    }

    fn trie_root_and_preimages(values: &[Vec<u8>]) -> (B256, Vec<(PreimageKey, Vec<u8>)>) {
        let mut trie = ordered_trie_with_encoder(values, |value, buf| {
            buf.put_slice(value.as_ref());
        });
        let root = trie.root();
        let preimages = trie
            .take_proof_nodes()
            .into_inner()
            .into_values()
            .map(|value| {
                let hash = keccak256(value.as_ref());
                (PreimageKey::new_keccak256(*hash), value.into())
            })
            .collect();
        (root, preimages)
    }

    fn header_preimage(header: &Header) -> (B256, Vec<u8>) {
        let hash = header.hash_slow();
        let mut encoded = Vec::new();
        header.encode(&mut encoded);
        (hash, encoded)
    }

    fn base_deposit_tx_2718() -> Vec<u8> {
        let tx = TxDeposit {
            source_hash: B256::repeat_byte(0x01),
            from: Address::repeat_byte(0x02),
            mint: 1,
            gas_limit: 2,
            to: TxKind::Call(Address::repeat_byte(0x03)),
            value: U256::from(4_u64),
            input: Bytes::from(vec![5]),
            is_system_transaction: false,
        };
        BaseTxEnvelope::Deposit(tx.seal_slow()).encoded_2718()
    }

    fn base_deposit_receipt_2718() -> Vec<u8> {
        BaseReceiptEnvelope::from_parts(true, 100, vec![], OpTxType::Deposit, Some(1), Some(2))
            .encoded_2718()
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn base_format_block_decodes_and_drops_deposit() {
        let txs = vec![base_deposit_tx_2718()];
        let (transactions_root, mut preimages) = trie_root_and_preimages(&txs);
        let header = Header { transactions_root, number: 42, timestamp: 1, ..Default::default() };
        let (hash, encoded_header) = header_preimage(&header);
        preimages.push((PreimageKey::new_keccak256(*hash), encoded_header));
        let oracle = Arc::new(MockOracle::new(preimages));
        let mut provider = OracleL1ChainProvider::new(hash, oracle, L1TxFormat::Base);

        let (info, txs) = provider.block_info_and_transactions_by_hash(hash).await.unwrap();

        assert_eq!(info.number, 42);
        assert!(
            txs.is_empty(),
            "the deposit is verified in the trie and then dropped during down-conversion"
        );
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn base_format_receipts_decode_and_preserve_deposit() {
        let receipts = vec![base_deposit_receipt_2718()];
        let (receipts_root, mut preimages) = trie_root_and_preimages(&receipts);
        let header = Header { receipts_root, number: 42, timestamp: 1, ..Default::default() };
        let (hash, encoded_header) = header_preimage(&header);
        preimages.push((PreimageKey::new_keccak256(*hash), encoded_header));
        let oracle = Arc::new(MockOracle::new(preimages));
        let mut provider = OracleL1ChainProvider::new(hash, oracle, L1TxFormat::Base);

        let receipts = provider.receipts_by_hash(hash).await.unwrap();

        assert_eq!(receipts.len(), 1);
        assert_eq!(receipts[0].cumulative_gas_used, 100);
    }

    fn base_eip8130_tx_2718() -> Vec<u8> {
        let tx = TxEip8130 {
            chain_id: 8453,
            sender: Some(Address::repeat_byte(0x11)),
            nonce_key: U256::ZERO,
            nonce_sequence: 1,
            valid_after: 0,
            valid_before: 0,
            max_priority_fee_per_gas: 1_000_000_000,
            max_fee_per_gas: 5_000_000_000,
            gas_limit: 100_000,
            account_changes: vec![],
            calls: vec![],
            metadata: Bytes::new(),
            payer: None,
        };
        let signed = Eip8130Signed::new(tx, Bytes::from_static(&[0xab; 32]), Bytes::new());
        BaseTxEnvelope::Eip8130(signed).encoded_2718()
    }

    fn base_eip8130_receipt_2718() -> Vec<u8> {
        BaseReceiptEnvelope::from_parts(true, 200, vec![], OpTxType::Eip8130, None, None)
            .encoded_2718()
    }

    fn eip1559_tx_2718() -> Vec<u8> {
        let tx = TxEip1559 {
            chain_id: 8453,
            nonce: 5,
            gas_limit: 21000,
            to: TxKind::Call(Address::repeat_byte(0x88)),
            max_fee_per_gas: 5_000_000_000,
            max_priority_fee_per_gas: 1_000_000_000,
            ..Default::default()
        };
        let sig = Signature::new(U256::from(1u64), U256::from(2u64), false);
        let signed = Signed::new_unchecked(tx, sig, B256::ZERO);
        BaseTxEnvelope::Eip1559(signed).encoded_2718()
    }

    fn eip1559_receipt_2718() -> Vec<u8> {
        BaseReceiptEnvelope::from_parts(true, 300, vec![], OpTxType::Eip1559, None, None)
            .encoded_2718()
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn base_format_block_decodes_and_drops_eip8130() {
        let txs = vec![base_deposit_tx_2718(), base_eip8130_tx_2718()];
        let (transactions_root, mut preimages) = trie_root_and_preimages(&txs);
        let header = Header { transactions_root, number: 42, timestamp: 1, ..Default::default() };
        let (hash, encoded_header) = header_preimage(&header);
        preimages.push((PreimageKey::new_keccak256(*hash), encoded_header));
        let oracle = Arc::new(MockOracle::new(preimages));
        let mut provider = OracleL1ChainProvider::new(hash, oracle, L1TxFormat::Base);

        let (info, txs) = provider.block_info_and_transactions_by_hash(hash).await.unwrap();

        assert_eq!(info.number, 42);
        assert!(txs.is_empty(), "both deposit and EIP-8130 are dropped during down-conversion");
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn base_format_receipts_decode_and_preserve_eip8130() {
        let receipts = vec![base_deposit_receipt_2718(), base_eip8130_receipt_2718()];
        let (receipts_root, mut preimages) = trie_root_and_preimages(&receipts);
        let header = Header { receipts_root, number: 42, timestamp: 1, ..Default::default() };
        let (hash, encoded_header) = header_preimage(&header);
        preimages.push((PreimageKey::new_keccak256(*hash), encoded_header));
        let oracle = Arc::new(MockOracle::new(preimages));
        let mut provider = OracleL1ChainProvider::new(hash, oracle, L1TxFormat::Base);

        let receipts = provider.receipts_by_hash(hash).await.unwrap();

        assert_eq!(receipts.len(), 2);
        assert_eq!(receipts[0].cumulative_gas_used, 100);
        assert_eq!(receipts[1].cumulative_gas_used, 200);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn base_format_mixed_block_preserves_standard_txs() {
        let txs = vec![base_deposit_tx_2718(), eip1559_tx_2718(), base_eip8130_tx_2718()];
        let (transactions_root, mut preimages) = trie_root_and_preimages(&txs);
        let header = Header { transactions_root, number: 42, timestamp: 1, ..Default::default() };
        let (hash, encoded_header) = header_preimage(&header);
        preimages.push((PreimageKey::new_keccak256(*hash), encoded_header));
        let oracle = Arc::new(MockOracle::new(preimages));
        let mut provider = OracleL1ChainProvider::new(hash, oracle, L1TxFormat::Base);

        let (info, txs) = provider.block_info_and_transactions_by_hash(hash).await.unwrap();

        assert_eq!(info.number, 42);
        assert_eq!(txs.len(), 1, "only the EIP-1559 tx should survive down-conversion");
        assert!(matches!(txs[0], TxEnvelope::Eip1559(_)), "surviving tx must be EIP-1559");
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn base_format_mixed_receipts_preserve_all() {
        let receipts =
            vec![base_deposit_receipt_2718(), eip1559_receipt_2718(), base_eip8130_receipt_2718()];
        let (receipts_root, mut preimages) = trie_root_and_preimages(&receipts);
        let header = Header { receipts_root, number: 42, timestamp: 1, ..Default::default() };
        let (hash, encoded_header) = header_preimage(&header);
        preimages.push((PreimageKey::new_keccak256(*hash), encoded_header));
        let oracle = Arc::new(MockOracle::new(preimages));
        let mut provider = OracleL1ChainProvider::new(hash, oracle, L1TxFormat::Base);

        let receipts = provider.receipts_by_hash(hash).await.unwrap();

        assert_eq!(receipts.len(), 3);
        assert_eq!(receipts[0].cumulative_gas_used, 100);
        assert_eq!(receipts[1].cumulative_gas_used, 300);
        assert_eq!(receipts[2].cumulative_gas_used, 200);
    }
}
