//! Lookahead payload-transaction adapter that drives bounded prewarm scheduling.

use std::{collections::VecDeque, sync::Arc};

use alloy_primitives::{Address, TxHash};
use base_execution_txpool::BasePooledTx;
use reth_payload_util::PayloadTransactions;
use reth_transaction_pool::{
    BestTransactions, BestTransactionsAttributes, PoolTransaction, TransactionPool,
    ValidPoolTransaction,
};

use super::{PrewarmScheduler, SimSetup};
use crate::{ParkablePayloadTransactions, metrics::PrewarmMetrics};

/// Payload-transaction adapter that schedules bounded lookahead predicate-state warming
/// while the build loop consumes candidates.
///
/// The main transaction iterator is owned and delegated to untouched, so the build's
/// parking lifecycle and dynamic-inclusion semantics are exactly those of the inner
/// adapter. The independent lookahead cursor is opened with the same attributes as the
/// main iterator and is only ever advanced (read-only).
pub struct PrewarmingBestTransactions<I, T>
where
    I: ParkablePayloadTransactions<Transaction = T>,
    T: PoolTransaction + BasePooledTx,
{
    /// The build's main transaction iterator, delegated to unchanged.
    pub inner: I,
    /// Independent read-only lookahead cursor over the same pool attributes. `None` when
    /// prewarming is inactive or the cursor is exhausted/saturated.
    pub cursor: Option<Box<dyn BestTransactions<Item = Arc<ValidPoolTransaction<T>>>>>,
    /// The build's prewarm scheduler. `None` when prewarming is inactive.
    pub prewarm: Option<Arc<PrewarmScheduler>>,
    /// Transaction-simulation warming setup. `None` when simulation warming is off.
    pub sim: Option<SimSetup<T>>,
    /// Simulation-eligible transactions waiting for a slot in the bounded simulation
    /// window, nearest the build loop first.
    ///
    /// The lookahead cursor is shared with predicate-key warming and runs far ahead of the
    /// simulation window, so transactions that find the window full are held here instead
    /// of being lost: the cursor cannot be rewound.
    pub sim_deferred: VecDeque<Arc<ValidPoolTransaction<T>>>,
}

impl<I, T> std::fmt::Debug for PrewarmingBestTransactions<I, T>
where
    I: ParkablePayloadTransactions<Transaction = T>,
    T: PoolTransaction + BasePooledTx,
{
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PrewarmingBestTransactions")
            .field("has_cursor", &self.cursor.is_some())
            .field("has_scheduler", &self.prewarm.is_some())
            .field("has_simulation", &self.sim.is_some())
            .field("sim_deferred", &self.sim_deferred.len())
            .finish_non_exhaustive()
    }
}

impl<I, T> PrewarmingBestTransactions<I, T>
where
    I: ParkablePayloadTransactions<Transaction = T>,
    T: PoolTransaction + BasePooledTx,
{
    /// Wraps the build's transaction iterator with prewarm lookahead scheduling.
    ///
    /// When `prewarm` is active, opens an independent read-only lookahead cursor from
    /// `pool` with the same `attributes` as the main iterator and schedules the initial
    /// bounded burst. When inactive, the adapter is a pure pass-through.
    pub fn new<P>(
        inner: I,
        pool: P,
        attributes: BestTransactionsAttributes,
        prewarm: Option<Arc<PrewarmScheduler>>,
        sim: Option<SimSetup<T>>,
    ) -> Self
    where
        P: TransactionPool<Transaction = T>,
    {
        let cursor = prewarm
            .as_ref()
            .filter(|scheduler| scheduler.should_advance_cursor())
            .map(|_| pool.best_transactions_with_attributes(attributes));
        Self::with_cursor(inner, cursor, prewarm, sim)
    }

    /// Wraps an existing lookahead cursor. Schedules the initial bounded burst.
    pub fn with_cursor(
        inner: I,
        cursor: Option<Box<dyn BestTransactions<Item = Arc<ValidPoolTransaction<T>>>>>,
        prewarm: Option<Arc<PrewarmScheduler>>,
        sim: Option<SimSetup<T>>,
    ) -> Self {
        let mut adapter = Self { inner, cursor, prewarm, sim, sim_deferred: VecDeque::new() };
        let initial = adapter.prewarm.as_ref().map_or(0, |scheduler| scheduler.lookahead());
        adapter.advance_lookahead(initial);
        adapter
    }

    /// Advances the lookahead cursor by at most `budget` transactions, scheduling each
    /// transaction's declared predicate state and, when simulation warming is on, its
    /// simulation. Stops when scheduling saturates or the cursor is exhausted. Never
    /// blocks.
    pub fn advance_lookahead(&mut self, budget: usize) {
        let Some(scheduler) = self.prewarm.clone() else { return };
        self.advance_cursor(&scheduler, budget);
        // Always drain: the simulation window reopens as workers finish, independently of
        // whether the shared cursor still has transactions left to scan.
        self.drain_simulations(&scheduler);
    }

    /// Advances the shared lookahead cursor by at most `budget` transactions, warming each
    /// transaction's declared predicate keys and queueing it for simulation if eligible.
    fn advance_cursor(&mut self, scheduler: &PrewarmScheduler, budget: usize) {
        for _ in 0..budget {
            if !scheduler.should_advance_cursor() {
                self.cursor = None;
                return;
            }
            let Some(transaction) = self.cursor.as_mut().and_then(Iterator::next) else {
                self.cursor = None;
                return;
            };
            scheduler.schedule_transaction(&transaction.transaction);
            self.defer_simulation(scheduler, transaction);
        }
    }

    /// Queues one lookahead transaction for simulation, if it is eligible.
    ///
    /// Phase 1 policy: only transactions that declare no validity predicates are
    /// simulated. Predicated transactions already have their declared state warmed cheaply
    /// by [`PrewarmScheduler::schedule_transaction`] and are the ones the build loop is
    /// most likely to skip, so worker time goes to transactions that are certain to
    /// execute against snapshot state.
    fn defer_simulation(
        &mut self,
        scheduler: &PrewarmScheduler,
        transaction: Arc<ValidPoolTransaction<T>>,
    ) {
        let Some(sim) = self.sim.as_ref() else { return };
        if !transaction.transaction.validity_predicates().is_empty() || scheduler.is_sim_saturated()
        {
            return;
        }
        // Bounded by the lookahead cursor burst: one cursor advance defers at most
        // `scheduler.lookahead()` transactions before the next drain, and the window
        // itself holds `sim.lookahead`. Beyond that, holding more only grows memory, so a
        // full buffer drops the farthest transactions (those held are nearest the build
        // loop).
        if self.sim_deferred.len() < scheduler.lookahead().max(sim.lookahead) {
            self.sim_deferred.push_back(transaction);
        }
    }

    /// Schedules deferred simulations while the bounded simulation window has room.
    ///
    /// The window counts simulations that are queued or in flight, so it drains as workers
    /// finish: warming self-throttles to worker throughput instead of committing the whole
    /// cursor, and always picks the transactions nearest the build loop first.
    fn drain_simulations(&mut self, scheduler: &PrewarmScheduler) {
        let Some(lookahead) = self.sim.as_ref().map(|sim| sim.lookahead) else { return };
        while scheduler.pending_simulations() < lookahead {
            if scheduler.is_sim_saturated() {
                self.sim_deferred.clear();
                return;
            }
            let Some(transaction) = self.sim_deferred.pop_front() else { return };
            let Some(sim) = self.sim.as_ref() else { return };
            let Some(job) = (sim.factory)(&transaction.transaction) else { continue };
            if !scheduler.schedule_simulation(job) {
                self.sim_deferred.clear();
                return;
            }
        }
    }
}

impl<I, T> PayloadTransactions for PrewarmingBestTransactions<I, T>
where
    I: ParkablePayloadTransactions<Transaction = T>,
    T: PoolTransaction + BasePooledTx,
{
    type Transaction = T;

    fn next(&mut self, ctx: ()) -> Option<Self::Transaction> {
        let transaction = self.inner.next(ctx)?;
        // The build loop reaching a transaction whose simulation is still queued or in
        // flight means warming did not stay ahead: the read set it would have warmed is
        // not there yet. Counted to measure whether the simulation window and worker
        // budget keep up with the build loop.
        if self.sim.is_some()
            && let Some(scheduler) = self.prewarm.as_ref()
            && scheduler.simulation_pending(transaction.hash())
        {
            PrewarmMetrics::canonical_overtook_sim_total().increment(1);
        }
        // One lookahead advance per consumed candidate keeps the schedule bounded.
        self.advance_lookahead(1);
        Some(transaction)
    }

    fn mark_invalid(&mut self, sender: Address, nonce: u64) {
        self.inner.mark_invalid(sender, nonce);
    }
}

impl<I, T> ParkablePayloadTransactions for PrewarmingBestTransactions<I, T>
where
    I: ParkablePayloadTransactions<Transaction = T>,
    T: PoolTransaction + BasePooledTx,
{
    fn park_current(&mut self) -> bool {
        self.inner.park_current()
    }

    fn mark_current_committed(&mut self) {
        self.inner.mark_current_committed();
    }

    fn promote(&mut self, transaction_hash: TxHash) -> bool {
        self.inner.promote(transaction_hash)
    }

    fn discard_parked(&mut self, transaction_hash: TxHash) -> bool {
        self.inner.discard_parked(transaction_hash)
    }
}
