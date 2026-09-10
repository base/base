use base_common_types_chain::{
    BaseTxEnvelope, BlockBody, BlockBodyExt as BlockBodyTrait, BlockHeader, RecoveredBlock,
    SealedHeader, transaction::Recovered,
};
use base_common_types_rpc::{Block, BlockTransactions, BlockTransactionsKind, TransactionInfo};

/// Builds RPC block responses from execution blocks with recovered senders.
#[derive(Debug)]
pub struct RpcBlockConverter;

impl RpcBlockConverter {
    /// Converts the block into an RPC [`Block`] with the given [`BlockTransactionsKind`].
    ///
    /// The `converter` closure transforms each transaction into the desired response
    /// type.
    ///
    /// `header_builder` transforms the block header into RPC representation. It takes the
    /// consensus header and RLP length of the block which is a common dependency of RPC
    /// headers.
    pub fn into_rpc_block<T, RpcH, F, E>(
        block: RecoveredBlock,
        kind: BlockTransactionsKind,
        converter: F,
        header_builder: impl FnOnce(SealedHeader, usize) -> Result<RpcH, E>,
    ) -> Result<Block<T, RpcH>, E>
    where
        F: Fn(Recovered<BaseTxEnvelope>, TransactionInfo) -> Result<T, E>,
    {
        match kind {
            BlockTransactionsKind::Hashes => {
                Self::into_rpc_block_with_tx_hashes(block, header_builder)
            }
            BlockTransactionsKind::Full => {
                Self::into_rpc_block_full(block, converter, header_builder)
            }
        }
    }

    /// Converts the block to an RPC [`Block`] without consuming the block.
    ///
    /// For transaction hashes, only necessary parts are cloned for efficiency.
    /// For full transactions, the entire block is cloned.
    ///
    /// The `converter` closure transforms each transaction into the desired response
    /// type.
    ///
    /// `header_builder` transforms the block header into RPC representation. It takes the
    /// consensus header and RLP length of the block which is a common dependency of RPC
    /// headers.
    pub fn clone_into_rpc_block<T, RpcH, F, E>(
        block: &RecoveredBlock,
        kind: BlockTransactionsKind,
        converter: F,
        header_builder: impl FnOnce(SealedHeader, usize) -> Result<RpcH, E>,
    ) -> Result<Block<T, RpcH>, E>
    where
        F: Fn(Recovered<BaseTxEnvelope>, TransactionInfo) -> Result<T, E>,
    {
        match kind {
            BlockTransactionsKind::Hashes => {
                Self::to_rpc_block_with_tx_hashes(block, header_builder)
            }
            BlockTransactionsKind::Full => {
                Self::into_rpc_block_full(block.clone(), converter, header_builder)
            }
        }
    }

    /// Creates an RPC [`Block`] with transaction hashes from a reference.
    ///
    /// Returns [`BlockTransactions::Hashes`] containing only transaction hashes.
    /// Efficiently clones only necessary parts, not the entire block.
    pub fn to_rpc_block_with_tx_hashes<T, RpcH, E>(
        block: &RecoveredBlock,
        header_builder: impl FnOnce(SealedHeader, usize) -> Result<RpcH, E>,
    ) -> Result<Block<T, RpcH>, E> {
        let transactions = block.body().transaction_hashes_iter().copied().collect();
        let rlp_length = block.rlp_length();
        let header = block.clone_sealed_header();
        let withdrawals = block.body().withdrawals().cloned();

        let transactions = BlockTransactions::Hashes(transactions);
        let uncles = block.body().ommers().unwrap_or(&[]).iter().map(|h| h.hash_slow()).collect();
        let header = header_builder(header, rlp_length)?;

        Ok(Block { header, uncles, transactions, withdrawals })
    }

    /// Converts the block into an RPC [`Block`] with transaction hashes.
    ///
    /// Consumes the block and returns [`BlockTransactions::Hashes`] containing only transaction
    /// hashes.
    pub fn into_rpc_block_with_tx_hashes<T, E, RpcHeader>(
        block: RecoveredBlock,
        f: impl FnOnce(SealedHeader, usize) -> Result<RpcHeader, E>,
    ) -> Result<Block<T, RpcHeader>, E> {
        let transactions = block.body().transaction_hashes_iter().copied().collect();
        let rlp_length = block.rlp_length();
        let (header, body) = block.into_sealed_block().split_sealed_header_body();
        let BlockBody { ommers, withdrawals, .. } = body.into_ethereum_body();

        let transactions = BlockTransactions::Hashes(transactions);
        let uncles = ommers.into_iter().map(|h| h.hash_slow()).collect();
        let header = f(header, rlp_length)?;

        Ok(Block { header, uncles, transactions, withdrawals })
    }

    /// Converts the block into an RPC [`Block`] with full transaction objects.
    ///
    /// Returns [`BlockTransactions::Full`] with complete transaction data.
    /// The `converter` closure transforms each transaction with its metadata.
    pub fn into_rpc_block_full<T, RpcHeader, F, E>(
        block: RecoveredBlock,
        converter: F,
        header_builder: impl FnOnce(SealedHeader, usize) -> Result<RpcHeader, E>,
    ) -> Result<Block<T, RpcHeader>, E>
    where
        F: Fn(Recovered<BaseTxEnvelope>, TransactionInfo) -> Result<T, E>,
    {
        let block_number = block.header().number();
        let base_fee = block.header().base_fee_per_gas();
        let block_length = block.rlp_length();
        let block_hash = Some(block.hash());
        let block_timestamp = block.header().timestamp();

        let (block, senders) = block.split_sealed();
        let (header, body) = block.split_sealed_header_body();
        let BlockBody { transactions, ommers, withdrawals } = body.into_ethereum_body();

        let transactions = transactions
            .into_iter()
            .zip(senders)
            .enumerate()
            .map(|(idx, (tx, sender))| {
                #[allow(clippy::needless_update)]
                let tx_info = TransactionInfo {
                    hash: Some(tx.tx_hash()),
                    block_hash,
                    block_number: Some(block_number),
                    block_timestamp: Some(block_timestamp),
                    base_fee,
                    index: Some(idx as u64),
                };

                converter(Recovered::new_unchecked(tx, sender), tx_info)
            })
            .collect::<Result<Vec<_>, E>>()?;

        let transactions = BlockTransactions::Full(transactions);
        let uncles = ommers.into_iter().map(|h| h.hash_slow()).collect();
        let header = header_builder(header, block_length)?;

        let block = Block { header, uncles, transactions, withdrawals };

        Ok(block)
    }
}

#[cfg(test)]
mod tests {
    use core::convert::Infallible;

    use alloy_primitives::{Address, B256, Signature, U256};
    use base_common_types_chain::{BaseBlock, Header, Signed, TxLegacy};

    use super::*;

    fn block() -> RecoveredBlock {
        let transactions = (1..=2)
            .map(|nonce| {
                BaseTxEnvelope::Legacy(Signed::new_unchecked(
                    TxLegacy { nonce, ..Default::default() },
                    Signature::new(U256::from(1), U256::from(2), false),
                    B256::repeat_byte(nonce as u8),
                ))
            })
            .collect();
        RecoveredBlock::new_unhashed(
            BaseBlock {
                header: Header {
                    number: 42,
                    timestamp: 420,
                    base_fee_per_gas: Some(100),
                    ..Default::default()
                },
                body: BlockBody { transactions, ..Default::default() },
            },
            vec![Address::repeat_byte(1), Address::repeat_byte(2)],
        )
    }

    #[test]
    fn hash_responses_preserve_order_without_converting_transactions() {
        let block = block();
        let convert = |_, _| -> Result<(), Infallible> {
            panic!("hash-only responses must not convert transactions")
        };
        let borrowed = RpcBlockConverter::clone_into_rpc_block(
            &block,
            BlockTransactionsKind::Hashes,
            convert,
            |header, _| Ok(header),
        )
        .unwrap();
        let owned = RpcBlockConverter::into_rpc_block(
            block,
            BlockTransactionsKind::Hashes,
            convert,
            |header, _| Ok(header),
        )
        .unwrap();
        assert_eq!(borrowed, owned);
        assert_eq!(
            owned.transactions,
            BlockTransactions::Hashes(vec![B256::repeat_byte(1), B256::repeat_byte(2)])
        );
    }

    #[test]
    fn full_responses_include_recovered_senders_and_block_metadata() {
        let block = block();
        let hash = block.hash();
        let convert =
            |tx: Recovered<BaseTxEnvelope>, info| Ok::<_, Infallible>((tx.signer(), info));
        let borrowed = RpcBlockConverter::clone_into_rpc_block(
            &block,
            BlockTransactionsKind::Full,
            convert,
            |header, _| Ok(header),
        )
        .unwrap();
        let owned = RpcBlockConverter::into_rpc_block(
            block,
            BlockTransactionsKind::Full,
            convert,
            |header, _| Ok(header),
        )
        .unwrap();
        assert_eq!(borrowed, owned);
        let BlockTransactions::Full(transactions) = owned.transactions else {
            panic!("expected full transactions")
        };
        assert_eq!(transactions.len(), 2);
        for (index, (sender, info)) in transactions.into_iter().enumerate() {
            assert_eq!(sender, Address::repeat_byte(index as u8 + 1));
            assert_eq!(info.hash, Some(B256::repeat_byte(index as u8 + 1)));
            assert_eq!(info.index, Some(index as u64));
            assert_eq!(info.block_hash, Some(hash));
            assert_eq!(info.block_number, Some(42));
            assert_eq!(info.block_timestamp, Some(420));
            assert_eq!(info.base_fee, Some(100));
        }
    }
}
