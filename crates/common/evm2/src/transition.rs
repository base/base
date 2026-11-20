//! Irregular (non-transaction) state transitions for Base block boundaries.
//!
//! The Canyon, Denim, and Everest block-boundary hooks apply *irregular* state changes — direct
//! edits to account code and storage that are not the result of executing a transaction. This
//! module provides the pieces they share: [`BaseForkActivations`], the revm-free fork-schedule
//! input used to detect activation blocks; [`BaseBlockState`], the destination every block state
//! change is committed through; and [`IrregularStateChange`], which commits an edit through it
//! (the evm2 analogue of the reference's `DatabaseCommit::commit`).

use alloy_primitives::{Address, U256};
use base_common_genesis::{BaseFeature, BaseUpgrade, UpgradeConfig};
use evm2::{
    AccountInfo, Evm,
    evm::{BlockStateAccumulator, PendingState, StateChangeSource},
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

    /// Whether a behavior is active according to this schedule's existing activation rules.
    ///
    /// The [`UpgradeConfig`] implementation checks only explicitly configured timestamps; it
    /// does not cascade later upgrades into earlier ones. Bedrock features always return false
    /// because Bedrock has no timestamp slot. This is not interchangeable with rollup cascading
    /// or resolved EVM-spec feature checks.
    fn is_feature_active_at_timestamp(&self, feature: BaseFeature, timestamp: u64) -> bool {
        self.is_active_at_timestamp(feature.activation_upgrade(), timestamp)
    }
}

impl BaseForkActivations for UpgradeConfig {
    fn is_active_at_timestamp(&self, upgrade: BaseUpgrade, timestamp: u64) -> bool {
        self.activation_timestamp(upgrade).is_some_and(|activation| timestamp >= activation)
    }
}

/// Accepts state into a block: commits it to the EVM's accepted overlay, so later transactions
/// observe it, and records it in the block's state delta.
///
/// Every state change a Base block makes — pre-execution system calls, transition-block irregular
/// changes, and transactions — reaches the block through this trait, so an integration can keep
/// its own block-state representation (for example, one that also streams per-transaction state
/// updates) without re-implementing the block's execution rules.
pub trait BaseBlockState {
    /// Commits `state` to `evm`'s accepted overlay and records it in this block state.
    fn commit(&mut self, evm: &mut Evm<'_, BaseEvmTypes>, state: PendingState);
}

impl BaseBlockState for BlockStateAccumulator {
    fn commit(&mut self, evm: &mut Evm<'_, BaseEvmTypes>, state: PendingState) {
        // `BlockStateAccumulator`'s sink is infallible.
        let Ok(()) = state.visit(self);
        evm.state_mut().set_pending_state(state);
        evm.state_mut().commit_transaction();
    }
}

/// A single account's irregular state change: its pre-change account info, post-change account
/// info (with optional bytecode), and any storage-slot writes.
///
/// Applying it via [`apply`](Self::apply) commits it like a transaction's state: later
/// transactions in the block observe it, and it appears in the block's state delta — mirroring
/// the reference `DatabaseCommit::commit` of a touched account.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct IrregularStateChange {
    address: Address,
    original: Option<AccountInfo>,
    current: Option<AccountInfo>,
    storage: Vec<(U256, U256, U256)>,
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
        self.storage.push((key, original, current));
        self
    }

    /// Commits this change through `block_state`.
    pub fn apply(self, evm: &mut Evm<'_, BaseEvmTypes>, block_state: &mut impl BaseBlockState) {
        let mut state = PendingState::default();
        state.insert_account(self.address, self.original, self.current);
        for (key, original, current) in self.storage {
            state.insert_storage(self.address, key, original, current);
        }
        block_state.commit(evm, state);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn configured_features_use_explicit_timestamps_without_cascading() {
        let timestamps = [0, 49, 50, 99, 100, 101, u64::MAX];
        for (canyon_time, expected_canyon) in [
            (None, [false; 7]),
            (Some(0), [true; 7]),
            (Some(100), [false, false, false, false, true, true, true]),
        ] {
            let schedule =
                UpgradeConfig { canyon_time, delta_time: Some(50), ..Default::default() };
            for ((timestamp, canyon_active), delta_active) in timestamps
                .into_iter()
                .zip(expected_canyon)
                .zip([false, false, true, true, true, true, true])
            {
                assert_eq!(
                    schedule.is_feature_active_at_timestamp(
                        BaseFeature::DepositReceiptVersion,
                        timestamp,
                    ),
                    canyon_active,
                );
                assert_eq!(
                    schedule.is_feature_active_at_timestamp(BaseFeature::SpanBatches, timestamp),
                    delta_active,
                );
                assert!(!schedule.is_feature_active_at_timestamp(
                    BaseFeature::MonotonicBlockTimestamps,
                    timestamp,
                ));
            }
        }
    }
}
