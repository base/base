//! Canonical-state-driven exact-match mempool invalidation feeder.
//!
//! Reth's stock pool maintenance forwards account nonce and balance updates but
//! drops committed storage diffs. This task feeds complete canonical account
//! deltas into the EIP-8130 invalidation index.

use alloy_primitives::{B256, U256};
use futures::StreamExt;
use reth_provider::CanonStateNotification;
use revm::database::BundleState;
use tokio_stream::wrappers::{BroadcastStream, errors::BroadcastStreamRecvError};
use tracing::{debug, warn};

use crate::AccountStateDiff;

/// Applies canonical state diffs to a pool's EIP-8130 invalidation guard.
pub trait StateDiffInvalidation: Clone + Send + Sync + 'static {
    /// Invalidates guarded transactions affected by an executed state diff.
    fn invalidate_from_state_diff(&self, diffs: &[AccountStateDiff]) -> usize;

    /// Invalidates every guarded transaction after a state-feed gap.
    fn invalidate_all_tracked(&self) -> usize;
}

/// Feeds each canonical block's account and storage diff into the pool.
pub async fn maintain_state_diff_invalidation<P, N>(
    pool: P,
    mut events: BroadcastStream<CanonStateNotification<N>>,
) where
    P: StateDiffInvalidation,
    N: reth_node_api::NodePrimitives,
{
    loop {
        let notification = match events.next().await {
            Some(Ok(notification)) => notification,
            Some(Err(BroadcastStreamRecvError::Lagged(missed))) => {
                let removed = pool.invalidate_all_tracked();
                warn!(
                    missed = missed,
                    removed = removed,
                    "canonical state stream lagged; invalidated all guarded transactions"
                );
                continue;
            }
            None => break,
        };

        if matches!(&notification, CanonStateNotification::Reorg { .. }) {
            // The committed segment omits state changes caused solely by rolling
            // back the reverted segment. Correct targeted handling would need
            // both key sets, so conservatively flush this exceptional path.
            let removed = pool.invalidate_all_tracked();
            warn!(
                removed = removed,
                "canonical chain reorged; invalidated all guarded transactions"
            );
            continue;
        }

        let committed = notification.committed();
        let diffs = AccountStateDiff::collect(&committed.execution_outcome().bundle);
        if diffs.is_empty() {
            continue;
        }
        let removed = pool.invalidate_from_state_diff(&diffs);
        if removed > 0 {
            debug!(
                removed = removed,
                accounts = diffs.len(),
                "invalidated EIP-8130 transactions from canonical state diff"
            );
        }
    }
}

impl AccountStateDiff {
    /// Collects accounts that changed a watched canonical state surface.
    #[must_use]
    pub fn collect(bundle: &BundleState) -> Vec<Self> {
        let mut diffs = Vec::new();
        for (address, account) in &bundle.state {
            let new_balance = account.info.as_ref().map(|info| info.balance);
            let old_balance = account.original_info.as_ref().map(|info| info.balance);
            let balance = (new_balance != old_balance).then(|| new_balance.unwrap_or(U256::ZERO));

            let new_nonce = account.info.as_ref().map(|info| info.nonce);
            let old_nonce = account.original_info.as_ref().map(|info| info.nonce);
            let nonce_changed = new_nonce != old_nonce;

            let new_code_hash = account.info.as_ref().map(|info| info.code_hash);
            let old_code_hash = account.original_info.as_ref().map(|info| info.code_hash);
            let code_changed = new_code_hash != old_code_hash;

            let changed_slots = account
                .storage
                .iter()
                .filter(|(_, slot)| slot.is_changed())
                .map(|(key, _)| B256::from(*key))
                .collect::<Vec<_>>();

            if balance.is_some() || nonce_changed || code_changed || !changed_slots.is_empty() {
                diffs.push(Self {
                    address: *address,
                    balance,
                    nonce_changed,
                    code_changed,
                    changed_slots,
                });
            }
        }
        diffs
    }
}

#[cfg(test)]
mod tests {
    use alloy_primitives::Address;
    use revm::{
        database::{BundleAccount, states::StorageSlot},
        primitives::HashMap,
        state::AccountInfo,
    };

    use super::*;

    fn info(balance: u64, nonce: u64) -> AccountInfo {
        AccountInfo { balance: U256::from(balance), nonce, ..Default::default() }
    }

    fn account(
        original: Option<AccountInfo>,
        present: Option<AccountInfo>,
        slots: Vec<(U256, StorageSlot)>,
    ) -> BundleAccount {
        let mut account = BundleAccount {
            info: present,
            original_info: original,
            storage: Default::default(),
            status: revm::database::AccountStatus::Changed,
        };
        for (key, slot) in slots {
            account.storage.insert(key, slot);
        }
        account
    }

    fn bundle(entries: Vec<(Address, BundleAccount)>) -> BundleState {
        let mut state = HashMap::default();
        for (address, account) in entries {
            state.insert(address, account);
        }
        BundleState { state, ..Default::default() }
    }

    #[test]
    fn collects_balance_nonce_code_and_changed_slots() {
        let address = Address::repeat_byte(1);
        let mut original = info(100, 1);
        original.code_hash = B256::repeat_byte(2);
        let mut present = info(50, 2);
        present.code_hash = B256::repeat_byte(3);
        let changed =
            StorageSlot { previous_or_original_value: U256::ZERO, present_value: U256::from(1) };
        let unchanged =
            StorageSlot { previous_or_original_value: U256::from(2), present_value: U256::from(2) };
        let bundle = bundle(vec![(
            address,
            account(
                Some(original),
                Some(present),
                vec![(U256::from(4), changed), (U256::from(5), unchanged)],
            ),
        )]);

        let diffs = AccountStateDiff::collect(&bundle);
        assert_eq!(diffs.len(), 1);
        assert_eq!(diffs[0].balance, Some(U256::from(50)));
        assert!(diffs[0].nonce_changed);
        assert!(diffs[0].code_changed);
        assert_eq!(diffs[0].changed_slots, vec![B256::from(U256::from(4))]);
    }

    #[test]
    fn self_destruct_sets_balance_to_zero() {
        let address = Address::repeat_byte(1);
        let bundle = bundle(vec![(address, account(Some(info(100, 0)), None, vec![]))]);
        let diffs = AccountStateDiff::collect(&bundle);
        assert_eq!(diffs[0].balance, Some(U256::ZERO));
    }
}
