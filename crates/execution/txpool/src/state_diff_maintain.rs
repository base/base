//! Canonical-state-driven exact-match mempool invalidation feeder.
//!
//! Reth's stock pool maintenance forwards only account nonce/balance to the pool
//! (via `CanonicalStateUpdate`) and drops the block's storage diff. This task
//! subscribes to the canonical-state broadcast in parallel, reads each committed
//! block's full `BundleState`, and feeds the per-account balance/nonce/storage
//! deltas into [`BaseTransactionPool::apply_state_diff`](crate::BaseTransactionPool::apply_state_diff)
//! so channelized EIP-8130 transactions whose watched nonce, actor-config, or
//! channel-nonce slot changed (or whose payer balance dropped) are invalidated
//! ahead of the builder.

use alloy_primitives::{B256, U256};
use base_common_precompiles::NonceManagerStorage;
use futures::StreamExt;
use reth_provider::CanonStateNotification;
use revm::database::BundleState;
use tokio_stream::wrappers::{BroadcastStream, errors::BroadcastStreamRecvError};
use tracing::{debug, warn};

use crate::AccountStateDiff;

/// Applies executed state diffs to a pool's EIP-8130 invalidation guard.
///
/// Implemented by [`crate::BaseTransactionPool`]; abstracted as a trait so the
/// maintenance loop does not need the pool's full generic signature.
pub trait StateDiffInvalidation: Clone + Send + Sync + 'static {
    /// Invalidates guarded transactions affected by an executed per-account
    /// state diff. Returns the number of transactions removed.
    fn invalidate_from_state_diff(&self, diffs: &[AccountStateDiff]) -> usize;

    /// Invalidates every transaction tracked by the EIP-8130 guard.
    ///
    /// Used when an upstream state-diff source reports a gap: retaining any
    /// transaction validated before the missing state changes would be unsafe.
    fn invalidate_all_tracked(&self) -> usize;
}

/// Drains the canonical-state notification stream, feeding each committed block's
/// account/storage diff into the pool's exact-match invalidation index.
///
/// Intended to be spawned as a critical task alongside (not in place of) reth's
/// stock pool maintenance, on both client and builder nodes.
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
                    missed,
                    removed, "canon state stream lagged; invalidated all guarded transactions"
                );
                continue;
            }
            None => break,
        };

        // `committed()` yields the newly-canonical chain for both commits and
        // reorgs, so the diff always reflects the state the next block builds on.
        let committed = notification.committed();
        let diffs = AccountStateDiff::collect(&committed.execution_outcome().bundle);
        if diffs.is_empty() {
            continue;
        }

        let removed = pool.invalidate_from_state_diff(&diffs);
        if removed > 0 {
            debug!(
                removed,
                accounts = diffs.len(),
                "invalidated sidecar transactions from committed state diff"
            );
        }
    }
}

impl AccountStateDiff {
    /// Builds changed-account diffs from an executed [`BundleState`], keeping
    /// only accounts that changed a watched surface (balance, protocol nonce,
    /// bytecode hash, or a storage slot).
    ///
    /// This is shared by the committed-state maintenance task and the
    /// flashblock builder so invalidation follows whichever state advances
    /// first.
    #[must_use]
    pub fn collect(bundle: &BundleState) -> Vec<Self> {
        let mut diffs = Vec::new();
        for (address, account) in &bundle.state {
            let new_balance = account.info.as_ref().map(|info| info.balance);
            let old_balance = account.original_info.as_ref().map(|info| info.balance);
            // A self-destructed account (no current info) effectively drops to a
            // zero balance, which correctly invalidates anything it sponsored.
            let balance = (new_balance != old_balance).then(|| new_balance.unwrap_or(U256::ZERO));

            let new_nonce = account.info.as_ref().map(|info| info.nonce);
            let old_nonce = account.original_info.as_ref().map(|info| info.nonce);
            let nonce_changed = new_nonce != old_nonce;

            let new_code_hash = account.info.as_ref().map(|info| info.code_hash);
            let old_code_hash = account.original_info.as_ref().map(|info| info.code_hash);
            let code_changed = new_code_hash != old_code_hash;

            let changed_slots: Vec<B256> = account
                .storage
                .iter()
                .filter(|(_, slot)| slot.is_changed())
                .map(|(key, _)| B256::from(*key))
                .collect();

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

    /// Builds diffs for intra-block invalidation between flashblocks.
    ///
    /// Protocol and channel nonce advances are intentionally omitted here. The
    /// builder has already pruned each included transaction, which advances the
    /// corresponding pool lane and promotes a valid successor. Treating the
    /// shared nonce key as exact-match state would immediately evict that
    /// successor before the next flashblock. Canonical-state invalidation keeps
    /// the conservative nonce-key handling for competing blocks and reorgs.
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
        let mut acc = BundleAccount {
            info: present,
            original_info: original,
            storage: Default::default(),
            status: revm::database::AccountStatus::Changed,
        };
        for (key, slot) in slots {
            acc.storage.insert(key, slot);
        }
        acc
    }

    fn bundle(entries: Vec<(Address, BundleAccount)>) -> BundleState {
        let mut state = HashMap::default();
        for (addr, acc) in entries {
            state.insert(addr, acc);
        }
        BundleState { state, ..Default::default() }
    }

    fn find(diffs: &[AccountStateDiff], addr: Address) -> Option<&AccountStateDiff> {
        diffs.iter().find(|d| d.address == addr)
    }

    #[test]
    fn balance_change_is_captured_and_unchanged_is_skipped() {
        let changed = Address::repeat_byte(0x01);
        let same = Address::repeat_byte(0x02);
        let bundle = bundle(vec![
            (changed, account(Some(info(100, 0)), Some(info(50, 0)), vec![])),
            (same, account(Some(info(100, 5)), Some(info(100, 5)), vec![])),
        ]);

        let diffs = AccountStateDiff::collect(&bundle);

        let changed_diff = find(&diffs, changed).expect("balance change must be captured");
        assert_eq!(changed_diff.balance, Some(U256::from(50u64)));
        assert!(!changed_diff.nonce_changed);
        assert!(find(&diffs, same).is_none(), "unchanged account must be skipped");
    }

    #[test]
    fn nonce_change_is_captured() {
        let addr = Address::repeat_byte(0x03);
        let bundle = bundle(vec![(addr, account(Some(info(100, 1)), Some(info(100, 2)), vec![]))]);

        let diffs = AccountStateDiff::collect(&bundle);

        let diff = find(&diffs, addr).expect("nonce change must be captured");
        assert!(diff.nonce_changed);
        assert_eq!(diff.balance, None, "balance unchanged so must not be set");
    }

    #[test]
    fn code_hash_change_is_captured() {
        let addr = Address::repeat_byte(0x06);
        let mut original = info(100, 1);
        original.code_hash = B256::repeat_byte(0x11);
        let mut present = original.clone();
        present.code_hash = B256::repeat_byte(0x22);
        let bundle = bundle(vec![(addr, account(Some(original), Some(present), vec![]))]);

        let diffs = AccountStateDiff::collect(&bundle);

        let diff = find(&diffs, addr).expect("code hash change must be captured");
        assert!(diff.code_changed);
    }

    #[test]
    fn intra_block_diff_omits_nonce_advances() {
        let sender = Address::repeat_byte(0x07);
        let nonce_slot = U256::from(9u64);
        let bundle = bundle(vec![
            (sender, account(Some(info(100, 1)), Some(info(100, 2)), vec![])),
            (
                NonceManagerStorage::ADDRESS,
                account(
                    Some(info(0, 0)),
                    Some(info(0, 0)),
                    vec![(
                        nonce_slot,
                        StorageSlot {
                            previous_or_original_value: U256::ZERO,
                            present_value: U256::from(1u64),
                        },
                    )],
                ),
            ),
        ]);

        assert!(
            AccountStateDiff::collect_for_intra_block(&bundle).is_empty(),
            "included nonce advances must not evict newly promoted successors"
        );
    }

    #[test]
    fn only_changed_slots_are_collected() {
        let addr = Address::repeat_byte(0x04);
        let slots = vec![
            // Changed slot.
            (
                U256::from(7u64),
                StorageSlot {
                    previous_or_original_value: U256::ZERO,
                    present_value: U256::from(9u64),
                },
            ),
            // Touched-but-unchanged slot.
            (
                U256::from(8u64),
                StorageSlot {
                    previous_or_original_value: U256::from(3u64),
                    present_value: U256::from(3u64),
                },
            ),
        ];
        let bundle = bundle(vec![(addr, account(Some(info(100, 0)), Some(info(100, 0)), slots))]);

        let diffs = AccountStateDiff::collect(&bundle);

        let diff = find(&diffs, addr).expect("storage change must be captured");
        assert_eq!(diff.changed_slots, vec![B256::from(U256::from(7u64))]);
    }

    #[test]
    fn self_destruct_drops_balance_to_zero() {
        let addr = Address::repeat_byte(0x05);
        let bundle = bundle(vec![(addr, account(Some(info(100, 0)), None, vec![]))]);

        let diffs = AccountStateDiff::collect(&bundle);

        let diff = find(&diffs, addr).expect("destroyed account must be captured");
        assert_eq!(diff.balance, Some(U256::ZERO));
    }
}
