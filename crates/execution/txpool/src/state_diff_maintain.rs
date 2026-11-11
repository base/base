//! Canonical-state-driven exact-match mempool invalidation feeder.
//!
//! Reth's stock pool maintenance forwards account nonce and balance updates but
//! drops committed storage diffs. This task feeds complete canonical account
//! deltas into the EIP-8130 invalidation index.

use alloy_primitives::{B256, U256};
use futures::StreamExt;
use reth_evm::BundleState;
use reth_provider::CanonStateNotification;
use tokio_stream::wrappers::{BroadcastStream, errors::BroadcastStreamRecvError};
use tracing::{debug, warn};

use crate::AccountStateDiff;

/// Why every guarded transaction was flushed in one shot. Both paths are
/// fail-safe (the guard cannot prove any admission current), but they differ
/// wildly in frequency, so they carry distinct metric labels.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InvalidationCause {
    /// A canonical chain reorg. The committed segment omits state changes caused
    /// solely by rolling back the reverted segment, so targeted invalidation is
    /// not possible. Should effectively never happen on Base — a reorg signals
    /// something has gone seriously wrong upstream, so this label is a strong
    /// alerting signal.
    Reorg,
    /// A gap in the canonical-state feed (the broadcast stream lagged and
    /// dropped notifications). Uncommon, but more likely than a [`Self::Reorg`],
    /// and likewise a signal worth alerting on.
    FeedGap,
}

impl InvalidationCause {
    /// Low-cardinality static metric label.
    #[must_use]
    pub const fn as_label(self) -> &'static str {
        match self {
            Self::Reorg => "reorg",
            Self::FeedGap => "feed_gap",
        }
    }
}

/// Applies canonical state diffs to a pool's EIP-8130 invalidation guard.
pub trait StateDiffInvalidation: Clone + Send + Sync + 'static {
    /// Invalidates guarded transactions affected by an executed state diff.
    fn invalidate_from_state_diff(&self, diffs: &[AccountStateDiff]) -> usize;

    /// Invalidates every guarded transaction, attributing the flush to `cause`.
    fn invalidate_all_tracked(&self, cause: InvalidationCause) -> usize;
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
                let removed = pool.invalidate_all_tracked(InvalidationCause::FeedGap);
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
            let removed = pool.invalidate_all_tracked(InvalidationCause::Reorg);
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
            let nonce = (new_nonce != old_nonce).then(|| new_nonce.unwrap_or_default());

            let new_code_hash = account.info.as_ref().map(|info| info.code_hash);
            let old_code_hash = account.original_info.as_ref().map(|info| info.code_hash);
            let code_changed = new_code_hash != old_code_hash;

            let changed_slots = account
                .storage
                .iter()
                .filter(|(_, slot)| slot.is_changed())
                .map(|(key, slot)| (B256::from(*key), slot.present_value))
                .collect::<Vec<_>>();

            if balance.is_some() || nonce.is_some() || code_changed || !changed_slots.is_empty() {
                diffs.push(Self { address: *address, balance, nonce, code_changed, changed_slots });
            }
        }
        diffs
    }
}

#[cfg(test)]
mod tests {
    use alloy_primitives::Address;
    use evm2::evm::AccountInfo;
    use reth_trie_common::bundle_state::{AccountStatus, BundleAccount, StorageSlot};

    use super::*;

    fn info(balance: u64, nonce: u64) -> AccountInfo {
        AccountInfo::default().with_balance(U256::from(balance)).with_nonce(nonce)
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
            status: AccountStatus::Changed,
        };
        for (key, slot) in slots {
            account.storage.insert(key, slot);
        }
        account
    }

    fn bundle(entries: Vec<(Address, BundleAccount)>) -> BundleState {
        BundleState { state: entries.into_iter().collect(), ..Default::default() }
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
        assert_eq!(diffs[0].nonce, Some(2));
        assert!(diffs[0].code_changed);
        assert_eq!(diffs[0].changed_slots, vec![(B256::from(U256::from(4)), U256::from(1))]);
    }

    #[test]
    fn self_destruct_sets_balance_to_zero() {
        let address = Address::repeat_byte(1);
        let bundle = bundle(vec![(address, account(Some(info(100, 0)), None, vec![]))]);
        let diffs = AccountStateDiff::collect(&bundle);
        assert_eq!(diffs[0].balance, Some(U256::ZERO));
    }
}
