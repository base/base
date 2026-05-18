//! Reth compatibility implementations for base-alloy consensus types.
//!
//! This module provides implementations of reth traits gated behind the `reth` feature flag,
//! including `InMemorySize`, `SignedTransaction`, `SerdeBincodeCompat`, `Compact`,
//! `Envelope`, `ToTxCompact`, `FromTxCompact`, `Compress`, and `Decompress`.

// Ensure `reth-ethereum-primitives` serde-bincode-compat feature is activated.
use alloc::{borrow::Cow, vec::Vec};

use alloy_consensus::{
    Header, Receipt, Sealed, Signed, TxEip1559, TxEip2930, TxEip7702, TxLegacy, TxReceipt,
    constants::EIP7702_TX_TYPE_ID,
};
use alloy_primitives::{Address, B256, Bytes, Signature, TxKind, U256};
use bytes::{Buf, BufMut};
use reth_codecs::{
    Compact, CompactZstd,
    txtype::{
        COMPACT_EXTENDED_IDENTIFIER_FLAG, COMPACT_IDENTIFIER_EIP1559, COMPACT_IDENTIFIER_EIP2930,
        COMPACT_IDENTIFIER_LEGACY,
    },
};
use reth_ethereum_primitives as _;

use crate::{
    BaseBlock, BasePooledTransaction, BaseReceipt, BaseTxEnvelope, BaseTypedTransaction,
    DEPOSIT_TX_TYPE_ID, DepositReceipt, OpTxType, TxDeposit,
};

// ---------------------------------------------------------------------------
// InMemorySize (reth-primitives-traits)
// ---------------------------------------------------------------------------

impl reth_primitives_traits::InMemorySize for OpTxType {
    #[inline]
    fn size(&self) -> usize {
        core::mem::size_of::<Self>()
    }
}

impl reth_primitives_traits::InMemorySize for TxDeposit {
    #[inline]
    fn size(&self) -> usize {
        Self::size(self)
    }
}

impl reth_primitives_traits::InMemorySize for DepositReceipt {
    fn size(&self) -> usize {
        self.inner.size()
            + core::mem::size_of_val(&self.deposit_nonce)
            + core::mem::size_of_val(&self.deposit_receipt_version)
    }
}

impl reth_primitives_traits::InMemorySize for BaseReceipt {
    fn size(&self) -> usize {
        match self {
            Self::Legacy(receipt)
            | Self::Eip2930(receipt)
            | Self::Eip1559(receipt)
            | Self::Eip7702(receipt) => receipt.size(),
            Self::Deposit(receipt) => receipt.size(),
        }
    }
}

impl reth_primitives_traits::InMemorySize for BaseTypedTransaction {
    fn size(&self) -> usize {
        match self {
            Self::Legacy(tx) => tx.size(),
            Self::Eip2930(tx) => tx.size(),
            Self::Eip1559(tx) => tx.size(),
            Self::Eip7702(tx) => tx.size(),
            Self::Deposit(tx) => tx.size(),
        }
    }
}

impl reth_primitives_traits::InMemorySize for BasePooledTransaction {
    fn size(&self) -> usize {
        match self {
            Self::Legacy(tx) => tx.size(),
            Self::Eip2930(tx) => tx.size(),
            Self::Eip1559(tx) => tx.size(),
            Self::Eip7702(tx) => tx.size(),
        }
    }
}

impl reth_primitives_traits::InMemorySize for BaseTxEnvelope {
    fn size(&self) -> usize {
        match self {
            Self::Legacy(tx) => tx.size(),
            Self::Eip2930(tx) => tx.size(),
            Self::Eip1559(tx) => tx.size(),
            Self::Eip7702(tx) => tx.size(),
            Self::Deposit(tx) => tx.size(),
        }
    }
}

// ---------------------------------------------------------------------------
// SignedTransaction (reth-primitives-traits)
// ---------------------------------------------------------------------------

impl reth_primitives_traits::SignedTransaction for BasePooledTransaction {}

impl reth_primitives_traits::SignedTransaction for BaseTxEnvelope {
    fn is_system_tx(&self) -> bool {
        self.is_system_transaction()
    }
}

// ---------------------------------------------------------------------------
// SerdeBincodeCompat (reth-primitives-traits)
// ---------------------------------------------------------------------------

impl reth_primitives_traits::serde_bincode_compat::SerdeBincodeCompat for BaseTxEnvelope {
    type BincodeRepr<'a> = crate::serde_bincode_compat::transaction::BaseTxEnvelope<'a>;

    fn as_repr(&self) -> Self::BincodeRepr<'_> {
        self.into()
    }

    fn from_repr(repr: Self::BincodeRepr<'_>) -> Self {
        repr.into()
    }
}

impl reth_primitives_traits::serde_bincode_compat::SerdeBincodeCompat for BaseReceipt {
    type BincodeRepr<'a> = crate::serde_bincode_compat::BaseReceipt<'a>;

    fn as_repr(&self) -> Self::BincodeRepr<'_> {
        self.into()
    }

    fn from_repr(repr: Self::BincodeRepr<'_>) -> Self {
        repr.into()
    }
}

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
            Self::Deposit(_) => &DEPOSIT_SIGNATURE,
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
            tx_type: receipt.tx_type(),
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
    fn decompress(value: &[u8]) -> Result<Self, reth_db_api::DatabaseError> {
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
    fn decompress(value: &[u8]) -> Result<Self, reth_db_api::DatabaseError> {
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
