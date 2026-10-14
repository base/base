//! Synchronous driver that builds one block of flashblocks through the production build loop.
//!
//! The instruction-count benchmark (`benches/flashblock_build_iai.rs`) and the event-volume
//! test (`tests/flashblock_build_events.rs`) both run blocks through this driver so they measure the
//! same code the payload builder runs on the builder thread: the fallback block at flashblock
//! index 0 (pre-execution steps and a `build_block` without pool transactions or a state root),
//! then, as `BasePayloadBuilder::build_next_flashblock` does at indices 1 through N,
//! [`BasePayloadBuilderCtx::execute_best_transactions`] over a [`BestFlashblocksTxs`] that is
//! refreshed from the pool before every flashblock and a `build_block` without a state root,
//! and finally a finalizing `build_block` with the state root plus the final inclusion events.
//!
//! The driver deliberately omits the async payload-job plumbing around that loop: websocket
//! publication, pool maintenance other than pruning (`update_accounts`, invalidation and expiry
//! sweeps), metering-provider bookkeeping, and the flashblock lifecycle events emitted
//! by the payload builder itself. Those paths need a live node and do not scale with the
//! transaction backlog the benchmarks model.

use alloy_primitives::{B256, TxHash};
use base_common_flashblocks::FlashblockId;
use base_execution_txpool::BasePooledTransaction;
use reth_node_api::PayloadBuilderError;
use reth_provider::{
    HashedPostStateProvider, ProviderError, StateRootProvider, StorageRootProvider,
};
use reth_revm::State;

use super::payload::{build_block, emit_final_inclusion_events, execute_pre_steps};
use crate::{
    BasePayloadBuilderCtx, BestFlashblocksTxs, BlockDeferrals, ParkableBestPayloadTransactions,
    RejectionCache, ResourceLimits, RestingPredicateMode,
};

/// Selection totals for one block built by [`FlashblockBlockDriver`].
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct FlashblockBlockOutcome {
    /// Transactions included across every flashblock of the block.
    pub included: u64,
    /// Candidates the build loop considered, summed over every flashblock.
    pub considered: u64,
    /// Candidates parked for a later position or flashblock, summed over every flashblock.
    pub deferred: u64,
    /// The finalized block's state root.
    pub state_root: B256,
}

/// Builds one block of flashblocks through the production build loop. See the module docs for
/// what is and is not covered.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FlashblockBlockDriver {
    /// Flashblocks built for the block.
    pub flashblocks: u64,
    /// Gas each flashblock adds to the cumulative block gas target.
    pub gas_per_flashblock: u64,
    /// Resting-predicate mode applied to the iterator, as the payload builder applies
    /// `--builder.resting-predicates`.
    pub resting_predicate_mode: RestingPredicateMode,
}

impl FlashblockBlockDriver {
    /// Builds every flashblock of one block and finalizes it with a state root.
    ///
    /// `next_iterator` is called before each pool flashblock with its zero-based position (the
    /// flashblock index minus one) and returns a fresh
    /// best-transactions iterator over the pool, exactly as the payload builder refreshes
    /// [`BestFlashblocksTxs`] from the pool before every flashblock. Use it to add arrivals to the
    /// pool between flashblocks. After each pool flashblock, `on_flashblock` receives the hashes it
    /// included and the hashes it rejected permanently, which the payload builder prunes from and
    /// removes from the pool.
    pub fn run_block<DB, P>(
        &self,
        ctx: &mut BasePayloadBuilderCtx,
        state: &mut State<DB>,
        rejection_cache: RejectionCache,
        mut next_iterator: impl FnMut(u64) -> ParkableBestPayloadTransactions<BasePooledTransaction>,
        mut on_flashblock: impl FnMut(&[TxHash], &[TxHash]),
    ) -> Result<FlashblockBlockOutcome, PayloadBuilderError>
    where
        DB: alloy_evm::Database<Error = ProviderError>
            + std::fmt::Debug
            + AsRef<P>
            + revm::Database,
        P: StateRootProvider + HashedPostStateProvider + StorageRootProvider,
    {
        ctx.extra.target_flashblock_count = self.flashblocks;
        ctx.extra.gas_per_batch = self.gas_per_flashblock;

        // The fallback block: production publishes it at index 0 before any pool transactions,
        // and computes its state root only when it skips flashblock building.
        ctx.extra.flashblock_index = 0;
        let mut info = execute_pre_steps(state, ctx)?;
        build_block(state, ctx, &mut info, FlashblockId::default(), false)?;

        let mut deferrals = BlockDeferrals::default();
        let mut outcome = FlashblockBlockOutcome::default();
        let mut best = BestFlashblocksTxs::new(next_iterator(0), rejection_cache)
            .with_resting_predicate_mode(self.resting_predicate_mode);

        for flashblock_index in 1..=self.flashblocks {
            let target_gas = flashblock_index * self.gas_per_flashblock;
            ctx.extra.flashblock_index = flashblock_index;
            ctx.extra.target_gas_for_batch = target_gas;

            if flashblock_index > 1 {
                best.refresh_iterator(next_iterator(flashblock_index - 1));
            }

            // The same limits `build_next_flashblock` sets, with the DA targets split evenly across
            // flashblocks. The per-transaction execution-time limit is wall-clock based, so it stays
            // off to keep instruction counts deterministic.
            let limits = ResourceLimits {
                block_gas_limit: target_gas.min(ctx.block_gas_limit()),
                tx_data_limit: ctx.builder_config.da_config.max_da_tx_size(),
                block_data_limit: ctx
                    .builder_config
                    .da_config
                    .max_da_block_size()
                    .map(|da_limit| da_limit / self.flashblocks * flashblock_index),
                da_footprint_gas_scalar: info.da_footprint_scalar,
                block_da_footprint_limit: info
                    .da_footprint_scalar
                    .map(|_| ctx.block_gas_limit() / self.flashblocks * flashblock_index),
                tx_execution_time_limit_us: None,
                block_uncompressed_size_limit: ctx.builder_config.max_uncompressed_block_size,
            };
            let diag = ctx.execute_best_transactions(
                &mut info,
                &mut deferrals,
                state,
                &mut best,
                &limits,
            )?;

            let committed = info.executed_transactions[info.extra.last_flashblock_index..]
                .iter()
                .map(|tx| tx.tx_hash())
                .collect::<Vec<_>>();
            best.mark_committed(&committed);
            if !diag.permanently_rejected_txs.is_empty() {
                best.mark_rejected(&diag.permanently_rejected_txs);
            }
            on_flashblock(&committed, &diag.permanently_rejected_txs);

            outcome.considered += diag.txs_considered;
            outcome.deferred += diag.txs_deferred;
            build_block(state, ctx, &mut info, FlashblockId::default(), false)?;
        }

        let (payload, _, _) = build_block(state, ctx, &mut info, FlashblockId::default(), true)?;
        emit_final_inclusion_events(ctx, &payload);
        outcome.included = info.executed_transactions.len() as u64;
        outcome.state_root = payload.block().state_root;
        Ok(outcome)
    }
}
