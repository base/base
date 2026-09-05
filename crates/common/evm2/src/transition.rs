//! Irregular (non-transaction) state transitions for Base block boundaries.
//!
//! The Canyon, Cobalt, and Zenith block-boundary hooks apply *irregular* state changes — direct
//! edits to account code and storage that are not the result of executing a transaction. This
//! module provides the two pieces they share: [`BaseForkActivations`], the revm-free fork-schedule
//! input used to detect activation blocks, and [`IrregularStateChange`], which persists an edit
//! into the EVM's accepted overlay and records it in the block-state delta in one step (the evm2
//! analogue of the reference's `DatabaseCommit::commit`).

use alloy_primitives::{Address, U256};
use base_common_genesis::{BaseUpgrade, UpgradeConfig};
use evm2::{
    AccountInfo, Evm,
    evm::{
        AccountChangeRef, BlockStateAccumulator, StateChangeSink, StateChangeSource, StorageChange,
    },
};

use crate::BaseEvmTypes;

/// A revm-free view of a Base fork schedule, sufficient for the block-boundary transition hooks to
/// detect whether an upgrade is active at a given L2 timestamp.
///
/// Implemented for [`UpgradeConfig`] (the genesis activation schedule) so callers can drive the
/// hooks without pulling in `base-common-chains`/revm. Node integrations that hold a richer
/// schedule type can implement this trait for it.
pub trait BaseForkActivations {
    /// Returns whether `upgrade` is active at `timestamp`.
    fn is_active_at_timestamp(&self, upgrade: BaseUpgrade, timestamp: u64) -> bool;
}

impl BaseForkActivations for UpgradeConfig {
    fn is_active_at_timestamp(&self, upgrade: BaseUpgrade, timestamp: u64) -> bool {
        self.activation_timestamp(upgrade).is_some_and(|activation| timestamp >= activation)
    }
}

impl<T: BaseForkActivations> BaseForkActivations for &T {
    fn is_active_at_timestamp(&self, upgrade: BaseUpgrade, timestamp: u64) -> bool {
        (*self).is_active_at_timestamp(upgrade, timestamp)
    }
}

/// A single account's irregular state change: its pre-change account info, post-change account
/// info (with optional bytecode), and any storage-slot writes.
///
/// Applying it via [`apply`](Self::apply) both persists it to the EVM's accepted overlay (so later
/// transactions in the block observe it) and records it in the [`BlockStateAccumulator`] (so it
/// appears in the block's state delta) — mirroring the reference `DatabaseCommit::commit` of a
/// touched account, which evm2 has no single-call equivalent for.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct IrregularStateChange {
    address: Address,
    original: Option<AccountInfo>,
    current: Option<AccountInfo>,
    storage: Vec<StorageChange>,
}

impl IrregularStateChange {
    /// Creates a change to `address` from `original` (its pre-change account, `None` if absent) to
    /// `current` (its post-change account, `None` to delete).
    pub const fn new(
        address: Address,
        original: Option<AccountInfo>,
        current: Option<AccountInfo>,
    ) -> Self {
        Self { address, original, current, storage: Vec::new() }
    }

    /// Records a storage-slot write on this account, from `original` to `current`.
    pub fn with_storage(mut self, key: U256, original: U256, current: U256) -> Self {
        self.storage.push(StorageChange { address: self.address, key, original, current });
        self
    }

    /// Persists this change to `evm`'s accepted overlay and records it in `block_state`.
    pub fn apply(&self, evm: &mut Evm<'_, BaseEvmTypes>, block_state: &mut BlockStateAccumulator) {
        evm.commit_source(self);
        // `BlockStateAccumulator`'s sink is infallible.
        let Ok(()) = self.visit(block_state);
    }
}

impl StateChangeSource for IrregularStateChange {
    fn visit<S: StateChangeSink>(&self, sink: &mut S) -> Result<(), S::Error> {
        // Register bytecode first so the code hash the account references resolves in the sink.
        if let Some(current) = &self.current
            && let Some(code) = &current.code
            && !code.is_empty()
        {
            sink.bytecode(current.code_hash, code)?;
        }
        sink.account(AccountChangeRef {
            address: self.address,
            original: self.original.as_ref(),
            current: self.current.as_ref(),
            created: false,
            selfdestructed: false,
        })?;
        for change in &self.storage {
            sink.storage(*change)?;
        }
        Ok(())
    }
}
