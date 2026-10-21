//! Handoff of the EIP-8130 committed phase count from the executor to the caller
//! of [`Evm::transact`].
//!
//! The EIP-8130 executor counts the call phases that committed while running a
//! transaction's `calls`, but [`Evm::transact`] can only return a
//! [`ResultAndState`], whose `output` already carries the transaction's revert
//! data and so cannot be repurposed, and reth's block-executor / factory
//! machinery is generic over `E: Evm` and offers no type-safe channel for extra
//! per-transaction execution metadata.
//!
//! [`Eip8130CommittedPhases`] bridges that gap with a thread-local slot whose
//! lifetime is a single synchronous [`Evm::transact`] call: the executor
//! [`set`]s the count as the last step of executing the transaction, and the
//! code that called `transact` must [`take`] it on the same thread as soon as
//! the call returns, before executing anything else. From there the count
//! travels explicitly with the execution result
//! ([`BaseTxResult::eip8130_committed_phases`]) and is passed to the receipt
//! builder ([`BaseReceiptBuilder::build_receipt`]), so the receipt may be built
//! on a different thread from the one that executed the transaction. The receipt
//! builder never reads the slot.
//!
//! # Panic safety
//!
//! The slot is global mutable state, so a panic between [`set`] and [`take`] (for
//! instance, one caught by a `catch_unwind` in a parallel-execution context)
//! could otherwise leave one transaction's count behind for the next
//! transaction on the same worker thread to misattribute. The executor closes
//! this by calling [`clear`] at the very start of every `execute`, so the slot
//! only ever reflects the in-flight transaction: any value leaked by an earlier
//! transaction is discarded before the current one runs, and `set` publishes the
//! current count as `execute`'s last step.
//!
//! [`Evm::transact`]: alloy_evm::Evm::transact
//! [`ResultAndState`]: revm::context_interface::result::ResultAndState
//! [`BaseTxResult::eip8130_committed_phases`]: crate::BaseTxResult::eip8130_committed_phases
//! [`BaseReceiptBuilder::build_receipt`]: crate::BaseReceiptBuilder::build_receipt
//! [`set`]: Eip8130CommittedPhases::set
//! [`take`]: Eip8130CommittedPhases::take
//! [`clear`]: Eip8130CommittedPhases::clear

#[cfg(feature = "std")]
std::thread_local! {
    /// Per-thread slot holding the most recently executed EIP-8130 transaction's
    /// committed phase count, awaiting consumption by the caller of `Evm::transact`.
    static COMMITTED_PHASES: core::cell::Cell<u64> = const { core::cell::Cell::new(0) };
}

/// Thread-local handoff for the EIP-8130 committed phase count between the
/// executor and the caller of `Evm::transact`. See the [module docs](self) for
/// the contract.
#[derive(Debug, Clone, Copy)]
#[non_exhaustive]
pub struct Eip8130CommittedPhases;

impl Eip8130CommittedPhases {
    /// Clears any count left in the slot, called at the start of `execute` so a
    /// value leaked by an earlier transaction (e.g. a panic caught between a prior
    /// [`Self::set`] and its [`Self::take`]) can never be misattributed to the
    /// current transaction's receipt. No-op in `no_std` builds.
    #[cfg(feature = "std")]
    pub fn clear() {
        COMMITTED_PHASES.with(|cell| cell.set(0));
    }

    /// Clears any count left in the slot, called at the start of `execute` so a
    /// value leaked by an earlier transaction (e.g. a panic caught between a prior
    /// [`Self::set`] and its [`Self::take`]) can never be misattributed to the
    /// current transaction's receipt. No-op in `no_std` builds.
    #[cfg(not(feature = "std"))]
    pub const fn clear() {}

    /// Records the committed phase count of the EIP-8130 transaction just
    /// executed on this thread, to be consumed by the next [`Self::take`] once
    /// `transact` returns. No-op in `no_std` builds (where EIP-8130 execution is
    /// disabled).
    pub fn set(committed_phases: u64) {
        #[cfg(feature = "std")]
        COMMITTED_PHASES.with(|cell| cell.set(committed_phases));
        #[cfg(not(feature = "std"))]
        {
            // EIP-8130 execution is `std`-gated, so the only caller never runs in
            // `no_std`; the count is therefore always zero here. Guard against a
            // future caller silently dropping real data (which `take` could not
            // recover) rather than failing loudly.
            debug_assert_eq!(
                committed_phases, 0,
                "EIP-8130 committed phase count dropped in a no_std build; take() would return 0"
            );
            let _ = committed_phases;
        }
    }

    /// Takes (and clears) the committed phase count recorded by the most recent
    /// [`Self::set`] on this thread. Returns `0` in `no_std` builds.
    #[cfg(feature = "std")]
    pub fn take() -> u64 {
        COMMITTED_PHASES.with(|cell| cell.replace(0))
    }

    /// Takes (and clears) the committed phase count recorded by the most recent
    /// [`Self::set`] on this thread. Returns `0` in `no_std` builds.
    #[cfg(not(feature = "std"))]
    pub const fn take() -> u64 {
        0
    }
}
