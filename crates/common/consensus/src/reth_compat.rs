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
use alloy_primitives::{
    Address, B256, BlockHash, BlockNumber, Bloom, Bytes, Signature, TxKind, U256,
};
use bytes::{Buf, BufMut};
use reth_codecs::{
    Compact, CompactZstd, DecompressError,
    txtype::{
        COMPACT_EXTENDED_IDENTIFIER_FLAG, COMPACT_IDENTIFIER_EIP1559, COMPACT_IDENTIFIER_EIP2930,
        COMPACT_IDENTIFIER_LEGACY,
    },
};

use crate::{
    BaseBlock, BaseHeader, BaseHeaderFields, BaseReceipt, BaseTxEnvelope, BaseTypedTransaction,
    DEPOSIT_TX_TYPE_ID, DepositReceipt, EIP8130_TX_TYPE_ID, Eip8130Receipt, OpTxType,
    TimestampMillisPartError, TxDeposit, TxEip8130,
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
            Self::Eip8130(tx) => tx.to_compact(out),
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
            OpTxType::Eip8130 => {
                let (tx, buf) = TxEip8130::from_compact(buf, buf.len());
                (Self::Eip8130(tx), buf)
            }
        }
    }
}

// ---------------------------------------------------------------------------
// ToTxCompact / FromTxCompact – BaseTxEnvelope
// ---------------------------------------------------------------------------

impl reth_codecs::alloy::transaction::ToTxCompact for BaseTxEnvelope {
    fn to_tx_compact(&self, buf: &mut (impl BufMut + AsMut<[u8]>)) {
        match self {
            Self::Legacy(tx) => {
                tx.tx().to_compact(buf);
            }
            Self::Eip2930(tx) => {
                tx.tx().to_compact(buf);
            }
            Self::Eip1559(tx) => {
                tx.tx().to_compact(buf);
            }
            Self::Eip7702(tx) => {
                tx.tx().to_compact(buf);
            }
            Self::Deposit(tx) => {
                tx.to_compact(buf);
            }
            Self::Eip8130(tx) => {
                tx.to_compact(buf);
            }
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
            OpTxType::Eip8130 => {
                let (tx, buf) = Compact::from_compact(buf, buf.len());
                // EIP-8130 carries sender_auth / payer_auth inside the signed
                // payload itself, so the outer envelope signature is only a
                // placeholder mandated by the trait contract.
                let _ = signature;
                (Self::Eip8130(tx), buf)
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Envelope – BaseTxEnvelope
// ---------------------------------------------------------------------------

/// Placeholder signature used for transaction types without an ECDSA signature.
const PLACEHOLDER_SIGNATURE: Signature = Signature::new(U256::ZERO, U256::ZERO, false);

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
            Self::Deposit(_) | Self::Eip8130(_) => &PLACEHOLDER_SIGNATURE,
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

/// Backward-compatible `Compact` wrapper for the EIP-8130 per-phase statuses
/// stored as the trailing field of [`CompactBaseReceipt`].
///
/// The reth `Compact` derive reads a trailing `Vec`/`Cow` field by calling
/// `decode_varuint` on the remaining buffer, which panics when that buffer is
/// empty. Receipts written before this field existed (every legacy/EIP-1559/
/// deposit receipt already on disk) have no trailing bytes, so decoding them
/// with the derive would panic. This wrapper makes the addition backward
/// compatible:
///
/// * `from_compact` returns an empty value when no trailing bytes remain, so
///   pre-existing on-disk receipts decode unchanged.
/// * `to_compact` writes nothing when the statuses are empty, so non-EIP-8130
///   receipts (and EIP-8130 receipts with empty `calls`) stay byte-identical to
///   the current on-disk format and never grow the encoding.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
struct CompactPhaseStatuses(Vec<u8>);

impl Compact for CompactPhaseStatuses {
    fn to_compact<B>(&self, buf: &mut B) -> usize
    where
        B: BufMut + AsMut<[u8]>,
    {
        if self.0.is_empty() {
            return 0;
        }
        self.0.to_compact(buf)
    }

    fn from_compact(buf: &[u8], len: usize) -> (Self, &[u8]) {
        // Receipts written before this field existed have no trailing bytes;
        // decode them as empty rather than reading past the end of the buffer.
        if buf.is_empty() {
            return (Self(Vec::new()), buf);
        }
        let (statuses, buf) = Vec::<u8>::from_compact(buf, len);
        (Self(statuses), buf)
    }
}

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
    /// EIP-8130 per-phase execution statuses. Persisted to the node-local
    /// database so `eth_getTransactionReceipt` can surface `phaseStatuses`;
    /// excluded from the consensus receipt encoding. Empty for non-8130
    /// receipts. Must remain the last field — see [`CompactPhaseStatuses`] for
    /// the backward-compatibility contract with pre-existing on-disk receipts.
    eip8130_phase_statuses: CompactPhaseStatuses,
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
            eip8130_phase_statuses: if let BaseReceipt::Eip8130(receipt) = receipt {
                CompactPhaseStatuses(receipt.phase_statuses.clone())
            } else {
                CompactPhaseStatuses(Vec::new())
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
            eip8130_phase_statuses,
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
                Self::Eip8130(Eip8130Receipt::new(inner, eip8130_phase_statuses.0))
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

/// Magic prefix for Base-owned compact v1 header rows.
///
/// `[0x7E, 0x01]` is split intentionally:
///
/// - `0x7E` is the impossible-legacy sentinel
/// - `0x01` is the Base-owned compact header version
///
/// `0x7E` sets the frozen legacy compact difficulty length field to `63`, which is impossible for
/// a `U256` difficulty value and therefore cannot collide with any historical upstream header row.
pub const BASE_HEADER_COMPACT_V1_MAGIC: [u8; 2] = [0x7E, 0x01];

/// Error returned when decoding the Base-owned compact header boundary fails.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum BaseHeaderCompactError {
    /// Base-owned compact bytes started with the reserved prefix but not a known version.
    #[error("invalid Base header compact magic prefix")]
    InvalidMagic,

    /// Base-owned compact bytes were truncated before the payload started.
    #[error("Base header compact bytes are too short")]
    InputTooShort,

    /// Base-owned compact v1 rows must contain at least one Base-owned field.
    #[error("Base header compact v1 payload must contain Base-owned fields")]
    EmptyBaseFields,

    /// Base-owned compact v1 timestamp milliseconds did not fit into the public `u16` type.
    #[error("Base header compact timestamp milliseconds {0} do not fit in u16")]
    TimestampMillisPartOutOfRange(u64),

    /// Base-owned compact v1 inner header compact bytes contained trailing data.
    #[error("Base header compact inner header compact bytes have trailing bytes")]
    TrailingInnerHeaderCompact,

    /// Base-owned fields failed semantic validation.
    #[error(transparent)]
    TimestampMillisPart(#[from] TimestampMillisPartError),
}

/// Frozen legacy compact v0 snapshot for pre-hardfork stored header rows.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Default, Compact)]
#[reth_codecs(crate = "reth_codecs")]
pub struct LegacyHeaderCompactV0 {
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
    /// Optional newly-added upstream header fields.
    pub extra_fields: Option<LegacyHeaderExtV0>,
    /// Free-form extra data.
    pub extra_data: Bytes,
}

/// Frozen legacy compact v0 snapshot for upstream optional header fields.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Default, Compact)]
#[reth_codecs(crate = "reth_codecs")]
pub struct LegacyHeaderExtV0 {
    /// EIP-7685 execution requests hash.
    pub requests_hash: Option<B256>,
    /// EIP-7928 block access list hash.
    pub block_access_list_hash: Option<B256>,
    /// EIP-7843 slot number.
    pub slot_number: Option<u64>,
}

impl LegacyHeaderExtV0 {
    /// Returns `Some(self)` if any field is populated, `None` otherwise.
    pub const fn into_option(self) -> Option<Self> {
        if self.requests_hash.is_some()
            || self.block_access_list_hash.is_some()
            || self.slot_number.is_some()
        {
            Some(self)
        } else {
            None
        }
    }
}

impl LegacyHeaderCompactV0 {
    /// Creates the frozen legacy compact v0 snapshot for a plain Ethereum header.
    pub fn from_header(header: &Header) -> Self {
        let Header {
            parent_hash,
            ommers_hash,
            beneficiary,
            state_root,
            transactions_root,
            receipts_root,
            withdrawals_root,
            logs_bloom,
            difficulty,
            number,
            gas_limit,
            gas_used,
            timestamp,
            mix_hash,
            nonce,
            base_fee_per_gas,
            blob_gas_used,
            excess_blob_gas,
            parent_beacon_block_root,
            requests_hash,
            block_access_list_hash,
            slot_number,
            extra_data,
        } = header;
        let extra_fields = LegacyHeaderExtV0 {
            requests_hash: *requests_hash,
            block_access_list_hash: *block_access_list_hash,
            slot_number: *slot_number,
        };

        Self {
            parent_hash: *parent_hash,
            ommers_hash: *ommers_hash,
            beneficiary: *beneficiary,
            state_root: *state_root,
            transactions_root: *transactions_root,
            receipts_root: *receipts_root,
            withdrawals_root: *withdrawals_root,
            logs_bloom: *logs_bloom,
            difficulty: *difficulty,
            number: *number,
            gas_limit: *gas_limit,
            gas_used: *gas_used,
            timestamp: *timestamp,
            mix_hash: *mix_hash,
            nonce: u64::from(*nonce),
            base_fee_per_gas: *base_fee_per_gas,
            blob_gas_used: *blob_gas_used,
            excess_blob_gas: *excess_blob_gas,
            parent_beacon_block_root: *parent_beacon_block_root,
            extra_fields: extra_fields.into_option(),
            extra_data: extra_data.clone(),
        }
    }

    /// Reconstructs the plain Ethereum header stored in the frozen legacy compact v0 snapshot.
    pub fn into_header(self) -> Header {
        Header {
            parent_hash: self.parent_hash,
            ommers_hash: self.ommers_hash,
            beneficiary: self.beneficiary,
            state_root: self.state_root,
            transactions_root: self.transactions_root,
            receipts_root: self.receipts_root,
            withdrawals_root: self.withdrawals_root,
            logs_bloom: self.logs_bloom,
            difficulty: self.difficulty,
            number: self.number,
            gas_limit: self.gas_limit,
            gas_used: self.gas_used,
            timestamp: self.timestamp,
            mix_hash: self.mix_hash,
            nonce: self.nonce.into(),
            base_fee_per_gas: self.base_fee_per_gas,
            blob_gas_used: self.blob_gas_used,
            excess_blob_gas: self.excess_blob_gas,
            parent_beacon_block_root: self.parent_beacon_block_root,
            requests_hash: self.extra_fields.as_ref().and_then(|fields| fields.requests_hash),
            block_access_list_hash: self
                .extra_fields
                .as_ref()
                .and_then(|fields| fields.block_access_list_hash),
            slot_number: self.extra_fields.as_ref().and_then(|fields| fields.slot_number),
            extra_data: self.extra_data,
        }
    }
}

/// Base-owned compact v1 fields stored ahead of the nested upstream compact header bytes.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Default, Compact)]
#[reth_codecs(crate = "reth_codecs")]
pub struct BaseHeaderCompactFieldsV1 {
    /// Compact form of the Base-owned millisecond subsecond component.
    ///
    /// This stays in the Base-owned outer field bucket so future Base-only fields append here
    /// without borrowing upstream `Header` compact layout.
    pub timestamp_millis_part: Option<u64>,
}

impl BaseHeaderCompactFieldsV1 {
    /// Builds the compact v1 field set from the semantic Base-owned field set.
    pub fn from_base_fields(base: &BaseHeaderFields) -> Result<Self, BaseHeaderCompactError> {
        base.validate()?;
        Ok(Self { timestamp_millis_part: base.timestamp_millis_part.map(u64::from) })
    }

    /// Reconstructs semantic Base-owned fields from the compact v1 field set.
    pub fn try_into_base_fields(self) -> Result<BaseHeaderFields, BaseHeaderCompactError> {
        let Self { timestamp_millis_part } = self;
        let timestamp_millis_part = timestamp_millis_part
            .map(|part| {
                u16::try_from(part)
                    .map_err(|_| BaseHeaderCompactError::TimestampMillisPartOutOfRange(part))
            })
            .transpose()?;
        let base = BaseHeaderFields::new(timestamp_millis_part);
        base.validate()?;
        Ok(base)
    }
}

/// Base-owned compact v1 payload for post-hardfork stored header rows.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Default)]
pub struct BaseHeaderCompactV1 {
    /// Base-owned compact v1 field bucket for fields that belong to Base rather than upstream.
    pub base: BaseHeaderCompactFieldsV1,
    /// Raw upstream `Header::to_compact` bytes for the nested Ethereum header.
    ///
    /// Base owns the outer version boundary; upstream still owns the inner compact layout.
    pub inner_compact: Bytes,
}

impl BaseHeaderCompactV1 {
    /// Creates the Base-owned compact v1 payload for a post-hardfork Base header.
    pub fn from_header(header: &BaseHeader) -> Result<Self, BaseHeaderCompactError> {
        let mut inner_compact = Vec::new();
        header.inner.to_compact(&mut inner_compact);
        Ok(Self {
            base: BaseHeaderCompactFieldsV1::from_base_fields(&header.base)?,
            inner_compact: inner_compact.into(),
        })
    }

    /// Reconstructs the semantic Base header from a Base-owned compact v1 payload.
    pub fn try_into_header(self) -> Result<BaseHeader, BaseHeaderCompactError> {
        let base = self.base.try_into_base_fields()?;
        if base.is_empty() {
            return Err(BaseHeaderCompactError::EmptyBaseFields);
        }

        let (inner, remaining) =
            Header::from_compact(self.inner_compact.as_ref(), self.inner_compact.len());
        if !remaining.is_empty() {
            return Err(BaseHeaderCompactError::TrailingInnerHeaderCompact);
        }

        BaseHeader::new(inner, base).map_err(BaseHeaderCompactError::from)
    }
}

impl Compact for BaseHeaderCompactV1 {
    fn to_compact<B>(&self, buf: &mut B) -> usize
    where
        B: BufMut + AsMut<[u8]>,
    {
        self.base.to_compact(buf) + self.inner_compact.to_compact(buf)
    }

    fn from_compact(buf: &[u8], len: usize) -> (Self, &[u8]) {
        let (base, remaining) = BaseHeaderCompactFieldsV1::from_compact(buf, len);
        let base_len = buf.len() - remaining.len();
        let inner_len = len - base_len;
        let (inner_compact, remaining) = Bytes::from_compact(remaining, inner_len);
        (Self { base, inner_compact }, remaining)
    }
}

impl BaseHeader {
    /// Decodes Base compact bytes with validation for the Base-owned v1 boundary.
    pub fn decode_compact_checked(
        buf: &[u8],
        len: usize,
    ) -> Result<(Self, &[u8]), BaseHeaderCompactError> {
        if buf.starts_with(&BASE_HEADER_COMPACT_V1_MAGIC) {
            let payload_len = len
                .checked_sub(BASE_HEADER_COMPACT_V1_MAGIC.len())
                .ok_or(BaseHeaderCompactError::InputTooShort)?;
            let (payload, remaining) = BaseHeaderCompactV1::from_compact(
                &buf[BASE_HEADER_COMPACT_V1_MAGIC.len()..],
                payload_len,
            );
            return Ok((payload.try_into_header()?, remaining));
        }

        if buf.first().copied() == Some(BASE_HEADER_COMPACT_V1_MAGIC[0]) {
            return Err(BaseHeaderCompactError::InvalidMagic);
        }

        let (payload, remaining) = LegacyHeaderCompactV0::from_compact(buf, len);
        Ok((Self::from(payload.into_header()), remaining))
    }
}

impl Compact for BaseHeader {
    fn to_compact<B>(&self, buf: &mut B) -> usize
    where
        B: BufMut + AsMut<[u8]>,
    {
        if self.is_legacy() {
            return LegacyHeaderCompactV0::from_header(&self.inner).to_compact(buf);
        }

        let payload =
            BaseHeaderCompactV1::from_header(self).expect("invalid Base header compact fields");
        buf.put_slice(&BASE_HEADER_COMPACT_V1_MAGIC);
        BASE_HEADER_COMPACT_V1_MAGIC.len() + payload.to_compact(buf)
    }

    fn from_compact(buf: &[u8], len: usize) -> (Self, &[u8]) {
        Self::decode_compact_checked(buf, len)
            .unwrap_or_else(|error| panic!("invalid BaseHeader compact bytes: {error}"))
    }
}

impl reth_db_api::table::Compress for BaseHeader {
    type Compressed = Vec<u8>;

    fn compress_to_buf<B: BufMut + AsMut<[u8]>>(&self, buf: &mut B) {
        let _ = Compact::to_compact(self, buf);
    }
}

impl reth_db_api::table::Decompress for BaseHeader {
    fn decompress(value: &[u8]) -> Result<Self, DecompressError> {
        let (header, _) =
            Self::decode_compact_checked(value, value.len()).map_err(DecompressError::new)?;
        Ok(header)
    }
}

impl reth_primitives_traits::BlockHeader for BaseHeader {}

impl reth_primitives_traits::header::HeaderMut for BaseHeader {
    fn set_parent_hash(&mut self, hash: BlockHash) {
        self.inner.parent_hash = hash;
    }

    fn set_block_number(&mut self, number: BlockNumber) {
        self.inner.number = number;
    }

    fn set_timestamp(&mut self, timestamp: u64) {
        self.inner.timestamp = timestamp;
    }

    fn set_state_root(&mut self, state_root: B256) {
        self.inner.state_root = state_root;
    }

    fn set_difficulty(&mut self, difficulty: U256) {
        self.inner.difficulty = difficulty;
    }

    fn set_mix_hash(&mut self, mix_hash: B256) {
        self.inner.mix_hash = mix_hash;
    }

    fn set_extra_data(&mut self, extra_data: Bytes) {
        self.inner.extra_data = extra_data;
    }

    fn set_parent_beacon_block_root(&mut self, parent_beacon_block_root: Option<B256>) {
        self.inner.parent_beacon_block_root = parent_beacon_block_root;
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
    use alloy_consensus::{Header, Receipt};
    use alloy_primitives::{B256, Bytes, Log};
    use reth_codecs::Compact;

    use super::*;

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
            block_access_list_hash: Some(B256::repeat_byte(0x44)),
            slot_number: Some(55),
            extra_data: Bytes::from_static(b"legacy-header"),
            ..Default::default()
        }
    }

    #[test]
    fn compact_phase_statuses_empty_writes_nothing_and_decodes_empty() {
        let mut buf = Vec::new();
        let written = CompactPhaseStatuses(Vec::new()).to_compact(&mut buf);
        assert_eq!(written, 0);
        assert!(buf.is_empty());

        let (decoded, rest) = CompactPhaseStatuses::from_compact(&[], 0);
        assert_eq!(decoded, CompactPhaseStatuses(Vec::new()));
        assert!(rest.is_empty());
    }

    #[test]
    fn compact_phase_statuses_nonempty_roundtrips() {
        let statuses = CompactPhaseStatuses(vec![0x01, 0x00]);
        let mut buf = Vec::new();
        statuses.to_compact(&mut buf);
        assert!(!buf.is_empty());
        let (decoded, _) = CompactPhaseStatuses::from_compact(&buf, buf.len());
        assert_eq!(decoded, statuses);
    }

    #[test]
    fn base_receipt_compact_decode_tolerates_missing_phase_statuses() {
        let receipt = BaseReceipt::Legacy(Receipt {
            status: true.into(),
            cumulative_gas_used: 21_000,
            logs: vec![Log::default()],
        });

        let mut buf = Vec::new();
        let len = receipt.to_compact(&mut buf);
        let (decoded, _) = BaseReceipt::from_compact(&buf, len);

        assert_eq!(decoded, receipt);
    }

    #[test]
    fn base_receipt_compact_roundtrips_eip8130_phase_statuses() {
        let receipt = BaseReceipt::Eip8130(Eip8130Receipt::new(
            Receipt {
                status: true.into(),
                cumulative_gas_used: 21_000,
                logs: vec![Log::default()],
            },
            vec![0x01, 0x00, 0x01],
        ));

        let mut buf = Vec::new();
        let len = receipt.to_compact(&mut buf);
        let (decoded, _) = BaseReceipt::from_compact(&buf, len);

        assert_eq!(decoded, receipt);
        let BaseReceipt::Eip8130(decoded) = decoded else {
            panic!("decoded receipt must remain an EIP-8130 receipt");
        };
        assert_eq!(decoded.phase_statuses, vec![0x01, 0x00, 0x01]);
    }

    #[test]
    fn legacy_header_compact_matches_upstream_header_bytes() {
        let inner = sample_inner_header();
        let header = BaseHeader::from(inner.clone());

        let mut base_encoded = Vec::new();
        let base_len = header.to_compact(&mut base_encoded);

        let mut upstream_encoded = Vec::new();
        let upstream_len = inner.to_compact(&mut upstream_encoded);

        assert_eq!(base_len, upstream_len);
        assert_eq!(base_encoded, upstream_encoded);
    }

    #[test]
    fn legacy_header_compact_decodes_as_empty_base_fields() {
        let inner = Header {
            timestamp: 1_780_334_562,
            number: 42_000,
            gas_limit: 30_000_000,
            gas_used: 1_234,
            base_fee_per_gas: Some(1_000_000_000),
            requests_hash: Some(B256::repeat_byte(0x33)),
            extra_data: Bytes::from_static(b"legacy-header"),
            ..Default::default()
        };

        let mut encoded = Vec::new();
        let len = inner.to_compact(&mut encoded);
        let (decoded, remaining) = BaseHeader::decode_compact_checked(&encoded, len).unwrap();

        assert!(remaining.is_empty());
        assert_eq!(decoded.inner, inner);
        assert_eq!(decoded.base, BaseHeaderFields::default());
    }

    #[test]
    fn post_fork_header_compact_round_trips_base_fields() {
        let header = BaseHeader::new(
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
                extra_data: Bytes::from_static(b"post-fork-header"),
                ..Default::default()
            },
            BaseHeaderFields::new(Some(600)),
        )
        .unwrap();

        let mut encoded = Vec::new();
        let len = header.to_compact(&mut encoded);
        let (decoded, remaining) = BaseHeader::decode_compact_checked(&encoded, len).unwrap();

        assert_eq!(&encoded[..BASE_HEADER_COMPACT_V1_MAGIC.len()], &BASE_HEADER_COMPACT_V1_MAGIC);
        assert!(remaining.is_empty());
        assert_eq!(decoded, header);
    }

    #[test]
    fn base_header_compact_v1_uses_one_flag_byte_for_base_fields() {
        assert_eq!(BaseHeaderCompactFieldsV1::bitflag_encoded_bytes(), 1);
    }

    #[test]
    fn post_fork_header_compact_v1_stores_upstream_compact_inner_header() {
        let header =
            BaseHeader::new(sample_inner_header(), BaseHeaderFields::new(Some(600))).unwrap();

        let payload = BaseHeaderCompactV1::from_header(&header).unwrap();

        let mut expected_inner_compact = Vec::new();
        header.inner.to_compact(&mut expected_inner_compact);

        assert_eq!(payload.base.timestamp_millis_part, Some(600));
        assert_eq!(payload.inner_compact.as_ref(), expected_inner_compact.as_slice());
    }
}
