//! Base transaction envelope for EVM2.

use alloy_eips::eip2718::Typed2718;
/// EIP-2718 transaction type byte for deposit transactions.
///
/// Re-exported from [`base_common_consensus`] to keep a single source of truth
/// shared with `base-common-evm`.
pub use base_common_consensus::DEPOSIT_TX_TYPE_ID as DEPOSIT_TX_TYPE;
/// The canonical deposit transaction, re-exported from [`base_common_consensus`]
/// rather than redeclared, so the evm2 envelope shares a single source of truth
/// with the rest of the workspace.
pub use base_common_consensus::TxDeposit;
use evm2::ethereum::TxEnvelope;

/// The Base transaction envelope: a deposit or a standard Ethereum
/// transaction.
#[derive(Clone, Debug)]
pub enum BaseTxEnvelope {
    /// An L1-originated deposit transaction.
    Deposit(TxDeposit),
    /// A standard, signed Ethereum transaction.
    Standard(TxEnvelope),
}

impl BaseTxEnvelope {
    /// Returns the deposit transaction, if this envelope is a deposit.
    pub const fn as_deposit(&self) -> Option<&TxDeposit> {
        match self {
            Self::Deposit(tx) => Some(tx),
            Self::Standard(_) => None,
        }
    }

    /// Returns whether this envelope is a deposit transaction.
    pub const fn is_deposit(&self) -> bool {
        matches!(self, Self::Deposit(_))
    }
}

impl Typed2718 for BaseTxEnvelope {
    fn ty(&self) -> u8 {
        match self {
            Self::Deposit(tx) => tx.ty(),
            Self::Standard(tx) => tx.ty(),
        }
    }
}

impl From<TxDeposit> for BaseTxEnvelope {
    fn from(tx: TxDeposit) -> Self {
        Self::Deposit(tx)
    }
}

impl From<TxEnvelope> for BaseTxEnvelope {
    fn from(tx: TxEnvelope) -> Self {
        Self::Standard(tx)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloy_primitives::{Address, TxKind};

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
        assert_eq!(tx.ty(), DEPOSIT_TX_TYPE);
        assert!(tx.is_deposit());
        assert!(tx.as_deposit().is_some());
    }
}
