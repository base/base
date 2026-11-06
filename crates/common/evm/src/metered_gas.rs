//! Handoff of a transaction's metered gas from the EVM to the caller of
//! [`Evm::transact`], for builder telemetry under gas-limit billing.
//!
//! Gas-limit billing ([`BaseFeature::GasLimitBilling`]) bills a transaction for
//! the gas it reserved, and every consensus surface (`ResultGas`, receipts, the
//! header) then reports the reserved gas. The gas the transaction really
//! consumed is still metered during execution but is discarded at settlement.
//! [`MeteredGas`] preserves it, per transaction, for the builder's transaction
//! events, which record it next to the billed gas so the data team can measure
//! how limits relate to consumption without re-simulating every transaction.
//!
//! The mechanism mirrors [`Eip8130CommittedPhases`]: [`Evm::transact`] can only
//! return a [`ResultAndState`], and reth's executor machinery is generic over
//! `E: Evm` with no channel for extra per-transaction metadata, so the value is
//! published through a thread-local slot whose lifetime is a single synchronous
//! `transact` call. The handler ([`BaseHandler::post_execution`]) or the EIP-8130
//! executor [`set`]s it as part of executing the transaction, and the caller of
//! `transact` [`take`]s it on the same thread as soon as the call returns, before
//! executing anything else. Nothing on the consensus path reads the slot: the
//! builder and the validator still compute identical receipts and headers.
//!
//! # Panic safety
//!
//! A panic between [`set`] and [`take`] could leave one transaction's value
//! behind for the next transaction on the same thread. Both execution paths call
//! [`clear`] as their first step ([`BaseHandler::validate_env`] and
//! [`Eip8130Executor::execute`]), so the slot only ever reflects the in-flight
//! transaction, and [`take`] returns `None` for a transaction that never reached
//! settlement, such as a deposit that failed and was included as a halt.
//!
//! [`Evm::transact`]: alloy_evm::Evm::transact
//! [`ResultAndState`]: revm::context_interface::result::ResultAndState
//! [`BaseFeature::GasLimitBilling`]: base_common_genesis::BaseFeature::GasLimitBilling
//! [`BaseHandler::post_execution`]: revm::handler::Handler::post_execution
//! [`BaseHandler::validate_env`]: revm::handler::Handler::validate_env
//! [`Eip8130CommittedPhases`]: crate::Eip8130CommittedPhases
//! [`Eip8130Executor::execute`]: crate::Eip8130Executor::execute
//! [`set`]: MeteredGas::set
//! [`take`]: MeteredGas::take
//! [`clear`]: MeteredGas::clear

#[cfg(feature = "std")]
std::thread_local! {
    /// Per-thread slot holding the most recently executed transaction's metered
    /// gas, awaiting consumption by the caller of `Evm::transact`.
    static METERED_GAS: core::cell::Cell<Option<MeteredGas>> = const { core::cell::Cell::new(None) };
}

/// The gas a transaction metered, as opposed to the gas it was billed and
/// reports under gas-limit billing. The module docs describe the handoff
/// contract.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MeteredGas {
    /// The gas the transaction would be charged, and its receipt would report,
    /// without gas-limit billing: gas spent less the EIP-3529-capped refund,
    /// raised to the EIP-7623 floor. For an EIP-8130 transaction this includes
    /// payer authentication. Equal to the billed gas before the feature activates.
    pub gas_used: u64,
    /// For an EIP-8130 transaction, the payer-authentication gas reserved and
    /// billed on top of the sender's `gas_limit` (`payer_floor`: the `payer_auth`
    /// bytes at the EIP-7623 floor rate), so that under gas-limit billing the
    /// billed gas is exactly `gas_limit + payer_auth_gas_reserved`. `None` for every other
    /// transaction type, whose billed gas is `gas_limit` alone.
    pub payer_auth_gas_reserved: Option<u64>,
}

impl MeteredGas {
    /// The metered gas of a transaction with no payer authentication.
    pub const fn new(gas_used: u64) -> Self {
        Self { gas_used, payer_auth_gas_reserved: None }
    }

    /// Clears any value left in the slot, called at the start of every
    /// transaction so a value leaked by an earlier one (e.g. a panic caught
    /// between a prior [`Self::set`] and its [`Self::take`]) can never be
    /// misattributed to the current transaction. No-op in `no_std` builds.
    #[cfg(feature = "std")]
    pub fn clear() {
        METERED_GAS.with(|cell| cell.set(None));
    }

    /// Clears any value left in the slot, called at the start of every
    /// transaction so a value leaked by an earlier one (e.g. a panic caught
    /// between a prior [`Self::set`] and its [`Self::take`]) can never be
    /// misattributed to the current transaction. No-op in `no_std` builds.
    #[cfg(not(feature = "std"))]
    pub const fn clear() {}

    /// Records the metered gas of the transaction being executed on this thread,
    /// to be consumed by the next [`Self::take`] once `transact` returns. No-op
    /// in `no_std` builds, which have no builder to read it.
    pub fn set(self) {
        #[cfg(feature = "std")]
        METERED_GAS.with(|cell| cell.set(Some(self)));
        #[cfg(not(feature = "std"))]
        let _ = self;
    }

    /// Takes (and clears) the metered gas recorded by the most recent
    /// [`Self::set`] on this thread, or `None` when the transaction executed
    /// since the last [`Self::clear`] never reached settlement. Returns `None` in
    /// `no_std` builds.
    #[cfg(feature = "std")]
    pub fn take() -> Option<Self> {
        METERED_GAS.with(|cell| cell.take())
    }

    /// Takes (and clears) the metered gas recorded by the most recent
    /// [`Self::set`] on this thread, or `None` when the transaction executed
    /// since the last [`Self::clear`] never reached settlement. Returns `None` in
    /// `no_std` builds.
    #[cfg(not(feature = "std"))]
    pub const fn take() -> Option<Self> {
        None
    }
}

#[cfg(all(test, feature = "std"))]
mod tests {
    use super::*;

    #[test]
    fn take_consumes_the_published_value_once() {
        MeteredGas::clear();
        assert_eq!(MeteredGas::take(), None);

        MeteredGas { gas_used: 21_000, payer_auth_gas_reserved: Some(7) }.set();
        assert_eq!(
            MeteredGas::take(),
            Some(MeteredGas { gas_used: 21_000, payer_auth_gas_reserved: Some(7) })
        );
        assert_eq!(MeteredGas::take(), None, "a value is read by exactly one caller");
    }

    #[test]
    fn clear_discards_a_value_left_by_an_earlier_transaction() {
        MeteredGas::new(42).set();
        MeteredGas::clear();
        assert_eq!(MeteredGas::take(), None);
    }

    #[test]
    fn later_set_replaces_an_unread_value() {
        MeteredGas::new(1).set();
        MeteredGas::new(2).set();
        assert_eq!(MeteredGas::take(), Some(MeteredGas::new(2)));
    }
}
