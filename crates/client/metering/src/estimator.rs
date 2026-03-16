//! Priority fee estimation based on resource consumption in flashblocks.
//!
//! This module provides the core algorithm for estimating the priority fee needed
//! to achieve inclusion in a block, based on historical transaction data.

use std::{collections::BTreeMap, num::NonZeroUsize, sync::Arc};

use alloy_primitives::U256;
use parking_lot::RwLock;

use crate::cache::{BlockMetrics, MeteredTransaction, MeteringCache};

/// Errors that can occur during priority fee estimation.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum EstimateError {
    /// The bundle's resource demand exceeds the configured capacity limit.
    #[error(
        "bundle {} demand ({demand}) exceeds capacity limit ({limit})",
        resource.as_name()
    )]
    DemandExceedsCapacity {
        /// The resource that exceeded capacity.
        resource: ResourceKind,
        /// The requested demand.
        demand: u128,
        /// The configured limit.
        limit: u128,
    },
}

/// Configured capacity limits for each resource type.
///
/// These limits mirror the builder's budget semantics.
///
/// Execution time resets for each flashblock, matching the builder's
/// `flashblock_execution_time_limit_us` and `reset_flashblock_execution_time()`.
/// Gas, DA bytes, and state root time accumulate across the block against
/// cumulative per-flashblock targets derived from the whole-block budget.
#[derive(Debug, Clone, Copy, Default)]
pub struct ResourceLimits {
    /// Gas budget for the whole block.
    pub gas_used: Option<u64>,
    /// Execution time budget per flashblock in microseconds.
    pub execution_time_us: Option<u128>,
    /// State root computation budget for the whole block in microseconds.
    pub state_root_time_us: Option<u128>,
    /// Data availability byte budget for the whole block.
    pub data_availability_bytes: Option<u64>,
}

impl ResourceLimits {
    /// Returns the limit for the given resource kind.
    pub fn limit_for(&self, resource: ResourceKind) -> Option<u128> {
        match resource {
            ResourceKind::GasUsed => self.gas_used.map(|v| v as u128),
            ResourceKind::ExecutionTime => self.execution_time_us,
            ResourceKind::StateRootTime => self.state_root_time_us,
            ResourceKind::DataAvailability => self.data_availability_bytes.map(|v| v as u128),
        }
    }
}

/// Resources that influence flashblock inclusion ordering.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum ResourceKind {
    /// Gas consumption.
    GasUsed,
    /// Execution time.
    ExecutionTime,
    /// State root computation time.
    StateRootTime,
    /// Data availability bytes.
    DataAvailability,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ResourceBudgetBehavior {
    ResetsEachFlashblock,
    AccumulatesUntilBlockEnd,
}

impl ResourceKind {
    /// Returns all resource kinds in a fixed order.
    pub const fn all() -> [Self; 4] {
        [Self::GasUsed, Self::ExecutionTime, Self::StateRootTime, Self::DataAvailability]
    }

    /// Returns how this resource budget behaves in the builder.
    ///
    /// Execution time resets each flashblock, while gas, DA bytes, and state root time
    /// accumulate across the block against growing cumulative targets in the tx-pool
    /// flashblock loop.
    const fn budget_behavior(self) -> ResourceBudgetBehavior {
        match self {
            Self::ExecutionTime => ResourceBudgetBehavior::ResetsEachFlashblock,
            Self::GasUsed | Self::StateRootTime | Self::DataAvailability => {
                ResourceBudgetBehavior::AccumulatesUntilBlockEnd
            }
        }
    }

    /// Returns a human-readable name for the resource kind.
    pub const fn as_name(&self) -> &'static str {
        match self {
            Self::GasUsed => "gas",
            Self::ExecutionTime => "execution time",
            Self::StateRootTime => "state root time",
            Self::DataAvailability => "data availability",
        }
    }

    /// Returns a camelCase name for JSON serialization.
    pub const fn as_camel_case(&self) -> &'static str {
        match self {
            Self::GasUsed => "gasUsed",
            Self::ExecutionTime => "executionTime",
            Self::StateRootTime => "stateRootTime",
            Self::DataAvailability => "dataAvailability",
        }
    }
}

/// Amount of resources required by the bundle being priced.
#[derive(Debug, Clone, Copy, Default)]
pub struct ResourceDemand {
    /// Gas demand.
    pub gas_used: Option<u64>,
    /// Execution time demand in microseconds.
    pub execution_time_us: Option<u128>,
    /// State root time demand in microseconds.
    pub state_root_time_us: Option<u128>,
    /// Data availability bytes demand.
    pub data_availability_bytes: Option<u64>,
}

impl ResourceDemand {
    /// Returns the demand for the given resource kind.
    pub fn demand_for(&self, resource: ResourceKind) -> Option<u128> {
        match resource {
            ResourceKind::GasUsed => self.gas_used.map(|v| v as u128),
            ResourceKind::ExecutionTime => self.execution_time_us,
            ResourceKind::StateRootTime => self.state_root_time_us,
            ResourceKind::DataAvailability => self.data_availability_bytes.map(|v| v as u128),
        }
    }
}

/// Fee estimate for a single resource type.
///
/// The estimation algorithm answers: "What priority fee would my bundle need to pay
/// to displace enough lower-paying transactions to free up the resources I need?"
#[derive(Debug, Clone)]
pub struct ResourceEstimate {
    /// Minimum fee to displace enough capacity for the bundle's resource demand.
    pub threshold_priority_fee: U256,
    /// Recommended fee based on a percentile of transactions above the threshold.
    /// Provides a safety margin over the bare minimum.
    pub recommended_priority_fee: U256,
    /// Total resource usage of transactions at or above the threshold fee.
    pub cumulative_usage: u128,
    /// Number of transactions at or above `threshold_priority_fee`. These higher-paying
    /// transactions remain included alongside the bundle; lower-paying ones are displaced.
    pub threshold_tx_count: usize,
    /// Total transactions considered in the estimate.
    pub total_transactions: usize,
}

/// Per-resource fee estimates.
///
/// Each field corresponds to a resource type. `None` indicates the resource
/// was not requested or could not be estimated.
#[derive(Debug, Clone, Default)]
pub struct ResourceEstimates {
    /// Gas usage estimate.
    pub gas_used: Option<ResourceEstimate>,
    /// Execution time estimate.
    pub execution_time: Option<ResourceEstimate>,
    /// State root time estimate.
    pub state_root_time: Option<ResourceEstimate>,
    /// Data availability estimate.
    pub data_availability: Option<ResourceEstimate>,
}

impl ResourceEstimates {
    /// Returns the estimate for the given resource kind.
    pub const fn get(&self, kind: ResourceKind) -> Option<&ResourceEstimate> {
        match kind {
            ResourceKind::GasUsed => self.gas_used.as_ref(),
            ResourceKind::ExecutionTime => self.execution_time.as_ref(),
            ResourceKind::StateRootTime => self.state_root_time.as_ref(),
            ResourceKind::DataAvailability => self.data_availability.as_ref(),
        }
    }

    /// Sets the estimate for the given resource kind.
    pub const fn set(&mut self, kind: ResourceKind, estimate: ResourceEstimate) {
        match kind {
            ResourceKind::GasUsed => self.gas_used = Some(estimate),
            ResourceKind::ExecutionTime => self.execution_time = Some(estimate),
            ResourceKind::StateRootTime => self.state_root_time = Some(estimate),
            ResourceKind::DataAvailability => self.data_availability = Some(estimate),
        }
    }

    /// Iterates over all present estimates with their resource kind.
    pub fn iter(&self) -> impl Iterator<Item = (ResourceKind, &ResourceEstimate)> {
        [
            (ResourceKind::GasUsed, &self.gas_used),
            (ResourceKind::ExecutionTime, &self.execution_time),
            (ResourceKind::StateRootTime, &self.state_root_time),
            (ResourceKind::DataAvailability, &self.data_availability),
        ]
        .into_iter()
        .filter_map(|(kind, opt)| opt.as_ref().map(|est| (kind, est)))
    }

    /// Returns true if no estimates are present.
    pub fn is_empty(&self) -> bool {
        self.iter().next().is_none()
    }
}

/// Estimates for a specific flashblock index.
#[derive(Debug, Clone)]
pub struct FlashblockResourceEstimates {
    /// Flashblock index in the pending flashblock stream.
    ///
    /// The base flashblock at index `0` is excluded from estimation because the builder's
    /// metered tx-pool budgeting loop starts at index `1`.
    pub flashblock_index: u64,
    /// Per-resource estimates.
    pub estimates: ResourceEstimates,
}

/// Aggregated estimates for a block.
#[derive(Debug, Clone)]
pub struct BlockPriorityEstimates {
    /// Block number.
    pub block_number: u64,
    /// Per-flashblock estimates for the scheduled tx-pool flashblocks (`1..=target`).
    pub flashblocks: Vec<FlashblockResourceEstimates>,
    /// Minimum recommended fee across all flashblocks (easiest inclusion).
    pub min_across_flashblocks: ResourceEstimates,
    /// Maximum recommended fee across all flashblocks (most competitive).
    pub max_across_flashblocks: ResourceEstimates,
}

/// Priority fee estimate aggregated across multiple recent blocks.
#[derive(Debug, Clone)]
pub struct RollingPriorityEstimate {
    /// Number of blocks that contributed to this estimate.
    pub blocks_sampled: usize,
    /// Per-resource estimates (median across sampled blocks).
    pub estimates: ResourceEstimates,
    /// Recommended priority fee: maximum across all resources.
    pub priority_fee: U256,
}

const TARGET_FLASHBLOCKS_PER_BLOCK_NON_ZERO_MSG: &str =
    "target_flashblocks_per_block must be greater than 0";

pub(crate) const fn assert_valid_percentile(percentile: f64) {
    if !(percentile >= 0.0 && percentile <= 1.0) {
        panic!("percentile must be between 0.0 and 1.0 inclusive");
    }
}

#[derive(Debug)]
struct FlashblockEstimateInput<'a> {
    flashblock_index: u64,
    transactions: Vec<&'a MeteredTransaction>,
}

#[derive(Debug)]
struct EstimatedBlock {
    estimates: BlockPriorityEstimates,
    rolling_summary: ResourceEstimates,
}

#[derive(Debug)]
struct BlockEstimateState<'a> {
    flashblock_inputs: Vec<FlashblockEstimateInput<'a>>,
    prefix_transactions: Vec<Vec<&'a MeteredTransaction>>,
    flashblock_estimates: Vec<FlashblockResourceEstimates>,
    min_across_flashblocks: ResourceEstimates,
    max_across_flashblocks: ResourceEstimates,
}

impl<'a> BlockEstimateState<'a> {
    fn new(block_metrics: &'a BlockMetrics, target_flashblocks_per_block: usize) -> Option<Self> {
        let flashblock_inputs =
            collect_scheduled_flashblock_inputs(block_metrics, target_flashblocks_per_block);
        if flashblock_inputs.iter().all(|flashblock| flashblock.transactions.is_empty()) {
            return None;
        }

        let prefix_transactions = collect_sorted_transaction_prefixes(&flashblock_inputs);
        let flashblock_estimates = flashblock_inputs
            .iter()
            .map(|flashblock| FlashblockResourceEstimates {
                flashblock_index: flashblock.flashblock_index,
                estimates: ResourceEstimates::default(),
            })
            .collect();

        Some(Self {
            flashblock_inputs,
            prefix_transactions,
            flashblock_estimates,
            min_across_flashblocks: ResourceEstimates::default(),
            max_across_flashblocks: ResourceEstimates::default(),
        })
    }

    fn record_estimate(
        &mut self,
        resource: ResourceKind,
        flashblock_position: usize,
        estimate: ResourceEstimate,
    ) {
        update_min_estimate(&mut self.min_across_flashblocks, resource, &estimate);
        update_max_estimate(&mut self.max_across_flashblocks, resource, &estimate);
        self.flashblock_estimates[flashblock_position].estimates.set(resource, estimate);
    }

    fn into_estimated_block(
        self,
        block_number: u64,
        rolling_summary: ResourceEstimates,
    ) -> EstimatedBlock {
        EstimatedBlock {
            estimates: BlockPriorityEstimates {
                block_number,
                flashblocks: self.flashblock_estimates,
                min_across_flashblocks: self.min_across_flashblocks,
                max_across_flashblocks: self.max_across_flashblocks,
            },
            rolling_summary,
        }
    }
}

/// Computes resource fee estimates based on cached flashblock metering data.
#[derive(Debug)]
pub struct PriorityFeeEstimator {
    cache: Arc<RwLock<MeteringCache>>,
    percentile: f64,
    limits: ResourceLimits,
    default_priority_fee: U256,
    target_flashblocks_per_block: usize,
}

impl PriorityFeeEstimator {
    /// Creates a new estimator referencing the shared metering cache.
    ///
    /// # Parameters
    /// - `cache`: Shared cache containing recent flashblock metering data.
    /// - `percentile`: Point in the fee distribution (among transactions above threshold)
    ///   to use for the recommended fee.
    /// - `limits`: Configured resource capacity limits.
    /// - `default_priority_fee`: Fee to return when a resource is not congested.
    /// - `target_flashblocks_per_block`: Number of tx-pool flashblocks the builder budgets
    ///   per block. This excludes the initial base flashblock at index `0`.
    pub const fn new(
        cache: Arc<RwLock<MeteringCache>>,
        percentile: f64,
        limits: ResourceLimits,
        default_priority_fee: U256,
        target_flashblocks_per_block: usize,
    ) -> Self {
        assert_valid_percentile(percentile);
        Self {
            cache,
            percentile,
            limits,
            default_priority_fee,
            target_flashblocks_per_block: NonZeroUsize::new(target_flashblocks_per_block)
                .expect(TARGET_FLASHBLOCKS_PER_BLOCK_NON_ZERO_MSG)
                .get(),
        }
    }

    /// Returns the bundle's demand and configured capacity for a resource,
    /// or `None` if either is unconfigured.
    fn resource_budget(
        &self,
        resource: ResourceKind,
        demand: &ResourceDemand,
    ) -> Option<(u128, u128)> {
        Some((demand.demand_for(resource)?, self.limits.limit_for(resource)?))
    }

    fn estimate_resource(
        &self,
        resource: ResourceKind,
        transactions: &[&MeteredTransaction],
        demand: u128,
        limit: u128,
    ) -> Result<ResourceEstimate, EstimateError> {
        compute_estimate(
            resource,
            transactions,
            demand,
            limit,
            usage_extractor(resource),
            self.percentile,
            self.default_priority_fee,
        )
    }

    const fn ensure_capacity(
        &self,
        resource: ResourceKind,
        demand: u128,
        limit: u128,
    ) -> Result<(), EstimateError> {
        if demand > limit {
            return Err(EstimateError::DemandExceedsCapacity { resource, demand, limit });
        }

        Ok(())
    }

    fn estimate_resetting_resource(
        &self,
        resource: ResourceKind,
        state: &mut BlockEstimateState<'_>,
        demand: u128,
        limit: u128,
    ) -> Result<ResourceEstimate, EstimateError> {
        self.ensure_capacity(resource, demand, limit)?;

        let mut rolling_summary = None;
        for flashblock_position in 0..state.flashblock_inputs.len() {
            let estimate = self.estimate_resource(
                resource,
                &state.flashblock_inputs[flashblock_position].transactions,
                demand,
                limit,
            )?;

            if rolling_summary.as_ref().is_none_or(|current: &ResourceEstimate| {
                is_more_competitive_estimate(&estimate, current)
            }) {
                rolling_summary = Some(estimate.clone());
            }

            state.record_estimate(resource, flashblock_position, estimate);
        }

        Ok(rolling_summary.expect("flashblock_inputs are non-empty"))
    }

    fn estimate_accumulating_resource(
        &self,
        resource: ResourceKind,
        state: &mut BlockEstimateState<'_>,
        demand: u128,
        total_limit: u128,
    ) -> Result<ResourceEstimate, EstimateError> {
        let block_end_limit = limit_at_block_end(total_limit, self.target_flashblocks_per_block);
        self.ensure_capacity(resource, demand, block_end_limit)?;

        for flashblock_position in 0..state.flashblock_inputs.len() {
            let deadline_limit = cumulative_limit_for_flashblock(
                total_limit,
                state.flashblock_inputs[flashblock_position].flashblock_index,
                self.target_flashblocks_per_block,
            );
            if demand > deadline_limit {
                continue;
            }

            let estimate = self.estimate_resource(
                resource,
                state.prefix_transactions[flashblock_position].as_slice(),
                demand,
                deadline_limit,
            )?;
            state.record_estimate(resource, flashblock_position, estimate);
        }

        let block_end_transactions = state
            .prefix_transactions
            .last()
            .map(Vec::as_slice)
            .expect("prefix_transactions are non-empty");
        let block_end_estimate =
            self.estimate_resource(resource, block_end_transactions, demand, block_end_limit)?;

        if state.min_across_flashblocks.get(resource).is_none() {
            update_min_estimate(&mut state.min_across_flashblocks, resource, &block_end_estimate);
            update_max_estimate(&mut state.max_across_flashblocks, resource, &block_end_estimate);
        }

        Ok(block_end_estimate)
    }

    /// Returns fee estimates for the provided block. If `block_number` is `None`
    /// the most recent block in the cache is used.
    ///
    /// Returns `Ok(None)` if the cache is empty, the requested block is not cached,
    /// or no transactions exist in the cached tx-pool flashblocks.
    ///
    /// Returns `Err` if the bundle's demand exceeds any resource's capacity limit.
    pub fn estimate_for_block(
        &self,
        block_number: Option<u64>,
        demand: ResourceDemand,
    ) -> Result<Option<BlockPriorityEstimates>, EstimateError> {
        let cache_guard = self.cache.read();
        let block_metrics = block_number
            .map_or_else(|| cache_guard.blocks_desc().next(), |target| cache_guard.block(target));
        let Some(block_metrics) = block_metrics else {
            return Ok(None);
        };

        self.estimate_block(block_metrics, demand)
            .map(|estimate| estimate.map(|estimated_block| estimated_block.estimates))
    }

    /// Estimates priority fees from a `BlockMetrics` reference without acquiring any lock.
    fn estimate_block(
        &self,
        block_metrics: &BlockMetrics,
        demand: ResourceDemand,
    ) -> Result<Option<EstimatedBlock>, EstimateError> {
        let block_number = block_metrics.block_number;
        let Some(mut state) =
            BlockEstimateState::new(block_metrics, self.target_flashblocks_per_block)
        else {
            return Ok(None);
        };

        let mut rolling_summary = ResourceEstimates::default();
        for resource in ResourceKind::all() {
            let Some((demand_value, limit_value)) = self.resource_budget(resource, &demand) else {
                continue;
            };

            let resource_summary = match resource.budget_behavior() {
                ResourceBudgetBehavior::ResetsEachFlashblock => self.estimate_resetting_resource(
                    resource,
                    &mut state,
                    demand_value,
                    limit_value,
                )?,
                ResourceBudgetBehavior::AccumulatesUntilBlockEnd => self
                    .estimate_accumulating_resource(
                        resource,
                        &mut state,
                        demand_value,
                        limit_value,
                    )?,
            };
            rolling_summary.set(resource, resource_summary);
        }

        Ok(Some(state.into_estimated_block(block_number, rolling_summary)))
    }

    /// Returns rolling fee estimates aggregated across the most recent blocks in the cache.
    ///
    /// For each resource, computes estimates per-block and takes the median recommended fee.
    /// The final `recommended_priority_fee` is the maximum across all resources.
    ///
    /// Returns `Ok(None)` if the cache is empty or no blocks contain transaction data.
    ///
    /// Returns `Err` if the bundle's demand exceeds any resource's capacity limit.
    pub fn estimate_rolling(
        &self,
        demand: ResourceDemand,
    ) -> Result<Option<RollingPriorityEstimate>, EstimateError> {
        let cache_guard = self.cache.read();

        if cache_guard.is_empty() {
            return Ok(None);
        }

        // Collect per-block summaries under a single read lock for a consistent snapshot.
        let mut block_estimates = Vec::new();
        for block_metrics in cache_guard.blocks_desc() {
            if let Some(est) = self.estimate_block(block_metrics, demand)? {
                block_estimates.push(est.rolling_summary);
            }
        }

        if block_estimates.is_empty() {
            return Ok(None);
        }

        // Compute median fee for each resource across blocks.
        let mut estimates = ResourceEstimates::default();
        let mut max_fee = U256::ZERO;

        for resource in ResourceKind::all() {
            let mut resource_estimates: Vec<ResourceEstimate> =
                block_estimates.iter().filter_map(|e| e.get(resource)).cloned().collect();

            if resource_estimates.is_empty() {
                continue;
            }

            resource_estimates.sort_by_key(|estimate| estimate.recommended_priority_fee);
            // Upper median: for even-length lists, picks the higher of the two middle values.
            let median_estimate = resource_estimates[resource_estimates.len() / 2].clone();
            max_fee = max_fee.max(median_estimate.recommended_priority_fee);
            estimates.set(resource, median_estimate);
        }

        if estimates.is_empty() {
            return Ok(None);
        }

        Ok(Some(RollingPriorityEstimate {
            blocks_sampled: block_estimates.len(),
            estimates,
            priority_fee: max_fee,
        }))
    }
}

fn collect_scheduled_flashblock_inputs<'a>(
    block_metrics: &'a BlockMetrics,
    target_flashblocks_per_block: usize,
) -> Vec<FlashblockEstimateInput<'a>> {
    let transactions_by_flashblock: BTreeMap<u64, Vec<&MeteredTransaction>> = block_metrics
        .flashblocks()
        .filter(|flashblock| flashblock.flashblock_index > 0)
        .map(|flashblock| {
            (flashblock.flashblock_index, flashblock.transactions().iter().collect::<Vec<_>>())
        })
        .collect();

    // Late FCUs can cause the builder to emit fewer flashblocks for a given block, but this
    // estimator still mirrors the configured tx-pool flashblock target rather than inferring the
    // reduced count from cached data. Any scheduled flashblocks that were never emitted are
    // therefore modeled as empty inputs.
    (1..=target_flashblocks_per_block as u64)
        .map(|flashblock_index| FlashblockEstimateInput {
            flashblock_index,
            transactions: transactions_by_flashblock
                .get(&flashblock_index)
                .cloned()
                .unwrap_or_default(),
        })
        .collect()
}

fn collect_sorted_transaction_prefixes<'a>(
    flashblocks: &[FlashblockEstimateInput<'a>],
) -> Vec<Vec<&'a MeteredTransaction>> {
    let total_tx_count = flashblocks.iter().map(|flashblock| flashblock.transactions.len()).sum();
    let mut prefixes = Vec::with_capacity(flashblocks.len());
    let mut running_transactions = Vec::with_capacity(total_tx_count);

    for flashblock in flashblocks {
        running_transactions =
            merge_transactions_desc(running_transactions, flashblock.transactions.as_slice());
        prefixes.push(running_transactions.clone());
    }

    prefixes
}

fn merge_transactions_desc<'a>(
    existing: Vec<&'a MeteredTransaction>,
    new_transactions: &[&'a MeteredTransaction],
) -> Vec<&'a MeteredTransaction> {
    let mut merged = Vec::with_capacity(existing.len() + new_transactions.len());
    let mut existing_iter = existing.into_iter().peekable();
    let mut new_iter = new_transactions.iter().copied().peekable();

    while let (Some(existing_tx), Some(new_tx)) = (existing_iter.peek(), new_iter.peek()) {
        if existing_tx.priority_fee_per_gas >= new_tx.priority_fee_per_gas {
            merged.push(existing_iter.next().expect("peeked existing transaction"));
        } else {
            merged.push(new_iter.next().expect("peeked new transaction"));
        }
    }

    merged.extend(existing_iter);
    merged.extend(new_iter);
    merged
}

/// Mirrors the builder's cumulative target math:
/// `per_batch = total_limit / flashblocks_per_block`, then the tx-pool flashblock deadline
/// is `per_batch * flashblock_index`.
///
/// This intentionally preserves the builder's integer division. If `total_limit` is not evenly
/// divisible by `total_flashblock_count`, the block-end deadline is slightly below `total_limit`.
const fn cumulative_limit_for_flashblock(
    total_limit: u128,
    flashblock_index: u64,
    total_flashblock_count: usize,
) -> u128 {
    if total_flashblock_count == 0 {
        return 0;
    }

    let per_flashblock_limit = total_limit / total_flashblock_count as u128;
    per_flashblock_limit.saturating_mul(flashblock_index as u128)
}

const fn limit_at_block_end(total_limit: u128, total_flashblock_count: usize) -> u128 {
    cumulative_limit_for_flashblock(
        total_limit,
        total_flashblock_count as u64,
        total_flashblock_count,
    )
}

const fn estimate_competitiveness_key(
    estimate: &ResourceEstimate,
) -> (U256, U256, usize, usize, u128) {
    (
        estimate.recommended_priority_fee,
        estimate.threshold_priority_fee,
        estimate.total_transactions,
        estimate.threshold_tx_count,
        estimate.cumulative_usage,
    )
}

fn is_more_competitive_estimate(candidate: &ResourceEstimate, current: &ResourceEstimate) -> bool {
    estimate_competitiveness_key(candidate) > estimate_competitiveness_key(current)
}

fn is_less_competitive_estimate(candidate: &ResourceEstimate, current: &ResourceEstimate) -> bool {
    estimate_competitiveness_key(candidate) < estimate_competitiveness_key(current)
}

fn update_min_estimate(
    estimates: &mut ResourceEstimates,
    resource: ResourceKind,
    candidate: &ResourceEstimate,
) {
    if estimates
        .get(resource)
        .is_none_or(|current| is_less_competitive_estimate(candidate, current))
    {
        estimates.set(resource, candidate.clone());
    }
}

fn update_max_estimate(
    estimates: &mut ResourceEstimates,
    resource: ResourceKind,
    candidate: &ResourceEstimate,
) {
    if estimates
        .get(resource)
        .is_none_or(|current| is_more_competitive_estimate(candidate, current))
    {
        estimates.set(resource, candidate.clone());
    }
}

/// Core estimation algorithm (top-down approach).
///
/// Given a sorted list of transactions and a resource limit, determines the minimum priority
/// fee needed to be included alongside enough high-paying transactions while still
/// leaving room for the bundle's demand.
///
/// # Arguments
///
/// * `transactions` - Must be sorted by priority fee descending (highest first)
/// * `resource` - The resource kind being estimated
/// * `demand` - How much of the resource the bundle needs
/// * `limit` - Maximum capacity for this resource
/// * `usage_fn` - Function to extract resource usage from a transaction
/// * `percentile` - Point in fee distribution for recommended fee (e.g., 0.5 for median)
/// * `default_fee` - Fee to return when resource is uncongested
///
/// # Algorithm
///
/// 1. Walk from highest-paying transactions, subtracting each transaction's usage from
///    remaining capacity.
/// 2. Stop when including another transaction would leave less capacity than the bundle needs.
/// 3. The threshold fee is the fee of the last included transaction (the minimum fee
///    among transactions that would be included alongside the bundle).
/// 4. If we include all transactions and still have capacity >= demand, the resource is
///    not congested, so return the configured default fee.
///
/// Returns `Err` if the bundle's demand exceeds the resource limit.
fn compute_estimate(
    resource: ResourceKind,
    transactions: &[&MeteredTransaction],
    demand: u128,
    limit: u128,
    usage_fn: fn(&MeteredTransaction) -> u128,
    percentile: f64,
    default_fee: U256,
) -> Result<ResourceEstimate, EstimateError> {
    // Bundle demand exceeds the resource limit entirely.
    if demand > limit {
        return Err(EstimateError::DemandExceedsCapacity { resource, demand, limit });
    }

    // No transactions or zero demand means no competition for this resource.
    if transactions.is_empty() || demand == 0 {
        return Ok(ResourceEstimate {
            threshold_priority_fee: default_fee,
            recommended_priority_fee: default_fee,
            cumulative_usage: 0,
            threshold_tx_count: 0,
            total_transactions: 0,
        });
    }

    // Walk from highest-paying transactions, subtracting usage from remaining capacity.
    // Stop when we can no longer fit another transaction while leaving room for demand.
    let mut remaining = limit;
    let mut included_usage = 0u128;
    let mut last_included_idx: Option<usize> = None;

    for (idx, tx) in transactions.iter().enumerate() {
        let usage = usage_fn(tx);

        // Check if we can include this transaction and still have room for the bundle.
        if remaining >= usage && remaining.saturating_sub(usage) >= demand {
            remaining = remaining.saturating_sub(usage);
            included_usage = included_usage.saturating_add(usage);
            last_included_idx = Some(idx);
        } else {
            // Can't include this transaction without crowding out the bundle.
            break;
        }
    }

    // If we included all transactions and still have room, resource is not congested.
    let is_uncongested = last_included_idx == Some(transactions.len() - 1) && remaining >= demand;

    if is_uncongested {
        return Ok(ResourceEstimate {
            threshold_priority_fee: default_fee,
            recommended_priority_fee: default_fee,
            cumulative_usage: included_usage,
            threshold_tx_count: transactions.len(),
            total_transactions: transactions.len(),
        });
    }

    let (supporting_count, threshold_fee, recommended_fee) = last_included_idx.map_or_else(
        || {
            // No transactions fit - even the first transaction would crowd out
            // the bundle. The bundle must beat the highest fee to be included.
            // Report 0 supporting transactions since none were actually included.
            let threshold_fee = transactions[0].priority_fee_per_gas;
            (0, threshold_fee, threshold_fee)
        },
        |idx| {
            // At least one transaction fits alongside the bundle.
            // The threshold is the fee of the last included transaction.
            let threshold_fee = transactions[idx].priority_fee_per_gas;

            // For recommended fee, look at included transactions (those above threshold)
            // and pick one at the specified percentile for a safety margin.
            let included = &transactions[..=idx];
            let recommended_fee = if included.len() <= 1 {
                threshold_fee
            } else {
                // Pick from the higher end of included transactions for safety.
                let pos = ((included.len() - 1) as f64 * (1.0 - percentile)).round() as usize;
                included[pos.min(included.len() - 1)].priority_fee_per_gas
            };

            (idx + 1, threshold_fee, recommended_fee)
        },
    );

    Ok(ResourceEstimate {
        threshold_priority_fee: threshold_fee,
        recommended_priority_fee: recommended_fee,
        cumulative_usage: included_usage,
        threshold_tx_count: supporting_count,
        total_transactions: transactions.len(),
    })
}

/// Returns a function that extracts the relevant resource usage from a transaction.
fn usage_extractor(resource: ResourceKind) -> fn(&MeteredTransaction) -> u128 {
    match resource {
        ResourceKind::GasUsed => |tx: &MeteredTransaction| tx.gas_used as u128,
        ResourceKind::ExecutionTime => |tx: &MeteredTransaction| tx.execution_time_us,
        ResourceKind::StateRootTime => |tx: &MeteredTransaction| tx.state_root_time_us,
        ResourceKind::DataAvailability => {
            |tx: &MeteredTransaction| tx.data_availability_bytes as u128
        }
    }
}

#[cfg(test)]
mod tests {
    use alloy_primitives::B256;

    use super::*;

    const DEFAULT_FEE: U256 = U256::from_limbs([1, 0, 0, 0]); // 1 wei

    fn tx(priority: u64, usage: u64) -> MeteredTransaction {
        let mut hash_bytes = [0u8; 32];
        hash_bytes[24..].copy_from_slice(&priority.to_be_bytes());
        MeteredTransaction {
            tx_hash: B256::new(hash_bytes),
            priority_fee_per_gas: U256::from(priority),
            gas_used: usage,
            execution_time_us: usage as u128,
            state_root_time_us: usage as u128,
            data_availability_bytes: usage,
        }
    }

    /// Creates a transaction with independent resource usage per dimension.
    fn tx_multi(
        priority: u64,
        gas: u64,
        exec_us: u128,
        state_root_us: u128,
        da_bytes: u64,
    ) -> MeteredTransaction {
        let mut hash_bytes = [0u8; 32];
        hash_bytes[24..].copy_from_slice(&priority.to_be_bytes());
        MeteredTransaction {
            tx_hash: B256::new(hash_bytes),
            priority_fee_per_gas: U256::from(priority),
            gas_used: gas,
            execution_time_us: exec_us,
            state_root_time_us: state_root_us,
            data_availability_bytes: da_bytes,
        }
    }

    #[test]
    fn compute_estimate_congested_resource() {
        // Limit: 30, Demand: 15
        // Transactions: priority=10 (10 gas), priority=5 (10 gas), priority=2 (10 gas)
        // Walking from top (highest fee):
        //   - Include tx priority=10: remaining = 30-10 = 20 >= 15 ok
        //   - Include tx priority=5:  remaining = 20-10 = 10 < 15 stop
        // Threshold = 10 (the last included tx's fee)
        let txs = [tx(10, 10), tx(5, 10), tx(2, 10)];
        let txs_refs: Vec<&MeteredTransaction> = txs.iter().collect();
        let quote = compute_estimate(
            ResourceKind::GasUsed,
            &txs_refs,
            15,
            30, // limit
            usage_extractor(ResourceKind::GasUsed),
            0.5,
            DEFAULT_FEE,
        )
        .expect("no error");
        assert_eq!(quote.threshold_priority_fee, U256::from(10));
        assert_eq!(quote.cumulative_usage, 10); // Only the first tx was included
        assert_eq!(quote.threshold_tx_count, 1);
        assert_eq!(quote.total_transactions, 3);
    }

    #[test]
    fn compute_estimate_uncongested_resource() {
        // Limit: 100, Demand: 15
        // All transactions fit with room to spare -> return default fee
        let txs = [tx(10, 10), tx(5, 10), tx(2, 10)];
        let txs_refs: Vec<&MeteredTransaction> = txs.iter().collect();
        let quote = compute_estimate(
            ResourceKind::GasUsed,
            &txs_refs,
            15,
            100, // limit is much larger than total usage
            usage_extractor(ResourceKind::GasUsed),
            0.5,
            DEFAULT_FEE,
        )
        .expect("no error");
        assert_eq!(quote.threshold_priority_fee, DEFAULT_FEE);
        assert_eq!(quote.recommended_priority_fee, DEFAULT_FEE);
        assert_eq!(quote.cumulative_usage, 30); // All txs included
        assert_eq!(quote.threshold_tx_count, 3);
    }

    #[test]
    fn compute_estimate_demand_exceeds_limit() {
        // Demand > Limit -> Error
        let txs = [tx(10, 10), tx(5, 10)];
        let txs_refs: Vec<&MeteredTransaction> = txs.iter().collect();
        let result = compute_estimate(
            ResourceKind::GasUsed,
            &txs_refs,
            50, // demand
            30, // limit
            usage_extractor(ResourceKind::GasUsed),
            0.5,
            DEFAULT_FEE,
        );
        assert!(matches!(
            result,
            Err(EstimateError::DemandExceedsCapacity {
                resource: ResourceKind::GasUsed,
                demand: 50,
                limit: 30,
            })
        ));
    }

    #[test]
    fn compute_estimate_exact_fit() {
        // Limit: 30, Demand: 20
        // Transactions: priority=10 (10 gas), priority=5 (10 gas)
        // After including tx priority=10: remaining = 20 >= 20 ok
        // After including tx priority=5: remaining = 10 < 20 stop
        let txs = [tx(10, 10), tx(5, 10)];
        let txs_refs: Vec<&MeteredTransaction> = txs.iter().collect();
        let quote = compute_estimate(
            ResourceKind::GasUsed,
            &txs_refs,
            20,
            30,
            usage_extractor(ResourceKind::GasUsed),
            0.5,
            DEFAULT_FEE,
        )
        .expect("no error");
        assert_eq!(quote.threshold_priority_fee, U256::from(10));
        assert_eq!(quote.cumulative_usage, 10);
        assert_eq!(quote.threshold_tx_count, 1);
    }

    #[test]
    fn compute_estimate_single_transaction() {
        // Single tx that fits
        let txs = [tx(10, 10)];
        let txs_refs: Vec<&MeteredTransaction> = txs.iter().collect();
        let quote = compute_estimate(
            ResourceKind::GasUsed,
            &txs_refs,
            15,
            30,
            usage_extractor(ResourceKind::GasUsed),
            0.5,
            DEFAULT_FEE,
        )
        .expect("no error");
        // After including the tx: remaining = 20 >= 15 ok
        // But we only have 1 tx, so it's uncongested
        assert_eq!(quote.threshold_priority_fee, DEFAULT_FEE);
        assert_eq!(quote.recommended_priority_fee, DEFAULT_FEE);
    }

    #[test]
    fn compute_estimate_no_room_for_any_tx() {
        // Limit: 25, Demand: 20
        // First tx uses 10, remaining = 15 < 20 -> can't even include first tx
        let txs = [tx(10, 10), tx(5, 10)];
        let txs_refs: Vec<&MeteredTransaction> = txs.iter().collect();
        let quote = compute_estimate(
            ResourceKind::GasUsed,
            &txs_refs,
            20,
            25,
            usage_extractor(ResourceKind::GasUsed),
            0.5,
            DEFAULT_FEE,
        )
        .expect("no error");
        // No transactions can be included, threshold is the highest fee
        assert_eq!(quote.threshold_priority_fee, U256::from(10));
        assert_eq!(quote.threshold_tx_count, 0);
        assert_eq!(quote.cumulative_usage, 0);
    }

    #[test]
    fn compute_estimate_empty_transactions() {
        // No transactions = uncongested, return default fee
        let txs_refs: Vec<&MeteredTransaction> = vec![];
        let quote = compute_estimate(
            ResourceKind::GasUsed,
            &txs_refs,
            15,
            30,
            usage_extractor(ResourceKind::GasUsed),
            0.5,
            DEFAULT_FEE,
        )
        .expect("no error");
        assert_eq!(quote.threshold_priority_fee, DEFAULT_FEE);
        assert_eq!(quote.recommended_priority_fee, DEFAULT_FEE);
    }

    const DEFAULT_LIMITS: ResourceLimits = ResourceLimits {
        gas_used: Some(25),
        execution_time_us: Some(100),
        state_root_time_us: None,
        data_availability_bytes: Some(100),
    };

    fn setup_estimator(
        limits: ResourceLimits,
    ) -> (Arc<RwLock<MeteringCache>>, PriorityFeeEstimator) {
        setup_estimator_with_target(limits, 1)
    }

    fn setup_estimator_with_target(
        limits: ResourceLimits,
        target_flashblocks_per_block: usize,
    ) -> (Arc<RwLock<MeteringCache>>, PriorityFeeEstimator) {
        let cache = Arc::new(RwLock::new(MeteringCache::new(4, target_flashblocks_per_block + 1)));
        let estimator = PriorityFeeEstimator::new(
            Arc::clone(&cache),
            0.5,
            limits,
            DEFAULT_FEE,
            target_flashblocks_per_block,
        );
        (cache, estimator)
    }

    #[test]
    #[should_panic(expected = "target_flashblocks_per_block must be greater than 0")]
    fn estimator_rejects_zero_target_flashblocks_per_block() {
        let _ = setup_estimator_with_target(DEFAULT_LIMITS, 0);
    }

    #[test]
    #[should_panic(expected = "percentile must be between 0.0 and 1.0 inclusive")]
    fn estimator_rejects_nan_percentile() {
        let cache = Arc::new(RwLock::new(MeteringCache::new(4, 2)));
        let _ = PriorityFeeEstimator::new(cache, f64::NAN, DEFAULT_LIMITS, DEFAULT_FEE, 1);
    }

    #[test]
    fn estimate_for_block_respects_limits() {
        let (cache, estimator) = setup_estimator(DEFAULT_LIMITS);
        {
            let mut guard = cache.write();
            guard.push_transaction(1, 1, tx(10, 10));
            guard.push_transaction(1, 1, tx(5, 10));
        }
        let demand = ResourceDemand { gas_used: Some(15), ..Default::default() };

        let estimates =
            estimator.estimate_for_block(Some(1), demand).expect("no error").expect("cached block");

        assert_eq!(estimates.block_number, 1);
        let gas_estimate = estimates.max_across_flashblocks.gas_used.expect("gas estimate present");
        assert_eq!(gas_estimate.threshold_priority_fee, U256::from(10));
    }

    #[test]
    fn estimate_for_block_propagates_limit_errors() {
        let mut limits = DEFAULT_LIMITS;
        limits.gas_used = Some(10);
        let (cache, estimator) = setup_estimator(limits);
        {
            let mut guard = cache.write();
            guard.push_transaction(1, 1, tx(10, 10));
            guard.push_transaction(1, 1, tx(5, 10));
        }
        let demand = ResourceDemand { gas_used: Some(15), ..Default::default() };

        let err = estimator
            .estimate_for_block(Some(1), demand)
            .expect_err("demand should exceed capacity");
        assert!(matches!(
            err,
            EstimateError::DemandExceedsCapacity {
                resource: ResourceKind::GasUsed,
                demand: 15,
                limit: 10
            }
        ));
    }

    #[test]
    fn estimate_rolling_aggregates_across_blocks() {
        let (cache, estimator) = setup_estimator(DEFAULT_LIMITS);
        {
            let mut guard = cache.write();
            // Block 1 → threshold 10
            guard.push_transaction(1, 1, tx(10, 10));
            guard.push_transaction(1, 1, tx(5, 10));
            // Block 2 → threshold 30
            guard.push_transaction(2, 1, tx(30, 10));
            guard.push_transaction(2, 1, tx(25, 10));
        }

        let demand = ResourceDemand { gas_used: Some(15), ..Default::default() };

        let rolling =
            estimator.estimate_rolling(demand).expect("no error").expect("estimates available");

        assert_eq!(rolling.blocks_sampled, 2);
        let gas_estimate = rolling.estimates.gas_used.expect("gas estimate present");
        // Median across [10, 30] = 30 (upper median for even count)
        assert_eq!(gas_estimate.threshold_priority_fee, U256::from(30));
        assert_eq!(gas_estimate.recommended_priority_fee, U256::from(30));
        assert_eq!(gas_estimate.cumulative_usage, 10);
        assert_eq!(gas_estimate.threshold_tx_count, 1);
        assert_eq!(gas_estimate.total_transactions, 2);
        assert_eq!(rolling.priority_fee, U256::from(30));
    }

    // === Rolling Estimate Tests ===

    #[test]
    fn estimate_rolling_returns_none_for_empty_cache() {
        let (_, estimator) = setup_estimator(DEFAULT_LIMITS);
        let demand = ResourceDemand { gas_used: Some(10), ..Default::default() };

        let result = estimator.estimate_rolling(demand).expect("no error");
        assert!(result.is_none());
    }

    #[test]
    fn estimate_for_block_returns_none_for_empty_cache() {
        let (_, estimator) = setup_estimator(DEFAULT_LIMITS);
        let demand = ResourceDemand { gas_used: Some(10), ..Default::default() };

        let result = estimator.estimate_for_block(Some(1), demand).expect("no error");
        assert!(result.is_none());
    }

    #[test]
    fn estimate_for_block_returns_none_for_missing_block() {
        let (cache, estimator) = setup_estimator(DEFAULT_LIMITS);
        {
            let mut guard = cache.write();
            guard.push_transaction(1, 1, tx(10, 10));
        }
        let demand = ResourceDemand { gas_used: Some(10), ..Default::default() };

        let result = estimator.estimate_for_block(Some(2), demand).expect("no error");
        assert!(result.is_none());
    }

    #[test]
    fn estimate_for_block_uses_most_recent_when_block_none() {
        let (cache, estimator) = setup_estimator(DEFAULT_LIMITS);
        {
            let mut guard = cache.write();
            guard.push_transaction(1, 1, tx(10, 10));
            guard.push_transaction(2, 1, tx(20, 10));
            guard.push_transaction(3, 1, tx(30, 10));
        }
        let demand = ResourceDemand { gas_used: Some(10), ..Default::default() };

        let result =
            estimator.estimate_for_block(None, demand).expect("no error").expect("block exists");

        assert_eq!(result.block_number, 3);
    }

    #[test]
    fn estimate_rolling_computes_median_for_odd_blocks() {
        let (cache, estimator) = setup_estimator(DEFAULT_LIMITS);
        {
            let mut guard = cache.write();
            // Block 1: threshold 10
            guard.push_transaction(1, 1, tx(10, 10));
            guard.push_transaction(1, 1, tx(5, 10));
            // Block 2: threshold 20
            guard.push_transaction(2, 1, tx(20, 10));
            guard.push_transaction(2, 1, tx(15, 10));
            // Block 3: threshold 30
            guard.push_transaction(3, 1, tx(30, 10));
            guard.push_transaction(3, 1, tx(25, 10));
        }
        let demand = ResourceDemand { gas_used: Some(15), ..Default::default() };

        let result = estimator.estimate_rolling(demand).expect("no error").expect("has data");

        assert_eq!(result.blocks_sampled, 3);
        let gas_estimate = result.estimates.gas_used.expect("gas estimate");
        assert_eq!(gas_estimate.recommended_priority_fee, U256::from(20));
    }

    #[test]
    fn estimate_rolling_priority_fee_is_max_across_resources() {
        let limits = ResourceLimits {
            gas_used: Some(50),
            execution_time_us: Some(200),
            state_root_time_us: None,
            data_availability_bytes: Some(200),
        };
        let (cache, estimator) = setup_estimator(limits);
        {
            let mut guard = cache.write();
            guard.push_transaction(1, 1, tx(100, 10));
            guard.push_transaction(1, 1, tx(50, 10));
        }
        let demand = ResourceDemand {
            gas_used: Some(25),
            execution_time_us: Some(25),
            data_availability_bytes: Some(25),
            ..Default::default()
        };

        let result = estimator.estimate_rolling(demand).expect("no error").expect("has data");

        let gas_fee =
            result.estimates.gas_used.map(|e| e.recommended_priority_fee).unwrap_or(U256::ZERO);
        let exec_fee = result
            .estimates
            .execution_time
            .map(|e| e.recommended_priority_fee)
            .unwrap_or(U256::ZERO);
        let da_fee = result
            .estimates
            .data_availability
            .map(|e| e.recommended_priority_fee)
            .unwrap_or(U256::ZERO);

        let expected_max = gas_fee.max(exec_fee).max(da_fee);
        assert_eq!(result.priority_fee, expected_max);
    }

    #[test]
    fn collect_sorted_transaction_prefixes_merges_each_flashblock_into_descending_prefixes() {
        let tx_100 = tx(100, 1);
        let tx_95 = tx(95, 1);
        let tx_90 = tx(90, 1);
        let tx_80 = tx(80, 1);
        let tx_70 = tx(70, 1);
        let tx_60 = tx(60, 1);

        let flashblocks = vec![
            FlashblockEstimateInput { flashblock_index: 1, transactions: vec![&tx_100, &tx_80] },
            FlashblockEstimateInput { flashblock_index: 2, transactions: vec![&tx_90, &tx_70] },
            FlashblockEstimateInput { flashblock_index: 3, transactions: vec![&tx_95, &tx_60] },
        ];

        let prefixes = collect_sorted_transaction_prefixes(&flashblocks);

        let priorities: Vec<Vec<U256>> = prefixes
            .iter()
            .map(|prefix| prefix.iter().map(|tx| tx.priority_fee_per_gas).collect())
            .collect();

        assert_eq!(
            priorities,
            vec![
                vec![U256::from(100), U256::from(80)],
                vec![U256::from(100), U256::from(90), U256::from(80), U256::from(70)],
                vec![
                    U256::from(100),
                    U256::from(95),
                    U256::from(90),
                    U256::from(80),
                    U256::from(70),
                    U256::from(60),
                ],
            ]
        );
    }

    #[test]
    fn estimate_for_block_resetting_resources_use_local_flashblock_competition() {
        let limits = ResourceLimits {
            gas_used: None,
            execution_time_us: Some(30),
            state_root_time_us: None,
            data_availability_bytes: None,
        };
        let (cache, estimator) = setup_estimator_with_target(limits, 2);
        {
            let mut guard = cache.write();
            guard.push_transaction(1, 1, tx_multi(100, 0, 15, 0, 0));
            guard.push_transaction(1, 1, tx_multi(90, 0, 15, 0, 0));
            guard.push_transaction(1, 2, tx_multi(80, 0, 15, 0, 0));
            guard.push_transaction(1, 2, tx_multi(70, 0, 15, 0, 0));
        }

        let demand = ResourceDemand { execution_time_us: Some(15), ..Default::default() };
        let result =
            estimator.estimate_for_block(Some(1), demand).expect("no error").expect("block exists");

        assert_eq!(result.flashblocks.len(), 2);
        let first = result.flashblocks[0]
            .estimates
            .execution_time
            .as_ref()
            .expect("execution time estimate");
        let second = result.flashblocks[1]
            .estimates
            .execution_time
            .as_ref()
            .expect("execution time estimate");

        assert_eq!(first.threshold_priority_fee, U256::from(100));
        assert_eq!(second.threshold_priority_fee, U256::from(80));
    }

    #[test]
    fn estimate_for_block_resetting_resource_prefers_non_empty_summary_on_fee_tie() {
        let limits = ResourceLimits {
            gas_used: None,
            execution_time_us: Some(100),
            state_root_time_us: None,
            data_availability_bytes: None,
        };
        let (cache, estimator) = setup_estimator_with_target(limits, 4);
        {
            let mut guard = cache.write();
            guard.push_transaction(1, 3, tx_multi(100, 0, 0, 0, 0));
            guard.push_transaction(1, 3, tx_multi(90, 0, 0, 0, 0));
        }

        let demand = ResourceDemand { execution_time_us: Some(10), ..Default::default() };
        let result =
            estimator.estimate_for_block(Some(1), demand).expect("no error").expect("block exists");

        let execution_summary =
            result.max_across_flashblocks.execution_time.as_ref().expect("execution time summary");
        assert_eq!(execution_summary.recommended_priority_fee, DEFAULT_FEE);
        assert_eq!(execution_summary.total_transactions, 2);
        assert_eq!(execution_summary.threshold_tx_count, 2);
    }

    #[test]
    fn estimate_rolling_resetting_resource_prefers_non_empty_summary_on_fee_tie() {
        let limits = ResourceLimits {
            gas_used: None,
            execution_time_us: Some(100),
            state_root_time_us: None,
            data_availability_bytes: None,
        };
        let (cache, estimator) = setup_estimator_with_target(limits, 4);
        {
            let mut guard = cache.write();
            guard.push_transaction(1, 3, tx_multi(100, 0, 0, 0, 0));
            guard.push_transaction(1, 3, tx_multi(90, 0, 0, 0, 0));
        }

        let demand = ResourceDemand { execution_time_us: Some(10), ..Default::default() };
        let result = estimator.estimate_rolling(demand).expect("no error").expect("has data");

        let execution_summary =
            result.estimates.execution_time.as_ref().expect("execution time summary");
        assert_eq!(execution_summary.recommended_priority_fee, DEFAULT_FEE);
        assert_eq!(execution_summary.total_transactions, 2);
        assert_eq!(execution_summary.threshold_tx_count, 2);
    }

    #[test]
    fn estimate_for_block_accumulating_resources_follow_builder_cumulative_targets() {
        let limits = ResourceLimits {
            gas_used: Some(100),
            execution_time_us: None,
            state_root_time_us: None,
            data_availability_bytes: None,
        };
        let (cache, estimator) = setup_estimator_with_target(limits, 4);
        {
            let mut guard = cache.write();
            guard.push_transaction(1, 1, tx_multi(100, 20, 0, 0, 0));
            guard.push_transaction(1, 3, tx_multi(90, 20, 0, 0, 0));
        }

        let demand = ResourceDemand { gas_used: Some(40), ..Default::default() };
        let result =
            estimator.estimate_for_block(None, demand).expect("no error").expect("block exists");

        assert_eq!(result.flashblocks.len(), 4);
        assert!(result.flashblocks[0].estimates.gas_used.is_none());

        let later_flashblock = result.flashblocks[1]
            .estimates
            .gas_used
            .as_ref()
            .expect("gas estimate for later flashblock");
        assert_eq!(later_flashblock.threshold_priority_fee, U256::from(100));

        let block_summary = result.max_across_flashblocks.gas_used.expect("gas summary");
        assert_eq!(block_summary.threshold_priority_fee, U256::from(100));
    }

    #[test]
    fn estimate_rolling_accumulating_resources_uses_block_end_budget() {
        let limits = ResourceLimits {
            gas_used: Some(100),
            execution_time_us: None,
            state_root_time_us: None,
            data_availability_bytes: None,
        };
        let (cache, estimator) = setup_estimator_with_target(limits, 4);
        {
            let mut guard = cache.write();
            guard.push_transaction(1, 1, tx_multi(100, 20, 0, 0, 0));
            guard.push_transaction(1, 3, tx_multi(90, 20, 0, 0, 0));
        }

        let demand = ResourceDemand { gas_used: Some(40), ..Default::default() };
        let rolling =
            estimator.estimate_rolling(demand).expect("no error").expect("rolling estimate");

        let gas_estimate = rolling.estimates.gas_used.expect("gas estimate");
        assert_eq!(gas_estimate.threshold_priority_fee, DEFAULT_FEE);
        assert_eq!(gas_estimate.recommended_priority_fee, DEFAULT_FEE);
        assert_eq!(gas_estimate.cumulative_usage, 40);
        assert_eq!(gas_estimate.threshold_tx_count, 2);
    }

    #[test]
    fn estimate_for_block_ignores_base_flashblock_for_budgeted_resources() {
        let limits = ResourceLimits {
            gas_used: Some(20),
            execution_time_us: None,
            state_root_time_us: None,
            data_availability_bytes: None,
        };
        let (cache, estimator) = setup_estimator(limits);
        {
            let mut guard = cache.write();
            guard.push_transaction(1, 0, tx_multi(100, 10, 0, 0, 0));
            guard.push_transaction(1, 1, tx_multi(10, 1, 0, 0, 0));
        }

        let demand = ResourceDemand { gas_used: Some(15), ..Default::default() };
        let result =
            estimator.estimate_for_block(Some(1), demand).expect("no error").expect("block exists");

        let gas_estimate = result.flashblocks[0]
            .estimates
            .gas_used
            .as_ref()
            .expect("gas estimate for first tx-pool flashblock");
        assert_eq!(gas_estimate.threshold_priority_fee, DEFAULT_FEE);
        assert_eq!(gas_estimate.recommended_priority_fee, DEFAULT_FEE);
    }
}
