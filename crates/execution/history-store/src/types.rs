//! Block rows exchanged between the node export endpoint, the writer and segment files.

use std::panic::AssertUnwindSafe;

use alloy_consensus::Header;
use alloy_eips::eip2718::{Decodable2718, Encodable2718};
use alloy_primitives::{Address, B256, Bytes};
use alloy_rlp::{Decodable, Encodable};
use base_common_consensus::{BaseReceipt, BaseTxEnvelope};
use reth_codecs::Compact;
use reth_db_api::models::{AccountBeforeTx, StorageBeforeTx};
use serde::{Deserialize, Serialize};

use crate::HistoryStoreError;

/// Position of a block's transactions in reth's global transaction numbering.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct BodyIndices {
    /// Global transaction number of the block's first transaction.
    pub first_tx_num: u64,
    /// Number of transactions in the block.
    pub tx_count: u64,
}

impl BodyIndices {
    /// Returns the global transaction number one past the block's last transaction.
    pub const fn next_tx_num(&self) -> u64 {
        self.first_tx_num + self.tx_count
    }
}

/// One block decoded into reth's own row types, plus the hash computed from its header.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BlockData {
    /// Block hash, always computed locally from `header`.
    pub hash: B256,
    /// Block header.
    pub header: Header,
    /// Global transaction numbering of the block body.
    pub body_indices: BodyIndices,
    /// Transactions in block order.
    pub transactions: Vec<BaseTxEnvelope>,
    /// Recovered sender of each transaction.
    pub senders: Vec<Address>,
    /// Receipt of each transaction.
    pub receipts: Vec<BaseReceipt>,
    /// Account values before this block changed them.
    pub account_changes: Vec<AccountBeforeTx>,
    /// Storage values before this block changed them.
    pub storage_changes: Vec<StorageBeforeTx>,
}

/// The rows of one block besides its header, as passed to [`BlockData::new`].
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct BlockRows {
    /// Global transaction numbering of the block body.
    pub body_indices: BodyIndices,
    /// Transactions in block order.
    pub transactions: Vec<BaseTxEnvelope>,
    /// Sender of each transaction.
    pub senders: Vec<Address>,
    /// Receipt of each transaction.
    pub receipts: Vec<BaseReceipt>,
    /// Account values before this block changed them.
    pub account_changes: Vec<AccountBeforeTx>,
    /// Storage values before this block changed them.
    pub storage_changes: Vec<StorageBeforeTx>,
}

impl BlockData {
    /// Returns the block number from the header.
    pub const fn number(&self) -> u64 {
        self.header.number
    }

    /// Builds block data from a header and its rows, computing the block hash.
    pub fn new(header: Header, rows: BlockRows) -> Self {
        Self {
            hash: header.hash_slow(),
            header,
            body_indices: rows.body_indices,
            transactions: rows.transactions,
            senders: rows.senders,
            receipts: rows.receipts,
            account_changes: rows.account_changes,
            storage_changes: rows.storage_changes,
        }
    }
}

/// Wire form of one block returned by `archive_getBlockRange`.
///
/// Rows are reth's own encodings: the header is RLP, transactions are EIP-2718, and receipts and
/// changeset entries use reth's `Compact` codec (the static-file row format), which keeps deposit
/// receipt fields. The block hash is deliberately absent: the writer computes it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ArchiveBlock {
    /// Block number claimed by the node.
    pub number: u64,
    /// RLP-encoded header.
    pub header: Bytes,
    /// Global transaction numbering of the block body.
    pub body_indices: BodyIndices,
    /// EIP-2718 encoded transactions.
    pub transactions: Vec<Bytes>,
    /// Sender of each transaction as stored by the node.
    pub senders: Vec<Address>,
    /// `Compact`-encoded receipts.
    pub receipts: Vec<Bytes>,
    /// `Compact`-encoded account changeset entries.
    pub account_changes: Vec<Bytes>,
    /// `Compact`-encoded storage changeset entries.
    pub storage_changes: Vec<Bytes>,
}

impl ArchiveBlock {
    /// Encodes typed block data into its wire form.
    pub fn from_data(data: &BlockData) -> Self {
        let mut header = Vec::with_capacity(data.header.length());
        data.header.encode(&mut header);
        Self {
            number: data.number(),
            header: header.into(),
            body_indices: data.body_indices,
            transactions: data.transactions.iter().map(|tx| tx.encoded_2718().into()).collect(),
            senders: data.senders.clone(),
            receipts: data.receipts.iter().map(Self::compact_bytes).collect(),
            account_changes: data.account_changes.iter().map(Self::compact_bytes).collect(),
            storage_changes: data.storage_changes.iter().map(Self::compact_bytes).collect(),
        }
    }

    /// Decodes the wire form into typed rows and computes the block hash from the header.
    pub fn decode(&self) -> Result<BlockData, HistoryStoreError> {
        let block = self.number;
        let header = Header::decode(&mut self.header.as_ref()).map_err(|err| {
            HistoryStoreError::Decode { what: "header", block, reason: err.to_string() }
        })?;
        let transactions = self
            .transactions
            .iter()
            .map(|raw| {
                BaseTxEnvelope::decode_2718_exact(raw.as_ref()).map_err(|err| {
                    HistoryStoreError::Decode {
                        what: "transaction",
                        block,
                        reason: err.to_string(),
                    }
                })
            })
            .collect::<Result<Vec<_>, _>>()?;
        Ok(BlockData::new(
            header,
            BlockRows {
                body_indices: self.body_indices,
                transactions,
                senders: self.senders.clone(),
                receipts: Self::decode_rows(&self.receipts, "receipt", block)?,
                account_changes: Self::decode_rows(
                    &self.account_changes,
                    "account changeset",
                    block,
                )?,
                storage_changes: Self::decode_rows(
                    &self.storage_changes,
                    "storage changeset",
                    block,
                )?,
            },
        ))
    }

    /// Returns the number of payload bytes this block contributes to a response.
    pub fn payload_len(&self) -> usize {
        self.header.len()
            + self.transactions.iter().map(|b| b.len()).sum::<usize>()
            + self.senders.len() * Address::len_bytes()
            + self.receipts.iter().map(|b| b.len()).sum::<usize>()
            + self.account_changes.iter().map(|b| b.len()).sum::<usize>()
            + self.storage_changes.iter().map(|b| b.len()).sum::<usize>()
    }

    /// Encodes one row with reth's `Compact` codec.
    pub fn compact_bytes<T: Compact>(value: &T) -> Bytes {
        let mut buf = Vec::new();
        value.to_compact(&mut buf);
        buf.into()
    }

    /// Decodes rows encoded with reth's `Compact` codec.
    ///
    /// `Compact` decoders panic on malformed input, and these bytes come from an untrusted node,
    /// so a panic is caught and reported as a decode error instead of crashing the writer. Each
    /// row must re-encode to the exact bytes received, which rejects trailing garbage and
    /// non-canonical encodings (some codecs, such as the zstd-wrapped receipt, do not report
    /// unconsumed input).
    pub fn decode_rows<T: Compact>(
        rows: &[Bytes],
        what: &'static str,
        block: u64,
    ) -> Result<Vec<T>, HistoryStoreError> {
        rows.iter()
            .map(|raw| {
                let value = std::panic::catch_unwind(AssertUnwindSafe(|| {
                    T::from_compact(raw.as_ref(), raw.len()).0
                }))
                .map_err(|_| HistoryStoreError::Decode {
                    what,
                    block,
                    reason: "malformed compact row".to_string(),
                })?;
                if Self::compact_bytes(&value) != *raw {
                    return Err(HistoryStoreError::Decode {
                        what,
                        block,
                        reason: "row does not re-encode to the bytes received".to_string(),
                    });
                }
                Ok(value)
            })
            .collect()
    }
}
