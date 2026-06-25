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
use alloy_primitives::{Address, B256, Bytes, Signature, TxKind, U256};
use bytes::{Buf, BufMut};
use reth_codecs::{
    Compact, CompactZstd, DecompressError,
    txtype::{
        COMPACT_EXTENDED_IDENTIFIER_FLAG, COMPACT_IDENTIFIER_EIP1559, COMPACT_IDENTIFIER_EIP2930,
        COMPACT_IDENTIFIER_LEGACY,
    },
};

use crate::{
    BaseBlock, BaseReceipt, BaseTxEnvelope, BaseTypedTransaction, DEPOSIT_TX_TYPE_ID,
    DepositReceipt, EIP8130_TX_TYPE_ID, Eip8130Receipt, OpTxType, TxDeposit, TxEip8130,
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
    use alloy_consensus::Receipt;
    use alloy_primitives::Log;

    use super::*;

    #[test]
    fn compact_phase_statuses_empty_writes_nothing_and_decodes_empty() {
        // Empty statuses must encode to zero bytes so non-8130 receipts (and
        // empty-`calls` 8130 receipts) keep the pre-existing on-disk format.
        let mut buf = Vec::new();
        let written = CompactPhaseStatuses(Vec::new()).to_compact(&mut buf);
        assert_eq!(written, 0);
        assert!(buf.is_empty());

        // An empty trailing buffer (a receipt written before this field existed)
        // must decode to empty rather than panic.
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
        // A non-8130 receipt encodes with zero trailing phase-status bytes, so
        // its `Compact` byte stream is identical to receipts written before the
        // `eip8130_phase_statuses` field existed. Decoding must not panic and
        // must reproduce the original receipt — proving the field addition is
        // backward compatible with pre-existing on-disk receipts.
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
        // Pins that `eip8130_phase_statuses` is wired through `CompactBaseReceipt`
        // as the trailing field: a non-empty status array must survive a full
        // encode/decode round-trip. Reordering or dropping the field (so an 8130
        // receipt decodes via the empty-trailing tolerance path) would lose the
        // statuses and fail this assertion.
        let receipt = BaseReceipt::Eip8130(crate::Eip8130Receipt::new(
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
}
