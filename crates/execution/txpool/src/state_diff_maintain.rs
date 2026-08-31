//! Canonical-state-driven exact-match mempool invalidation feeder.
//!
//! Reth's stock pool maintenance forwards account nonce and balance updates but
//! drops committed storage diffs. This task feeds complete canonical account
//! deltas into the EIP-8130 invalidation index.

use alloy_consensus::transaction::TxHashRef;
use alloy_primitives::{B256, TxHash, U256};
use base_common_precompiles::NonceManagerStorage;
use futures::StreamExt;
use reth_primitives_traits::BlockBody;
use reth_provider::CanonStateNotification;
use revm::database::BundleState;
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

/// Source of a state diff, which controls speculative-prune reconciliation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StateDiffOrigin {
    /// Diff from a sealed canonical block range.
    Canonical,
    /// Diff between unsealed flashblocks in the active speculative generation.
    IntraBlock,
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
    fn invalidate_from_state_diff(
        &self,
        diffs: &[AccountStateDiff],
        mined_hashes: &[TxHash],
        origin: StateDiffOrigin,
    ) -> usize;

    /// Invalidates every guarded transaction, attributing the flush to `cause`.
    fn invalidate_all_tracked(&self, cause: InvalidationCause) -> usize {
        self.invalidate_all_tracked_excluding(cause, &[])
    }

    /// Invalidates guarded transactions except hashes mined on the new canonical branch.
    fn invalidate_all_tracked_excluding(
        &self,
        cause: InvalidationCause,
        mined_hashes: &[TxHash],
    ) -> usize;
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

        let committed = notification.committed();
        let mined_hashes = committed
            .blocks_iter()
            .flat_map(|block| {
                block.body().transactions().iter().map(|transaction| *transaction.tx_hash())
            })
            .collect::<Vec<_>>();

        if matches!(&notification, CanonStateNotification::Reorg { .. }) {
            // The committed segment omits state changes caused solely by rolling
            // back the reverted segment. Correct targeted handling would need
            // both key sets, so conservatively flush this exceptional path.
            let removed =
                pool.invalidate_all_tracked_excluding(InvalidationCause::Reorg, &mined_hashes);
            warn!(
                removed = removed,
                "canonical chain reorged; invalidated all guarded transactions"
            );
            continue;
        }

        let diffs = AccountStateDiff::collect(&committed.execution_outcome().bundle);
        if diffs.is_empty() {
            continue;
        }
        let removed =
            pool.invalidate_from_state_diff(&diffs, &mined_hashes, StateDiffOrigin::Canonical);
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

    /// Collects watched state changes between flashblocks.
    ///
    /// Protocol and channel nonce advances are omitted because the builder has
    /// already pruned each included transaction, promoting the valid successor
    /// in that pool lane. Treating an included transaction's nonce advance as
    /// external invalidation would immediately evict that successor. This is a
    /// deliberately coarse intra-block filter: canonical invalidation remains
    /// conservative for unrelated protocol-nonce or nonce-manager writes.
    #[must_use]
    pub fn collect_for_intra_block(bundle: &BundleState) -> Vec<Self> {
        Self::collect(bundle)
            .into_iter()
            .filter_map(|mut diff| {
                diff.nonce_changed = false;
                if diff.address == NonceManagerStorage::ADDRESS {
                    diff.changed_slots.clear();
                }
                (diff.balance.is_some() || diff.code_changed || !diff.changed_slots.is_empty())
                    .then_some(diff)
            })
            .collect()
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

    #[test]
    fn intra_block_diff_omits_protocol_and_channel_nonce_advances() {
        let sender = Address::repeat_byte(2);
        let nonce_slot = U256::from(9);
        let changed_slot =
            StorageSlot { previous_or_original_value: U256::ZERO, present_value: U256::from(1) };
        let bundle = bundle(vec![
            (sender, account(Some(info(100, 1)), Some(info(100, 2)), vec![])),
            (
                NonceManagerStorage::ADDRESS,
                account(Some(info(0, 0)), Some(info(0, 0)), vec![(nonce_slot, changed_slot)]),
            ),
        ]);

        assert!(
            AccountStateDiff::collect_for_intra_block(&bundle).is_empty(),
            "included nonce advances must not evict newly promoted successors"
        );
    }

    #[test]
    fn intra_block_diff_retains_non_nonce_surfaces() {
        let account_address = Address::repeat_byte(3);
        let contract = Address::repeat_byte(4);
        let contract_slot = U256::from(10);
        let changed_slot =
            StorageSlot { previous_or_original_value: U256::ZERO, present_value: U256::from(1) };
        let bundle = bundle(vec![
            (account_address, account(Some(info(100, 1)), Some(info(50, 2)), vec![])),
            (
                NonceManagerStorage::ADDRESS,
                account(Some(info(100, 0)), Some(info(50, 0)), vec![(U256::from(9), changed_slot)]),
            ),
            (
                contract,
                account(
                    Some(info(100, 0)),
                    Some(info(100, 0)),
                    vec![(contract_slot, changed_slot)],
                ),
            ),
        ]);

        let diffs = AccountStateDiff::collect_for_intra_block(&bundle);
        let account_diff = diffs
            .iter()
            .find(|diff| diff.address == account_address)
            .expect("balance diff retained");
        assert_eq!(account_diff.balance, Some(U256::from(50)));
        assert!(!account_diff.nonce_changed);

        let nonce_manager_diff = diffs
            .iter()
            .find(|diff| diff.address == NonceManagerStorage::ADDRESS)
            .expect("nonce manager balance diff retained");
        assert_eq!(nonce_manager_diff.balance, Some(U256::from(50)));
        assert!(nonce_manager_diff.changed_slots.is_empty());

        let contract_diff =
            diffs.iter().find(|diff| diff.address == contract).expect("contract slot retained");
        assert_eq!(contract_diff.changed_slots, vec![B256::from(contract_slot)]);
    }
}
