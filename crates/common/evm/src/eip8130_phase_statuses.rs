//! Handoff of EIP-8130 `phaseStatuses` from the executor to the caller of
//! [`Evm::transact`].
//!
//! The EIP-8130 executor computes a per-phase status array while running a
//! transaction's `calls`, but [`Evm::transact`] can only return a
//! [`ResultAndState`], whose `output` already carries the transaction's revert
//! data and so cannot be repurposed, and reth's block-executor / factory
//! machinery is generic over `E: Evm` and offers no type-safe channel for extra
//! per-transaction execution metadata.
//!
//! [`Eip8130PhaseStatuses`] bridges that gap with a thread-local slot whose
//! lifetime is a single synchronous [`Evm::transact`] call: the executor
//! [`set`]s the statuses as the last step of executing the transaction, and the
//! code that called `transact` must [`take`] them on the same thread as soon as
//! the call returns, before executing anything else. From there the statuses
//! travel explicitly with the execution result
//! ([`BaseTxResult::eip8130_phase_statuses`]) and are passed to the receipt
//! builder ([`BaseReceiptBuilder::build_receipt`]), so the receipt may be built
//! on a different thread from the one that executed the transaction. The receipt
//! builder never reads the slot.
//!
//! # Panic safety
//!
//! The slot is global mutable state, so a panic between [`set`] and [`take`] (for
//! instance, one caught by a `catch_unwind` in a parallel-execution context)
//! could otherwise leave one transaction's statuses behind for the next
//! transaction on the same worker thread to misattribute. The executor closes
//! this by calling [`clear`] at the very start of every `execute`, so the slot
//! only ever reflects the in-flight transaction: any value leaked by an earlier
//! transaction is discarded before the current one runs, and `set` publishes the
//! current statuses as `execute`'s last step.
//!
//! [`Evm::transact`]: alloy_evm::Evm::transact
//! [`ResultAndState`]: revm::context_interface::result::ResultAndState
//! [`BaseTxResult::eip8130_phase_statuses`]: crate::BaseTxResult::eip8130_phase_statuses
//! [`BaseReceiptBuilder::build_receipt`]: crate::BaseReceiptBuilder::build_receipt
//! [`set`]: Eip8130PhaseStatuses::set
//! [`take`]: Eip8130PhaseStatuses::take
//! [`clear`]: Eip8130PhaseStatuses::clear

use alloc::vec::Vec;

#[cfg(feature = "std")]
std::thread_local! {
    /// Per-thread slot holding the most recently executed EIP-8130 transaction's
    /// per-phase statuses, awaiting consumption by the caller of `Evm::transact`.
    static PHASE_STATUSES: core::cell::RefCell<Vec<u8>> = const { core::cell::RefCell::new(Vec::new()) };
}

/// Thread-local handoff for EIP-8130 per-phase statuses between the executor and
/// the caller of `Evm::transact`. See the [module docs](self) for the contract.
#[derive(Debug, Clone, Copy)]
#[non_exhaustive]
pub struct Eip8130PhaseStatuses;

impl Eip8130PhaseStatuses {
    /// Clears any statuses left in the slot, called at the start of `execute` so a
    /// value leaked by an earlier transaction (e.g. a panic caught between a prior
    /// [`Self::set`] and its [`Self::take`]) can never be misattributed to the
    /// current transaction's receipt. No-op in `no_std` builds.
    #[cfg(feature = "std")]
    pub fn clear() {
        PHASE_STATUSES.with(|cell| cell.borrow_mut().clear());
    }

    /// Clears any statuses left in the slot, called at the start of `execute` so a
    /// value leaked by an earlier transaction (e.g. a panic caught between a prior
    /// [`Self::set`] and its [`Self::take`]) can never be misattributed to the
    /// current transaction's receipt. No-op in `no_std` builds.
    #[cfg(not(feature = "std"))]
    pub const fn clear() {}

    /// Records the per-phase statuses of the EIP-8130 transaction just executed
    /// on this thread, to be consumed by the next [`Self::take`] once `transact`
    /// returns. No-op in `no_std` builds (where EIP-8130 execution is disabled).
    pub fn set(statuses: Vec<u8>) {
        #[cfg(feature = "std")]
        PHASE_STATUSES.with(|cell| *cell.borrow_mut() = statuses);
        #[cfg(not(feature = "std"))]
        {
            // EIP-8130 execution is `std`-gated, so the only caller never runs in
            // `no_std`; statuses are therefore always empty here. Guard against a
            // future caller silently dropping real data (which `take` could not
            // recover) rather than failing loudly.
            debug_assert!(
                statuses.is_empty(),
                "EIP-8130 phase statuses dropped in a no_std build; take() would return empty"
            );
            let _ = statuses;
        }
    }

    /// Takes (and clears) the per-phase statuses recorded by the most recent
    /// [`Self::set`] on this thread. Returns an empty vector in `no_std` builds.
    #[cfg(feature = "std")]
    pub fn take() -> Vec<u8> {
        PHASE_STATUSES.with(|cell| core::mem::take(&mut *cell.borrow_mut()))
    }

    /// Takes (and clears) the per-phase statuses recorded by the most recent
    /// [`Self::set`] on this thread. Returns an empty vector in `no_std` builds.
    #[cfg(not(feature = "std"))]
    pub const fn take() -> Vec<u8> {
        Vec::new()
    }
}
