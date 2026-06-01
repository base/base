//! Reth compatibility implementations for Base consensus types.
//!
//! This module provides implementations of reth traits gated behind the `reth` feature flag,
//! including `Compact`, `Envelope`, `ToTxCompact`, `FromTxCompact`, `Compress`, and
//! `Decompress`.

use alloc::{borrow::Cow, vec::Vec};

use alloy_consensus::{
    Header, Receipt, Sealed, Signed, TxEip1559, TxEip2930, TxEip7702, TxLegacy, TxReceipt,
    constants::EIP7702_TX_TYPE_ID,
};
use alloy_primitives::{Address, B256, BlockNumber, Bloom, Bytes, Signature, TxKind, U256};
use bytes::{Buf, BufMut};
use reth_codecs::{
    Compact, CompactZstd, DecompressError,
    txtype::{
        COMPACT_EXTENDED_IDENTIFIER_FLAG, COMPACT_IDENTIFIER_EIP1559, COMPACT_IDENTIFIER_EIP2930,
        COMPACT_IDENTIFIER_LEGACY,
    },
};

use crate::{
    BaseBlock, BaseHeader, BaseReceipt, BaseTxEnvelope, BaseTypedTransaction, DEPOSIT_TX_TYPE_ID,
    DepositReceipt, EIP8130_TX_TYPE_ID, OpTxType, TxDeposit,
};

// ---------------------------------------------------------------------------
// Compact – TxDeposit
// ---------------------------------------------------------------------------

/// Helper struct for deriving `Compact` on deposit transactions.
///
/// 1:1 with [`TxDeposit`] but uses `Option<u128>` for `mint` so the bitflag
/// encoding can omit the zero case.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Default, Compact)]
#[reth_codecs(crate = "reth_codecs")]
pub struct CompactTxDeposit {
    /// Hash that uniquely identifies the source of the deposit.
    pub source_hash: B256,
    /// The address of the sender account.
    pub from: Address,
    /// The recipient or contract creation target.
    pub to: TxKind,
    /// The ETH value to mint on L2.
    pub mint: Option<u128>,
    /// The ETH value to send.
    pub value: U256,
    /// The gas limit for the L2 transaction.
    pub gas_limit: u64,
    /// Whether this transaction is exempt from the L2 gas limit.
    pub is_system_transaction: bool,
    /// Calldata.
    pub input: Bytes,
}

impl Compact for TxDeposit {
    fn to_compact<B>(&self, buf: &mut B) -> usize
    where
        B: BufMut + AsMut<[u8]>,
    {
        let tx = CompactTxDeposit {
            source_hash: self.source_hash,
            from: self.from,
            to: self.to,
            mint: match self.mint {
                0 => None,
                v => Some(v),
            },
            value: self.value,
            gas_limit: self.gas_limit,
            is_system_transaction: self.is_system_transaction,
            input: self.input.clone(),
        };
        tx.to_compact(buf)
    }

    fn from_compact(buf: &[u8], len: usize) -> (Self, &[u8]) {
        let (tx, remaining) = CompactTxDeposit::from_compact(buf, len);
        let alloy_tx = Self {
            source_hash: tx.source_hash,
            from: tx.from,
            to: tx.to,
            mint: tx.mint.unwrap_or_default(),
            value: tx.value,
            gas_limit: tx.gas_limit,
            is_system_transaction: tx.is_system_transaction,
            input: tx.input,
        };
        (alloy_tx, remaining)
    }
}

// ---------------------------------------------------------------------------
// Compact – OpTxType
// ---------------------------------------------------------------------------

impl Compact for OpTxType {
    fn to_compact<B>(&self, buf: &mut B) -> usize
    where
        B: BufMut + AsMut<[u8]>,
    {
        match self {
            Self::Legacy => COMPACT_IDENTIFIER_LEGACY,
            Self::Eip2930 => COMPACT_IDENTIFIER_EIP2930,
            Self::Eip1559 => COMPACT_IDENTIFIER_EIP1559,
            Self::Eip7702 => {
                buf.put_u8(EIP7702_TX_TYPE_ID);
                COMPACT_EXTENDED_IDENTIFIER_FLAG
            }
            Self::Deposit => {
                buf.put_u8(DEPOSIT_TX_TYPE_ID);
                COMPACT_EXTENDED_IDENTIFIER_FLAG
            }
            Self::Eip8130 => {
                buf.put_u8(EIP8130_TX_TYPE_ID);
                COMPACT_EXTENDED_IDENTIFIER_FLAG
            }
        }
    }

    fn from_compact(mut buf: &[u8], identifier: usize) -> (Self, &[u8]) {
        (
            match identifier {
                COMPACT_IDENTIFIER_LEGACY => Self::Legacy,
                COMPACT_IDENTIFIER_EIP2930 => Self::Eip2930,
                COMPACT_IDENTIFIER_EIP1559 => Self::Eip1559,
                COMPACT_EXTENDED_IDENTIFIER_FLAG => {
                    let extended_identifier = buf.get_u8();
                    match extended_identifier {
                        EIP7702_TX_TYPE_ID => Self::Eip7702,
                        DEPOSIT_TX_TYPE_ID => Self::Deposit,
                        EIP8130_TX_TYPE_ID => Self::Eip8130,
                        _ => panic!("Unsupported OpTxType identifier: {extended_identifier}"),
                    }
                }
                _ => panic!("Unknown identifier for OpTxType: {identifier}"),
            },
            buf,
        )
    }
}

// ---------------------------------------------------------------------------
// Compact – BaseTypedTransaction
// ---------------------------------------------------------------------------

impl Compact for BaseTypedTransaction {
    fn to_compact<B>(&self, out: &mut B) -> usize
    where
        B: BufMut + AsMut<[u8]>,
    {
        let identifier = self.tx_type().to_compact(out);
        match self {
            Self::Legacy(tx) => tx.to_compact(out),
            Self::Eip2930(tx) => tx.to_compact(out),
            Self::Eip1559(tx) => tx.to_compact(out),
            Self::Eip7702(tx) => tx.to_compact(out),
            Self::Deposit(tx) => tx.to_compact(out),
            Self::Eip8130(_) => unimplemented!(
                "Compact encoding for EIP-8130 BaseTypedTransaction is not yet implemented"
            ),
        };
        identifier
    }

    fn from_compact(buf: &[u8], identifier: usize) -> (Self, &[u8]) {
        let (tx_type, buf) = OpTxType::from_compact(buf, identifier);
        match tx_type {
            OpTxType::Legacy => {
                let (tx, buf) = Compact::from_compact(buf, buf.len());
                (Self::Legacy(tx), buf)
            }
            OpTxType::Eip2930 => {
                let (tx, buf) = Compact::from_compact(buf, buf.len());
                (Self::Eip2930(tx), buf)
            }
            OpTxType::Eip1559 => {
                let (tx, buf) = Compact::from_compact(buf, buf.len());
                (Self::Eip1559(tx), buf)
            }
            OpTxType::Eip7702 => {
                let (tx, buf) = Compact::from_compact(buf, buf.len());
                (Self::Eip7702(tx), buf)
            }
            OpTxType::Deposit => {
                let (tx, buf) = Compact::from_compact(buf, buf.len());
                (Self::Deposit(tx), buf)
            }
            OpTxType::Eip8130 => unimplemented!(
                "Compact decoding for EIP-8130 BaseTypedTransaction is not yet implemented"
            ),
        }
    }
}

// ---------------------------------------------------------------------------
// ToTxCompact / FromTxCompact – BaseTxEnvelope
// ---------------------------------------------------------------------------

impl reth_codecs::alloy::transaction::ToTxCompact for BaseTxEnvelope {
    fn to_tx_compact(&self, buf: &mut (impl BufMut + AsMut<[u8]>)) {
        match self {
            Self::Legacy(tx) => tx.tx().to_compact(buf),
            Self::Eip2930(tx) => tx.tx().to_compact(buf),
            Self::Eip1559(tx) => tx.tx().to_compact(buf),
            Self::Eip7702(tx) => tx.tx().to_compact(buf),
            Self::Deposit(tx) => tx.to_compact(buf),
            Self::Eip8130(_) => unimplemented!(
                "Compact encoding for EIP-8130 BaseTxEnvelope is not yet implemented"
            ),
        };
    }
}

impl reth_codecs::alloy::transaction::FromTxCompact for BaseTxEnvelope {
    type TxType = OpTxType;

    fn from_tx_compact(buf: &[u8], tx_type: OpTxType, signature: Signature) -> (Self, &[u8]) {
        match tx_type {
            OpTxType::Legacy => {
                let (tx, buf) = TxLegacy::from_compact(buf, buf.len());
                let tx = Signed::new_unhashed(tx, signature);
                (Self::Legacy(tx), buf)
            }
            OpTxType::Eip2930 => {
                let (tx, buf) = TxEip2930::from_compact(buf, buf.len());
                let tx = Signed::new_unhashed(tx, signature);
                (Self::Eip2930(tx), buf)
            }
            OpTxType::Eip1559 => {
                let (tx, buf) = TxEip1559::from_compact(buf, buf.len());
                let tx = Signed::new_unhashed(tx, signature);
                (Self::Eip1559(tx), buf)
            }
            OpTxType::Eip7702 => {
                let (tx, buf) = TxEip7702::from_compact(buf, buf.len());
                let tx = Signed::new_unhashed(tx, signature);
                (Self::Eip7702(tx), buf)
            }
            OpTxType::Deposit => {
                let (tx, buf) = TxDeposit::from_compact(buf, buf.len());
                let tx = Sealed::new(tx);
                (Self::Deposit(tx), buf)
            }
            OpTxType::Eip8130 => unimplemented!(
                "Compact decoding for EIP-8130 BaseTxEnvelope is not yet implemented"
            ),
        }
    }
}

// ---------------------------------------------------------------------------
// Envelope – BaseTxEnvelope
// ---------------------------------------------------------------------------

/// Deposit signature placeholder (all zeros).
const DEPOSIT_SIGNATURE: Signature = Signature::new(U256::ZERO, U256::ZERO, false);

impl reth_codecs::alloy::transaction::Envelope for BaseTxEnvelope {
    fn signature(&self) -> &Signature {
        match self {
            Self::Legacy(tx) => tx.signature(),
            Self::Eip2930(tx) => tx.signature(),
            Self::Eip1559(tx) => tx.signature(),
            Self::Eip7702(tx) => tx.signature(),
            // The `Envelope` trait forces a `&Signature` return, so neither variant can
            // signal absence the way `BaseTxEnvelope::signature` (which returns `Option`)
            // does. Both Deposit and EIP-8130 AA transactions carry their own auth model
            // and have no meaningful ECDSA signature: callers MUST NOT feed this value
            // into ECDSA recovery — it is an all-zero placeholder.
            Self::Deposit(_) | Self::Eip8130(_) => &DEPOSIT_SIGNATURE,
        }
    }

    fn tx_type(&self) -> Self::TxType {
        Self::tx_type(self)
    }
}

// ---------------------------------------------------------------------------
// Compact – BaseTxEnvelope (via CompactEnvelope)
// ---------------------------------------------------------------------------

impl Compact for BaseTxEnvelope {
    fn to_compact<B>(&self, buf: &mut B) -> usize
    where
        B: BufMut + AsMut<[u8]>,
    {
        reth_codecs::alloy::transaction::CompactEnvelope::to_compact(self, buf)
    }

    fn from_compact(buf: &[u8], len: usize) -> (Self, &[u8]) {
        reth_codecs::alloy::transaction::CompactEnvelope::from_compact(buf, len)
    }
}

// ---------------------------------------------------------------------------
// Compact – BaseReceipt (via CompactZstd helper)
// ---------------------------------------------------------------------------

#[derive(CompactZstd)]
#[reth_codecs(crate = "reth_codecs")]
#[reth_zstd(
    compressor = reth_zstd_compressors::with_receipt_compressor,
    decompressor = reth_zstd_compressors::with_receipt_decompressor
)]
struct CompactBaseReceipt<'a> {
    tx_type: OpTxType,
    success: bool,
    cumulative_gas_used: u64,
    #[expect(clippy::owned_cow)]
    logs: Cow<'a, Vec<alloy_primitives::Log>>,
    deposit_nonce: Option<u64>,
    deposit_receipt_version: Option<u64>,
}

impl<'a> From<&'a BaseReceipt> for CompactBaseReceipt<'a> {
    fn from(receipt: &'a BaseReceipt) -> Self {
        Self {
            success: receipt.status(),
            cumulative_gas_used: receipt.cumulative_gas_used(),
            logs: Cow::Borrowed(&receipt.as_receipt().logs),
            deposit_nonce: if let BaseReceipt::Deposit(receipt) = receipt {
                receipt.deposit_nonce
            } else {
                None
            },
            deposit_receipt_version: if let BaseReceipt::Deposit(receipt) = receipt {
                receipt.deposit_receipt_version
            } else {
                None
            },
            tx_type: receipt.tx_type(),
        }
    }
}

impl From<CompactBaseReceipt<'_>> for BaseReceipt {
    fn from(receipt: CompactBaseReceipt<'_>) -> Self {
        let CompactBaseReceipt {
            tx_type,
            success,
            cumulative_gas_used,
            logs,
            deposit_nonce,
            deposit_receipt_version,
        } = receipt;

        let inner =
            Receipt { status: success.into(), cumulative_gas_used, logs: logs.into_owned() };

        match tx_type {
            OpTxType::Legacy => Self::Legacy(inner),
            OpTxType::Eip2930 => Self::Eip2930(inner),
            OpTxType::Eip1559 => Self::Eip1559(inner),
            OpTxType::Eip7702 => Self::Eip7702(inner),
            OpTxType::Deposit => {
                Self::Deposit(DepositReceipt { inner, deposit_nonce, deposit_receipt_version })
            }
            OpTxType::Eip8130 => {
                unimplemented!("Compact decoding for EIP-8130 BaseReceipt is not yet implemented")
            }
        }
    }
}

impl Compact for BaseReceipt {
    fn to_compact<B>(&self, buf: &mut B) -> usize
    where
        B: BufMut + AsMut<[u8]>,
    {
        CompactBaseReceipt::from(self).to_compact(buf)
    }

    fn from_compact(buf: &[u8], len: usize) -> (Self, &[u8]) {
        let (receipt, buf) = CompactBaseReceipt::from_compact(buf, len);
        (receipt.into(), buf)
    }
}

// ---------------------------------------------------------------------------
// Compress / Decompress (reth-db-api)
// ---------------------------------------------------------------------------

impl reth_db_api::table::Compress for BaseTxEnvelope {
    type Compressed = Vec<u8>;

    fn compress_to_buf<B: BufMut + AsMut<[u8]>>(&self, buf: &mut B) {
        let _ = Compact::to_compact(self, buf);
    }
}

impl reth_db_api::table::Decompress for BaseTxEnvelope {
    fn decompress(value: &[u8]) -> Result<Self, DecompressError> {
        let (obj, _) = Compact::from_compact(value, value.len());
        Ok(obj)
    }
}

impl reth_db_api::table::Compress for BaseReceipt {
    type Compressed = Vec<u8>;

    fn compress_to_buf<B: BufMut + AsMut<[u8]>>(&self, buf: &mut B) {
        let _ = Compact::to_compact(self, buf);
    }
}

impl reth_db_api::table::Decompress for BaseReceipt {
    fn decompress(value: &[u8]) -> Result<Self, DecompressError> {
        let (obj, _) = Compact::from_compact(value, value.len());
        Ok(obj)
    }
}

// ---------------------------------------------------------------------------
// DepositReceiptExt trait
// ---------------------------------------------------------------------------

/// Trait for accessing deposit receipt fields on a [`reth_primitives_traits::Receipt`].
pub trait DepositReceiptExt: reth_primitives_traits::Receipt {
    /// Returns a mutable reference to the inner deposit receipt, if this is a deposit.
    fn as_deposit_receipt_mut(&mut self) -> Option<&mut DepositReceipt>;

    /// Returns a reference to the inner deposit receipt, if this is a deposit.
    fn as_deposit_receipt(&self) -> Option<&DepositReceipt>;
}

impl DepositReceiptExt for BaseReceipt {
    fn as_deposit_receipt_mut(&mut self) -> Option<&mut DepositReceipt> {
        match self {
            Self::Deposit(receipt) => Some(receipt),
            _ => None,
        }
    }

    fn as_deposit_receipt(&self) -> Option<&DepositReceipt> {
        match self {
            Self::Deposit(receipt) => Some(receipt),
            _ => None,
        }
    }
}

// ---------------------------------------------------------------------------
// Compact – BaseHeader
// ---------------------------------------------------------------------------

/// Helper struct used to derive [`Compact`] for [`BaseHeader`].
///
/// Notice: field layout is byte-compatible with the upstream reth helper used for
/// [`alloy_consensus::Header`]. The only Base-specific addition lives in [`BaseHeaderExt`].
#[derive(Debug, Clone, PartialEq, Eq, Hash, Default, Compact)]
#[reth_codecs(crate = "reth_codecs")]
pub struct BaseHeaderCompact {
    /// Parent block hash.
    pub parent_hash: B256,
    /// Ommers list hash.
    pub ommers_hash: B256,
    /// Block proposer / fee recipient.
    pub beneficiary: Address,
    /// State root after applying the block's transactions.
    pub state_root: B256,
    /// Transactions Merkle Patricia trie root.
    pub transactions_root: B256,
    /// Receipts trie root.
    pub receipts_root: B256,
    /// Withdrawals root (Shanghai).
    pub withdrawals_root: Option<B256>,
    /// Bloom filter for the block's logs.
    pub logs_bloom: Bloom,
    /// Block difficulty (Pre-PoS only).
    pub difficulty: U256,
    /// Block number.
    pub number: BlockNumber,
    /// Block gas limit.
    pub gas_limit: u64,
    /// Block gas used.
    pub gas_used: u64,
    /// Unix seconds timestamp.
    pub timestamp: u64,
    /// Post-PoS mixHash field.
    pub mix_hash: B256,
    /// Block nonce.
    pub nonce: u64,
    /// EIP-1559 base fee per gas.
    pub base_fee_per_gas: Option<u64>,
    /// EIP-4844 blob gas used.
    pub blob_gas_used: Option<u64>,
    /// EIP-4844 excess blob gas.
    pub excess_blob_gas: Option<u64>,
    /// EIP-4788 parent beacon block root.
    pub parent_beacon_block_root: Option<B256>,
    /// Optional newly-added header fields, including the Base millisecond trailer.
    pub extra_fields: Option<BaseHeaderExt>,
    /// Free-form extra data.
    pub extra_data: Bytes,
}

/// Optional extension fields appended to [`BaseHeaderCompact`].
///
/// New fields must always be `Option<T>` and appended at the end of this struct so older
/// compact-encoded headers continue to decode.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Default, Compact)]
#[reth_codecs(crate = "reth_codecs")]
pub struct BaseHeaderExt {
    /// EIP-7685 execution requests hash.
    pub requests_hash: Option<B256>,
    /// EIP-7928 block access list hash.
    pub block_access_list_hash: Option<B256>,
    /// EIP-7843 slot number.
    pub slot_number: Option<u64>,
    /// Post-Beryl millisecond subsecond component (Base-specific).
    ///
    /// Stored as `u64` because reth-codecs only implements [`Compact`] for `u8`/`u64`/`u128`;
    /// the value is always in the range `0..1000` and is converted to/from `u16` at the
    /// [`BaseHeader`] boundary.
    pub timestamp_millis_part: Option<u64>,
}

impl BaseHeaderExt {
    /// Returns `Some(self)` if any field is populated, `None` otherwise.
    ///
    /// Used to keep the `extra_fields` bucket absent (and therefore byte-compatible with
    /// pre-extension stored headers) when no extension data is present.
    pub const fn into_option(self) -> Option<Self> {
        if self.requests_hash.is_some()
            || self.block_access_list_hash.is_some()
            || self.slot_number.is_some()
            || self.timestamp_millis_part.is_some()
        {
            Some(self)
        } else {
            None
        }
    }
}

impl Compact for BaseHeader {
    fn to_compact<B>(&self, buf: &mut B) -> usize
    where
        B: BufMut + AsMut<[u8]>,
    {
        let extra_fields = BaseHeaderExt {
            requests_hash: self.inner.requests_hash,
            block_access_list_hash: self.inner.block_access_list_hash,
            slot_number: self.inner.slot_number,
            timestamp_millis_part: self.timestamp_millis_part.map(u64::from),
        };

        let helper = BaseHeaderCompact {
            parent_hash: self.inner.parent_hash,
            ommers_hash: self.inner.ommers_hash,
            beneficiary: self.inner.beneficiary,
            state_root: self.inner.state_root,
            transactions_root: self.inner.transactions_root,
            receipts_root: self.inner.receipts_root,
            withdrawals_root: self.inner.withdrawals_root,
            logs_bloom: self.inner.logs_bloom,
            difficulty: self.inner.difficulty,
            number: self.inner.number,
            gas_limit: self.inner.gas_limit,
            gas_used: self.inner.gas_used,
            timestamp: self.inner.timestamp,
            mix_hash: self.inner.mix_hash,
            nonce: self.inner.nonce.into(),
            base_fee_per_gas: self.inner.base_fee_per_gas,
            blob_gas_used: self.inner.blob_gas_used,
            excess_blob_gas: self.inner.excess_blob_gas,
            parent_beacon_block_root: self.inner.parent_beacon_block_root,
            extra_fields: extra_fields.into_option(),
            extra_data: self.inner.extra_data.clone(),
        };
        helper.to_compact(buf)
    }

    fn from_compact(buf: &[u8], len: usize) -> (Self, &[u8]) {
        let (helper, buf) = BaseHeaderCompact::from_compact(buf, len);
        let timestamp_millis_part = helper
            .extra_fields
            .as_ref()
            .and_then(|h| h.timestamp_millis_part)
            .map(|v| u16::try_from(v).unwrap_or(0));
        let inner = Header {
            parent_hash: helper.parent_hash,
            ommers_hash: helper.ommers_hash,
            beneficiary: helper.beneficiary,
            state_root: helper.state_root,
            transactions_root: helper.transactions_root,
            receipts_root: helper.receipts_root,
            withdrawals_root: helper.withdrawals_root,
            logs_bloom: helper.logs_bloom,
            difficulty: helper.difficulty,
            number: helper.number,
            gas_limit: helper.gas_limit,
            gas_used: helper.gas_used,
            timestamp: helper.timestamp,
            mix_hash: helper.mix_hash,
            nonce: helper.nonce.into(),
            base_fee_per_gas: helper.base_fee_per_gas,
            blob_gas_used: helper.blob_gas_used,
            excess_blob_gas: helper.excess_blob_gas,
            parent_beacon_block_root: helper.parent_beacon_block_root,
            requests_hash: helper.extra_fields.as_ref().and_then(|h| h.requests_hash),
            block_access_list_hash: helper
                .extra_fields
                .as_ref()
                .and_then(|h| h.block_access_list_hash),
            slot_number: helper.extra_fields.as_ref().and_then(|h| h.slot_number),
            extra_data: helper.extra_data,
        };
        (Self { inner, timestamp_millis_part }, buf)
    }
}

// ---------------------------------------------------------------------------
// BaseBlockBody / BasePrimitives
// ---------------------------------------------------------------------------

/// Base-specific block body type.
pub type BaseBlockBody = <BaseBlock as reth_primitives_traits::Block>::Body;

/// Primitive types for the Base node.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub struct BasePrimitives;

impl reth_primitives_traits::NodePrimitives for BasePrimitives {
    type Block = BaseBlock;
    type BlockHeader = Header;
    type BlockBody = BaseBlockBody;
    type SignedTx = BaseTxEnvelope;
    type Receipt = BaseReceipt;
}

#[cfg(test)]
mod tests {
    use alloy_consensus::Header;
    use alloy_primitives::B256;

    use super::*;
    use crate::VALID_TIMESTAMP_MILLIS_PARTS;

    fn sample_inner_header() -> Header {
        Header {
            timestamp: 1_780_334_562,
            number: 42_000,
            gas_limit: 30_000_000,
            gas_used: 1_234,
            base_fee_per_gas: Some(1_000_000_000),
            withdrawals_root: Some(B256::repeat_byte(0x11)),
            blob_gas_used: Some(0),
            excess_blob_gas: Some(0),
            parent_beacon_block_root: Some(B256::repeat_byte(0x22)),
            requests_hash: Some(B256::repeat_byte(0x33)),
            ..Default::default()
        }
    }

    #[test]
    fn base_header_compact_round_trip_with_none_millis_part() {
        let base_header = BaseHeader::from(sample_inner_header());
        let mut encoded = Vec::new();
        let len = base_header.to_compact(&mut encoded);
        assert_eq!(len, encoded.len());

        let (decoded, rest) = BaseHeader::from_compact(&encoded, len);
        assert!(rest.is_empty());
        assert_eq!(decoded, base_header);
    }

    #[test]
    fn base_header_compact_round_trip_with_some_millis_part() {
        for part in VALID_TIMESTAMP_MILLIS_PARTS {
            let base_header = BaseHeader::new(sample_inner_header(), Some(part)).unwrap();
            let mut encoded = Vec::new();
            let len = base_header.to_compact(&mut encoded);
            assert_eq!(len, encoded.len());

            let (decoded, rest) = BaseHeader::from_compact(&encoded, len);
            assert!(rest.is_empty(), "decoder must consume all bytes for part={part}");
            assert_eq!(decoded, base_header);
        }
    }

    #[test]
    fn base_header_compact_with_none_matches_alloy_header_bytes() {
        // For headers without the Base millisecond trailer, the compact encoding must be
        // byte-identical to the upstream `alloy_consensus::Header` compact encoding so
        // pre-Subsecond stored bytes continue to decode (and re-encode) unchanged.
        let inner = sample_inner_header();
        let base_header = BaseHeader::from(inner.clone());

        let mut base_encoding = Vec::new();
        let base_len = base_header.to_compact(&mut base_encoding);

        let mut alloy_encoding = Vec::new();
        let alloy_len = inner.to_compact(&mut alloy_encoding);

        assert_eq!(base_len, alloy_len);
        assert_eq!(base_encoding, alloy_encoding);
    }

    #[test]
    fn base_header_compact_decodes_alloy_header_bytes_as_none_millis_part() {
        // Old compact bytes (produced before the Base millisecond trailer existed) must
        // decode cleanly as `timestamp_millis_part = None`.
        let inner = sample_inner_header();
        let mut alloy_encoding = Vec::new();
        let alloy_len = inner.to_compact(&mut alloy_encoding);

        let (decoded, rest) = BaseHeader::from_compact(&alloy_encoding, alloy_len);
        assert!(rest.is_empty());
        assert_eq!(decoded.timestamp_millis_part, None);
        assert_eq!(decoded.inner, inner);
    }
}
