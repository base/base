//! Base transaction envelope for EVM2.

use alloy_eips::eip2718::Typed2718;
use alloy_primitives::{Address, B256, Bytes, TxKind, U256};
/// EIP-2718 transaction type byte for deposit transactions.
///
/// Re-exported from [`base_common_consensus`] to keep a single source of truth
/// shared with `base-common-evm`.
pub use base_common_consensus::DEPOSIT_TX_TYPE_ID as DEPOSIT_TX_TYPE;
use evm2::ethereum::TxEnvelope;

/// A deposit transaction (type `0x7e`).
///
/// Deposits are L1-originated: they mint value on L2, are exempt from the L1
/// data fee and standard gas payment, and carry an L1-derived `source_hash`
/// rather than a signature.
///
/// Named `DepositTx` (rather than `DepositTransaction`) to avoid colliding with
/// the [`DepositTransaction`](base_common_consensus::DepositTransaction) trait
/// exported by `base-common-consensus`, and to match the existing `TxDeposit`
/// naming convention.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DepositTx {
    /// Hash uniquely identifying the L1 source of this deposit.
    pub source_hash: B256,
    /// Address the deposit is sent from (the aliased L1 sender).
    pub from: Address,
    /// Call target, or `Create` for contract creation.
    pub to: TxKind,
    /// Value minted on L2 before execution; zero means no mint, matching
    /// `base_common_consensus::TxDeposit`.
    pub mint: u128,
    /// Value transferred with the call.
    pub value: U256,
    /// Gas limit for execution.
    pub gas_limit: u64,
    /// Whether this is a system transaction (no gas metering).
    pub is_system_transaction: bool,
    /// Calldata, or init code for a create.
    pub input: Bytes,
}

impl Typed2718 for DepositTx {
    fn ty(&self) -> u8 {
        DEPOSIT_TX_TYPE
    }
}

/// The Base transaction envelope: a deposit or a standard Ethereum
/// transaction.
#[derive(Clone, Debug)]
pub enum BaseTransaction {
    /// An L1-originated deposit transaction.
    Deposit(DepositTx),
    /// A standard, signed Ethereum transaction.
    Standard(TxEnvelope),
}

impl BaseTransaction {
    /// Returns the deposit transaction, if this envelope is a deposit.
    pub const fn as_deposit(&self) -> Option<&DepositTx> {
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

impl Typed2718 for BaseTransaction {
    fn ty(&self) -> u8 {
        match self {
            Self::Deposit(tx) => tx.ty(),
            Self::Standard(tx) => tx.ty(),
        }
    }
}

impl From<DepositTx> for BaseTransaction {
    fn from(tx: DepositTx) -> Self {
        Self::Deposit(tx)
    }
}

impl From<TxEnvelope> for BaseTransaction {
    fn from(tx: TxEnvelope) -> Self {
        Self::Standard(tx)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_deposit() -> DepositTx {
        DepositTx {
            source_hash: B256::ZERO,
            from: Address::repeat_byte(0x11),
            to: TxKind::Call(Address::repeat_byte(0x22)),
            mint: 0,
            value: U256::ZERO,
            gas_limit: 100_000,
            is_system_transaction: false,
            input: Bytes::new(),
        }
    }

    #[test]
    fn deposit_reports_type_byte() {
        let tx = BaseTransaction::Deposit(sample_deposit());
        assert_eq!(tx.ty(), DEPOSIT_TX_TYPE);
        assert!(tx.is_deposit());
        assert!(tx.as_deposit().is_some());
    }
}
