//! Base transaction envelope for EVM2.

use alloy_consensus::Transaction;
use alloy_eips::eip2718::Typed2718;
use alloy_primitives::Bytes;
use base_common_consensus::{Eip8130Signed, TxDeposit};
use evm2::ethereum::TxEnvelope;

/// The Base transaction envelope: a deposit, a standard Ethereum transaction, or an enshrined
/// EIP-8130 account-abstraction transaction.
#[derive(Clone, Debug)]
pub enum BaseTxEnvelope {
    /// An L1-originated deposit transaction.
    Deposit(TxDeposit),
    /// A standard, signed Ethereum transaction.
    Standard {
        /// The decoded standard transaction.
        tx: TxEnvelope,
        /// The EIP-2718 encoded bytes the transaction was decoded from, as posted to L1. Used
        /// to price the L1 data fee over the full transaction (matching the revm integration),
        /// which `TxEnvelope` alone cannot reproduce.
        enveloped: Bytes,
    },
    /// An enshrined EIP-8130 account-abstraction transaction (type `0x79`, Zenith onwards). Its
    /// enshrined execution path is layered on with the EIP-8130 track; this variant carries the
    /// signed transaction so the envelope and receipts can already represent it.
    ///
    /// TODO(eip8130-execution): carry the EIP-2718 serialized bytes here (as [`Self::Standard`]
    /// does) before the `0x79` execution handler is registered. Until then [`Self::enveloped`]
    /// returns `None` for this variant, which the executor's Jovian DA-footprint metering treats
    /// as an empty payload — hitting the minimum-tx-size floor and undercounting the true DA cost.
    /// Inert today (no `0x79` handler is registered, so an EIP-8130 tx never reaches DA
    /// accumulation), but it must be fixed together with the handler.
    Eip8130(Eip8130Signed),
}

impl BaseTxEnvelope {
    /// Builds a standard-transaction envelope from a decoded transaction and its EIP-2718
    /// encoded bytes.
    pub fn standard(tx: TxEnvelope, enveloped: Bytes) -> Self {
        // Empty enveloped bytes would make L1FeeParams::is_fee_exempt treat the transaction as
        // fee-exempt, silently zeroing the L1 data fee and operator fee — a consensus-level fee
        // miscalculation once this crate is wired into the node. This is an internal invariant that
        // must always hold, so enforce it in every build (not just debug) with a runtime assert.
        assert!(!enveloped.is_empty(), "standard tx must carry non-empty enveloped bytes");
        Self::Standard { tx, enveloped }
    }

    /// Returns the deposit transaction, if this envelope is a deposit.
    pub const fn as_deposit(&self) -> Option<&TxDeposit> {
        match self {
            Self::Deposit(tx) => Some(tx),
            Self::Standard { .. } | Self::Eip8130(_) => None,
        }
    }

    /// Returns whether this envelope is a deposit transaction.
    pub const fn is_deposit(&self) -> bool {
        matches!(self, Self::Deposit(_))
    }

    /// Returns the EIP-8130 transaction, if this envelope is one.
    pub const fn as_eip8130(&self) -> Option<&Eip8130Signed> {
        match self {
            Self::Eip8130(tx) => Some(tx),
            Self::Deposit(_) | Self::Standard { .. } => None,
        }
    }

    /// Returns whether this envelope is an EIP-8130 transaction.
    pub const fn is_eip8130(&self) -> bool {
        matches!(self, Self::Eip8130(_))
    }

    /// Returns the transaction's gas limit.
    pub fn gas_limit(&self) -> u64 {
        match self {
            Self::Standard { tx, .. } => tx.gas_limit(),
            Self::Deposit(tx) => tx.gas_limit,
            Self::Eip8130(tx) => tx.gas_limit(),
        }
    }

    /// Returns the standard Ethereum transaction, if this envelope is a standard transaction.
    pub const fn as_standard(&self) -> Option<&TxEnvelope> {
        match self {
            Self::Standard { tx, .. } => Some(tx),
            Self::Deposit(_) | Self::Eip8130(_) => None,
        }
    }

    /// Returns the standard transaction's EIP-2718 encoded bytes, if this envelope is a standard
    /// transaction.
    pub const fn enveloped(&self) -> Option<&Bytes> {
        match self {
            Self::Standard { enveloped, .. } => Some(enveloped),
            Self::Deposit(_) | Self::Eip8130(_) => None,
        }
    }
}

impl Typed2718 for BaseTxEnvelope {
    fn ty(&self) -> u8 {
        match self {
            Self::Deposit(tx) => tx.ty(),
            Self::Standard { tx, .. } => tx.ty(),
            Self::Eip8130(tx) => tx.ty(),
        }
    }
}

impl From<TxDeposit> for BaseTxEnvelope {
    fn from(tx: TxDeposit) -> Self {
        Self::Deposit(tx)
    }
}

impl From<Eip8130Signed> for BaseTxEnvelope {
    fn from(tx: Eip8130Signed) -> Self {
        Self::Eip8130(tx)
    }
}

#[cfg(test)]
mod tests {
    use alloy_primitives::{Address, TxKind};
    use base_common_consensus::{DEPOSIT_TX_TYPE_ID, EIP8130_TX_TYPE_ID, TxEip8130};

    use super::*;

    fn sample_deposit() -> TxDeposit {
        TxDeposit {
            from: Address::repeat_byte(0x11),
            to: TxKind::Call(Address::repeat_byte(0x22)),
            gas_limit: 100_000,
            ..Default::default()
        }
    }

    #[test]
    fn deposit_reports_type_byte() {
        let tx = BaseTxEnvelope::Deposit(sample_deposit());
        assert_eq!(tx.ty(), DEPOSIT_TX_TYPE_ID);
        assert!(tx.is_deposit());
        assert!(tx.as_deposit().is_some());
    }

    #[test]
    fn eip8130_reports_type_byte_and_gas_limit() {
        let tx = Eip8130Signed::new(
            TxEip8130 { gas_limit: 250_000, ..Default::default() },
            Bytes::new(),
            Bytes::new(),
        );
        let envelope = BaseTxEnvelope::from(tx);
        assert_eq!(envelope.ty(), EIP8130_TX_TYPE_ID);
        assert!(envelope.is_eip8130());
        assert!(envelope.as_eip8130().is_some());
        assert_eq!(envelope.gas_limit(), 250_000);
        // An EIP-8130 tx is neither a deposit nor a standard transaction.
        assert!(!envelope.is_deposit());
        assert!(envelope.as_standard().is_none());
        assert!(envelope.enveloped().is_none());
    }
}
