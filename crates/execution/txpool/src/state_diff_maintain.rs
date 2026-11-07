//! Canonical-state-driven exact-match mempool invalidation feeder.
//!
//! Reth's stock pool maintenance forwards account nonce and balance updates but
//! drops committed storage diffs. This task feeds complete canonical account
//! deltas into the EIP-8130 invalidation index.

use alloy_eips::BlockNumHash;
use alloy_primitives::{B256, U256};
use base_common_precompiles::NonceManagerStorage;
use futures::StreamExt;
use reth_provider::CanonStateNotification;
use revm::database::BundleState;
use tokio_stream::wrappers::{BroadcastStream, errors::BroadcastStreamRecvError};
use tracing::{debug, warn};

use crate::{AccountStateDiff, DormancyDiscardReason};

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

    /// Whether the pool keeps dormant validity transactions. Without it the maintenance task
    /// skips every dormancy call, and the work and blocking-thread hand-off before them.
    fn tracks_dormancy(&self) -> bool;

    /// Advances dormant validity transactions past the `canonical` blocks a notification
    /// committed, in ascending order: checks them against the builder's tip and classifies
    /// queued transactions against the head's state. Called for every notification, including
    /// those committing only empty blocks, while the pool tracks dormancy.
    fn advance_dormancy(&self, canonical: &[BlockNumHash]);

    /// Discards the dormancy index for `reason`, making every dormant validity transaction ready.
    fn discard_dormancy(&self, reason: DormancyDiscardReason);
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
                if pool.tracks_dormancy() {
                    run_blocking(&pool, |pool| {
                        pool.discard_dormancy(DormancyDiscardReason::FeedGap);
                    })
                    .await;
                }
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
            if pool.tracks_dormancy() {
                run_blocking(&pool, |pool| pool.discard_dormancy(DormancyDiscardReason::Reorg))
                    .await;
            }
            warn!(
                removed = removed,
                "canonical chain reorged; invalidated all guarded transactions"
            );
            continue;
        }

        let committed = notification.committed();
        let bundle = &committed.execution_outcome().bundle;
        let diffs = AccountStateDiff::collect(bundle);
        if !diffs.is_empty() {
            let removed = pool.invalidate_from_state_diff(&diffs);
            if removed > 0 {
                debug!(
                    removed = removed,
                    accounts = diffs.len(),
                    "invalidated EIP-8130 transactions from canonical state diff"
                );
            }
        }
        if pool.tracks_dormancy() {
            let canonical =
                committed.blocks_iter().map(|block| block.num_hash()).collect::<Vec<_>>();
            run_blocking(&pool, move |pool| pool.advance_dormancy(&canonical)).await;
        }
    }
}

/// Runs dormancy work that reads state or walks every dormant transaction on a blocking thread,
/// awaiting it so notifications are still processed in order.
async fn run_blocking<P: StateDiffInvalidation>(pool: &P, work: impl FnOnce(&P) + Send + 'static) {
    let pool = pool.clone();
    if let Err(error) = tokio::task::spawn_blocking(move || work(&pool)).await {
        warn!(error = %error, "dormancy maintenance task failed");
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

    /// Collects watched state changes between flashblocks.
    ///
    /// Protocol and channel nonce advances are omitted because the builder has
    /// already pruned each included transaction, promoting the valid successor
    /// in that pool lane. Treating an included transaction's nonce advance as
    /// external invalidation would immediately evict that successor. This is a
    /// deliberately coarse intra-block filter: canonical invalidation treats
    /// protocol-nonce and nonce-manager writes as nonce advances and drops the
    /// transactions they leave behind.
    #[must_use]
    pub fn collect_for_intra_block(bundle: &BundleState) -> Vec<Self> {
        Self::collect(bundle)
            .into_iter()
            .filter_map(|mut diff| {
                diff.nonce = None;
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
    use std::sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    };

    use alloy_primitives::Address;
    use base_common_consensus::BasePrimitives;
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

    /// Counts the dormancy calls the maintenance task makes. Hand-rolled because the trait
    /// requires `Clone`, and clones must share one count, which a mockall mock cannot express.
    #[derive(Debug, Clone)]
    struct DormancyCallCounter {
        tracks_dormancy: bool,
        calls: Arc<AtomicUsize>,
    }

    impl StateDiffInvalidation for DormancyCallCounter {
        fn invalidate_from_state_diff(&self, _: &[AccountStateDiff]) -> usize {
            0
        }

        fn invalidate_all_tracked(&self, _: InvalidationCause) -> usize {
            0
        }

        fn tracks_dormancy(&self) -> bool {
            self.tracks_dormancy
        }

        fn advance_dormancy(&self, _: &[BlockNumHash]) {
            self.calls.fetch_add(1, Ordering::Relaxed);
        }

        fn discard_dormancy(&self, _: DormancyDiscardReason) {
            self.calls.fetch_add(1, Ordering::Relaxed);
        }
    }

    #[rstest::rstest]
    #[case::off(false, 0)]
    #[case::enforce(true, 2)]
    #[tokio::test]
    async fn dormancy_work_runs_only_when_the_pool_tracks_dormancy(
        #[case] tracks_dormancy: bool,
        #[case] calls: usize,
    ) {
        let pool = DormancyCallCounter { tracks_dormancy, calls: Arc::default() };
        let (sender, receiver) = tokio::sync::broadcast::channel(2);
        sender
            .send(CanonStateNotification::<BasePrimitives>::Commit { new: Arc::default() })
            .unwrap();
        sender
            .send(CanonStateNotification::Reorg { old: Arc::default(), new: Arc::default() })
            .unwrap();
        drop(sender);

        maintain_state_diff_invalidation(pool.clone(), BroadcastStream::new(receiver)).await;

        assert_eq!(pool.calls.load(Ordering::Relaxed), calls);
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
        assert_eq!(account_diff.nonce, None);

        let nonce_manager_diff = diffs
            .iter()
            .find(|diff| diff.address == NonceManagerStorage::ADDRESS)
            .expect("nonce manager balance diff retained");
        assert_eq!(nonce_manager_diff.balance, Some(U256::from(50)));
        assert!(nonce_manager_diff.changed_slots.is_empty());

        let contract_diff =
            diffs.iter().find(|diff| diff.address == contract).expect("contract slot retained");
        assert_eq!(contract_diff.changed_slots, vec![(B256::from(contract_slot), U256::from(1))]);
    }
}
