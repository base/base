//! Base transaction envelope for EVM2.

use alloy_consensus::{EthereumTxEnvelope, Transaction};
use alloy_eips::{eip2718::Typed2718, eip2930::AccessList, eip7702::SignedAuthorization};
use alloy_primitives::{B256, Bytes, ChainId, TxKind, U256};
use base_common_consensus::{BaseTransactionSigned, Eip8130Signed, TxDeposit};
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
    /// An enshrined EIP-8130 account-abstraction transaction (type `0x79`, Everest onwards).
    Eip8130(Eip8130Tx),
}

/// An EIP-8130 transaction as executed: the signed transaction, its EIP-2718 encoding, and the
/// read-only simulation request, if any.
#[derive(Clone, Debug)]
pub struct Eip8130Tx {
    /// The signed EIP-8130 transaction.
    pub signed: Eip8130Signed,
    /// The EIP-2718 encoded bytes, as posted to L1. They price the intrinsic gas, the L1 data
    /// fee, the operator fee, and the Jovian DA footprint.
    pub enveloped: Bytes,
    /// `Some` only on the read-only RPC path (`eth_call` / `eth_estimateGas`), which skips
    /// signature verification and fee settlement and reverts all state. Block execution and pool
    /// validation always carry `None`, so an unverified transaction can never be included.
    pub simulation: Option<Eip8130Simulation>,
}

/// A read-only simulation request for an EIP-8130 transaction.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Eip8130Simulation {
    /// Whether to run the calls once (`eth_call`) or search for the minimum gas limit at which
    /// they succeed (`eth_estimateGas`).
    pub mode: Eip8130SimulationMode,
    /// The acting sender actor, when the request names one (e.g. a session key). Without it the
    /// account's self-actor acts.
    pub sender_actor_id: Option<B256>,
}

/// How an EIP-8130 simulation measures gas.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Eip8130SimulationMode {
    /// `eth_call`: the calls run once at the transaction's gas limit, reporting that run's
    /// metered gas.
    #[default]
    Call,
    /// `eth_estimateGas`: reports the smallest gas limit at which the calls succeed.
    Estimate,
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

    /// Builds the execution envelope of a signed Base transaction from its EIP-2718 encoding.
    pub fn from_signed(tx: BaseTransactionSigned, encoded: Bytes) -> Self {
        let standard =
            |tx: EthereumTxEnvelope<_>| Self::standard(TxEnvelope::from(tx), encoded.clone());
        match tx {
            BaseTransactionSigned::Legacy(tx) => standard(EthereumTxEnvelope::Legacy(tx)),
            BaseTransactionSigned::Eip2930(tx) => standard(EthereumTxEnvelope::Eip2930(tx)),
            BaseTransactionSigned::Eip1559(tx) => standard(EthereumTxEnvelope::Eip1559(tx)),
            BaseTransactionSigned::Eip7702(tx) => standard(EthereumTxEnvelope::Eip7702(tx)),
            BaseTransactionSigned::Deposit(tx) => Self::Deposit(tx.into_inner()),
            BaseTransactionSigned::Eip8130(tx) => Self::eip8130(tx, encoded),
        }
    }

    /// Builds the execution envelope of a signed EIP-8130 transaction from its EIP-2718
    /// encoding, for block execution and pool validation.
    pub const fn eip8130(signed: Eip8130Signed, enveloped: Bytes) -> Self {
        Self::Eip8130(Eip8130Tx { signed, enveloped, simulation: None })
    }

    /// Builds the envelope of an EIP-8130 transaction for a read-only RPC simulation. The
    /// simulated sender is the [`Recovered`](alloy_consensus::transaction::Recovered) signer the
    /// envelope is executed with; signatures are never verified.
    pub const fn eip8130_simulation(
        signed: Eip8130Signed,
        enveloped: Bytes,
        simulation: Eip8130Simulation,
    ) -> Self {
        Self::Eip8130(Eip8130Tx { signed, enveloped, simulation: Some(simulation) })
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
    pub const fn as_eip8130(&self) -> Option<&Eip8130Tx> {
        match self {
            Self::Eip8130(tx) => Some(tx),
            Self::Deposit(_) | Self::Standard { .. } => None,
        }
    }

    /// Returns the standard Ethereum transaction, if this envelope is a standard transaction.
    pub const fn as_standard(&self) -> Option<&TxEnvelope> {
        match self {
            Self::Standard { tx, .. } => Some(tx),
            Self::Deposit(_) | Self::Eip8130(_) => None,
        }
    }

    /// Returns the EIP-2718 encoded bytes of a standard or EIP-8130 transaction; `None` for a
    /// deposit, which pays no L1 data fee.
    pub const fn enveloped(&self) -> Option<&Bytes> {
        match self {
            Self::Standard { enveloped, .. } | Self::Eip8130(Eip8130Tx { enveloped, .. }) => {
                Some(enveloped)
            }
            Self::Deposit(_) => None,
        }
    }
}

impl Typed2718 for BaseTxEnvelope {
    fn ty(&self) -> u8 {
        match self {
            Self::Deposit(tx) => tx.ty(),
            Self::Standard { tx, .. } => tx.ty(),
            Self::Eip8130(tx) => tx.signed.ty(),
        }
    }
}

/// Forwards a [`Transaction`] method to the wrapped transaction of every variant.
macro_rules! delegate {
    ($self:ident.$method:ident($($arg:expr),*)) => {
        match $self {
            Self::Deposit(tx) => tx.$method($($arg),*),
            Self::Standard { tx, .. } => tx.$method($($arg),*),
            Self::Eip8130(tx) => tx.signed.$method($($arg),*),
        }
    };
}

impl Transaction for BaseTxEnvelope {
    fn chain_id(&self) -> Option<ChainId> {
        delegate!(self.chain_id())
    }

    fn nonce(&self) -> u64 {
        delegate!(self.nonce())
    }

    fn gas_limit(&self) -> u64 {
        delegate!(self.gas_limit())
    }

    fn gas_price(&self) -> Option<u128> {
        delegate!(self.gas_price())
    }

    fn max_fee_per_gas(&self) -> u128 {
        delegate!(self.max_fee_per_gas())
    }

    fn max_priority_fee_per_gas(&self) -> Option<u128> {
        delegate!(self.max_priority_fee_per_gas())
    }

    fn max_fee_per_blob_gas(&self) -> Option<u128> {
        delegate!(self.max_fee_per_blob_gas())
    }

    fn priority_fee_or_price(&self) -> u128 {
        delegate!(self.priority_fee_or_price())
    }

    fn effective_gas_price(&self, base_fee: Option<u64>) -> u128 {
        delegate!(self.effective_gas_price(base_fee))
    }

    fn is_dynamic_fee(&self) -> bool {
        delegate!(self.is_dynamic_fee())
    }

    fn kind(&self) -> TxKind {
        delegate!(self.kind())
    }

    fn is_create(&self) -> bool {
        delegate!(self.is_create())
    }

    fn value(&self) -> U256 {
        delegate!(self.value())
    }

    fn input(&self) -> &Bytes {
        delegate!(self.input())
    }

    fn access_list(&self) -> Option<&AccessList> {
        delegate!(self.access_list())
    }

    fn blob_versioned_hashes(&self) -> Option<&[B256]> {
        delegate!(self.blob_versioned_hashes())
    }

    fn authorization_list(&self) -> Option<&[SignedAuthorization]> {
        delegate!(self.authorization_list())
    }
}

impl From<TxDeposit> for BaseTxEnvelope {
    fn from(tx: TxDeposit) -> Self {
        Self::Deposit(tx)
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
        let enveloped = Bytes::from_static(&[EIP8130_TX_TYPE_ID, 0xc0]);
        let envelope = BaseTxEnvelope::eip8130(tx, enveloped.clone());
        assert_eq!(envelope.ty(), EIP8130_TX_TYPE_ID);
        assert!(envelope.as_eip8130().is_some_and(|tx| tx.simulation.is_none()));
        assert_eq!(envelope.gas_limit(), 250_000);
        // An EIP-8130 tx is neither a deposit nor a standard transaction, and its encoding prices
        // the L1 data fee and DA footprint.
        assert!(!envelope.is_deposit());
        assert!(envelope.as_standard().is_none());
        assert_eq!(envelope.enveloped(), Some(&enveloped));
    }
}
