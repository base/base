//! Pruner metrics.

use crate::PrunerOutput;

base_metrics::define_metrics! {
    optimism_trie.pruner
    #[describe("Pruning duration")]
    total_duration_seconds: histogram,
    #[describe("Number of pruned blocks")]
    pruned_blocks: gauge,
    #[describe("Number of account trie updates written in the prune run")]
    account_trie_updates_written: gauge,
    #[describe("Number of storage trie updates written in the prune run")]
    storage_trie_updates_written: gauge,
    #[describe("Number of hashed accounts written in the prune run")]
    hashed_accounts_written: gauge,
    #[describe("Number of hashed storages written in the prune run")]
    hashed_storages_written: gauge,
}

impl Metrics {
    /// Records the result of a prune operation.
    pub(super) fn record_prune_result(result: PrunerOutput) {
        let blocks_pruned = result.end_block - result.start_block;
        if blocks_pruned > 0 {
            Self::total_duration_seconds().record(result.duration.as_secs_f64());
            Self::pruned_blocks().set(blocks_pruned as f64);

            let wc = &result.write_counts;
            Self::account_trie_updates_written().set(wc.account_trie_updates_written_total as f64);
            Self::storage_trie_updates_written().set(wc.storage_trie_updates_written_total as f64);
            Self::hashed_accounts_written().set(wc.hashed_accounts_written_total as f64);
            Self::hashed_storages_written().set(wc.hashed_storages_written_total as f64);
        }
    }
}
