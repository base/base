//! Priority fee estimation based on resource consumption in flashblocks.

use std::sync::Arc;

use alloy_primitives::U256;
use parking_lot::RwLock;
use reth_optimism_payload_builder::config::OpDAConfig;

use crate::base::cache::{MeteredTransaction, MeteringCache};

/// Errors that can occur during priority fee estimation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum EstimateError {
    /// The bundle's resource demand exceeds the configured capacity limit.
    DemandExceedsCapacity {
        /// The resource that exceeded capacity.
        resource: ResourceKind,
        /// The requested demand.
        demand: u128,
        /// The configured limit.
        limit: u128,
    },
}

impl std::fmt::Display for EstimateError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::DemandExceedsCapacity { resource, demand, limit } => {
                write!(
                    f,
                    "bundle {} demand ({}) exceeds capacity limit ({})",
                    resource.as_name(),
                    demand,
                    limit
                )
            }
        }
    }
}

impl std::error::Error for EstimateError {}

/// Configured capacity limits for each resource type.
///
/// These values define the maximum capacity available per flashblock (or per block
/// for "use-it-or-lose-it" resources). The estimator uses these limits to determine
/// when resources are congested.
#[derive(Debug, Clone, Copy, Default)]
pub struct ResourceLimits {
    /// Gas limit per flashblock.
    pub gas_used: Option<u64>,
    /// Execution time budget in microseconds.
    pub execution_time_us: Option<u128>,
    /// State root computation time budget in microseconds.
    pub state_root_time_us: Option<u128>,
    /// Data availability bytes limit per flashblock.
    pub data_availability_bytes: Option<u64>,
}

impl ResourceLimits {
    /// Returns the limit for the given resource kind.
    fn limit_for(&self, resource: ResourceKind) -> Option<u128> {
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

impl ResourceKind {
    /// Returns all resource kinds in a fixed order.
    pub const fn all() -> [Self; 4] {
        [Self::GasUsed, Self::ExecutionTime, Self::StateRootTime, Self::DataAvailability]
    }

    /// Returns `true` if this resource is "use-it-or-lose-it", meaning capacity
    /// that isn't consumed in one flashblock cannot be reclaimed in later ones.
    ///
    /// Execution time is the canonical example: the block builder has a fixed
    /// time budget per block, and unused time in flashblock 0 doesn't roll over
    /// to flashblock 1. For these resources, the estimator aggregates usage
    /// across all flashblocks rather than evaluating each flashblock in isolation.
    ///
    /// Other resources like gas and DA bytes are bounded per-block but are
    /// evaluated per-flashblock since their limits apply independently.
    const fn use_it_or_lose_it(self) -> bool {
        matches!(self, Self::ExecutionTime)
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
    fn demand_for(&self, resource: ResourceKind) -> Option<u128> {
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
/// was not requested or could not be estimated (e.g., demand exceeds capacity).
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
    /// Flashblock index.
    pub flashblock_index: u64,
    /// Per-resource estimates.
    pub estimates: ResourceEstimates,
}

/// Aggregated estimates for a block.
#[derive(Debug, Clone)]
pub struct BlockPriorityEstimates {
    /// Block number.
    pub block_number: u64,
    /// Per-flashblock estimates.
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

/// Computes resource fee estimates based on cached flashblock metering data.
#[derive(Debug)]
pub struct PriorityFeeEstimator {
    cache: Arc<RwLock<MeteringCache>>,
    percentile: f64,
    limits: ResourceLimits,
    default_priority_fee: U256,
    /// Optional shared DA config from the miner RPC. When set, the estimator uses
    /// `max_da_block_size` from this config instead of `limits.data_availability_bytes`.
    /// This allows dynamic updates via `miner_setMaxDASize`.
    da_config: Option<OpDAConfig>,
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
    /// - `da_config`: Optional shared DA config for dynamic DA limit updates.
    pub const fn new(
        cache: Arc<RwLock<MeteringCache>>,
        percentile: f64,
        limits: ResourceLimits,
        default_priority_fee: U256,
        da_config: Option<OpDAConfig>,
    ) -> Self {
        Self { cache, percentile, limits, default_priority_fee, da_config }
    }

    /// Returns the current DA block size limit, preferring the dynamic `OpDAConfig` value
    /// if available, otherwise falling back to the static limit.
    pub fn max_da_block_size(&self) -> Option<u64> {
        self.da_config
            .as_ref()
            .and_then(|c| c.max_da_block_size())
            .or(self.limits.data_availability_bytes)
    }

    /// Returns the limit for the given resource kind, using dynamic config where available.
    fn limit_for(&self, resource: ResourceKind) -> Option<u128> {
        match resource {
            ResourceKind::DataAvailability => self.max_da_block_size().map(|v| v as u128),
            _ => self.limits.limit_for(resource),
        }
    }

    /// Returns fee estimates for the provided block. If `block_number` is `None`
    /// the most recent block in the cache is used.
    ///
    /// Returns `Ok(None)` if the cache is empty, the requested block is not cached,
    /// or no transactions exist in the cached flashblocks.
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

        let block_number = block_metrics.block_number;

        // Clone transactions per flashblock so we can drop the lock.
        // Transactions are pre-sorted descending by priority fee in the cache.
        let mut flashblock_transactions = Vec::new();
        let mut total_tx_count = 0usize;
        for flashblock in block_metrics.flashblocks() {
            let txs: Vec<MeteredTransaction> = flashblock.transactions().to_vec();
            if txs.is_empty() {
                continue;
            }
            total_tx_count += txs.len();
            flashblock_transactions.push((flashblock.flashblock_index, txs));
        }
        drop(cache_guard);

        if flashblock_transactions.is_empty() {
            return Ok(None);
        }

        // Build the aggregate list for use-it-or-lose-it resources.
        // Need to sort since we're combining multiple pre-sorted flashblocks.
        let mut aggregate_refs: Vec<&MeteredTransaction> = Vec::with_capacity(total_tx_count);
        for (_, txs) in &flashblock_transactions {
            aggregate_refs.extend(txs.iter());
        }
        aggregate_refs.sort_by(|a, b| b.priority_fee_per_gas.cmp(&a.priority_fee_per_gas));

        let mut flashblock_estimates = Vec::new();

        for (flashblock_index, txs) in &flashblock_transactions {
            // Build a reference slice for this flashblock's transactions.
            let txs_refs: Vec<&MeteredTransaction> = txs.iter().collect();

            let mut estimates = ResourceEstimates::default();
            for resource in ResourceKind::all() {
                let Some(demand_value) = demand.demand_for(resource) else {
                    continue;
                };
                let Some(limit_value) = self.limit_for(resource) else {
                    continue;
                };

                let transactions: &[&MeteredTransaction] =
                    if resource.use_it_or_lose_it() { &aggregate_refs } else { &txs_refs };
                let estimate = compute_estimate(
                    resource,
                    transactions,
                    demand_value,
                    limit_value,
                    usage_extractor(resource),
                    self.percentile,
                    self.default_priority_fee,
                )?;

                estimates.set(resource, estimate);
            }

            flashblock_estimates.push(FlashblockResourceEstimates {
                flashblock_index: *flashblock_index,
                estimates,
            });
        }

        let (min_across_flashblocks, max_across_flashblocks) =
            compute_min_max_estimates(&flashblock_estimates);

        Ok(Some(BlockPriorityEstimates {
            block_number,
            flashblocks: flashblock_estimates,
            min_across_flashblocks,
            max_across_flashblocks,
        }))
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
        let block_numbers: Vec<u64> = cache_guard.blocks_desc().map(|b| b.block_number).collect();
        drop(cache_guard);

        if block_numbers.is_empty() {
            return Ok(None);
        }

        // Collect per-block max estimates. Propagate any errors.
        let mut block_estimates = Vec::new();
        for &n in &block_numbers {
            if let Some(est) = self.estimate_for_block(Some(n), demand)? {
                block_estimates.push(est.max_across_flashblocks);
            }
        }

        if block_estimates.is_empty() {
            return Ok(None);
        }

        // Compute median fee for each resource across blocks.
        let mut estimates = ResourceEstimates::default();
        let mut max_fee = U256::ZERO;

        for resource in ResourceKind::all() {
            let mut fees: Vec<U256> = block_estimates
                .iter()
                .filter_map(|e| e.get(resource))
                .map(|e| e.recommended_priority_fee)
                .collect();

            if fees.is_empty() {
                continue;
            }

            fees.sort();
            let median_fee = fees[fees.len() / 2];
            max_fee = max_fee.max(median_fee);

            estimates.set(
                resource,
                ResourceEstimate {
                    threshold_priority_fee: median_fee,
                    recommended_priority_fee: median_fee,
                    cumulative_usage: 0,
                    threshold_tx_count: 0,
                    total_transactions: 0,
                },
            );
        }

        if estimates.is_empty() {
            return Ok(None);
        }

        Ok(Some(RollingPriorityEstimate {
            blocks_sampled: block_numbers.len(),
            estimates,
            priority_fee: max_fee,
        }))
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
            let percentile = percentile.clamp(0.0, 1.0);
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

/// Computes the minimum and maximum recommended fees across all flashblocks.
///
/// Returns two `ResourceEstimates`:
/// - First: For each resource, the estimate with the lowest recommended fee (easiest inclusion).
/// - Second: For each resource, the estimate with the highest recommended fee (most competitive).
fn compute_min_max_estimates(
    flashblocks: &[FlashblockResourceEstimates],
) -> (ResourceEstimates, ResourceEstimates) {
    let mut min_estimates = ResourceEstimates::default();
    let mut max_estimates = ResourceEstimates::default();

    for flashblock in flashblocks {
        for (resource, estimate) in flashblock.estimates.iter() {
            // Update min.
            let current_min = min_estimates.get(resource);
            if current_min.is_none()
                || estimate.recommended_priority_fee < current_min.unwrap().recommended_priority_fee
            {
                min_estimates.set(resource, estimate.clone());
            }

            // Update max.
            let current_max = max_estimates.get(resource);
            if current_max.is_none()
                || estimate.recommended_priority_fee > current_max.unwrap().recommended_priority_fee
            {
                max_estimates.set(resource, estimate.clone());
            }
        }
    }

    (min_estimates, max_estimates)
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
        let cache = Arc::new(RwLock::new(MeteringCache::new(4)));
        let estimator = PriorityFeeEstimator::new(cache.clone(), 0.5, limits, DEFAULT_FEE, None);
        (cache, estimator)
    }

    #[test]
    fn estimate_for_block_respects_limits() {
        let (cache, estimator) = setup_estimator(DEFAULT_LIMITS);
        {
            let mut guard = cache.write();
            guard.push_transaction(1, 0, tx(10, 10));
            guard.push_transaction(1, 0, tx(5, 10));
        }
        let mut demand = ResourceDemand::default();
        demand.gas_used = Some(15);

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
            guard.push_transaction(1, 0, tx(10, 10));
            guard.push_transaction(1, 0, tx(5, 10));
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
            guard.push_transaction(1, 0, tx(10, 10));
            guard.push_transaction(1, 0, tx(5, 10));
            // Block 2 → threshold 30
            guard.push_transaction(2, 0, tx(30, 10));
            guard.push_transaction(2, 0, tx(25, 10));
        }

        let demand = ResourceDemand { gas_used: Some(15), ..Default::default() };

        let rolling =
            estimator.estimate_rolling(demand).expect("no error").expect("estimates available");

        assert_eq!(rolling.blocks_sampled, 2);
        let gas_estimate = rolling.estimates.gas_used.expect("gas estimate present");
        // Median across [10, 30] = 30 (upper median for even count)
        assert_eq!(gas_estimate.recommended_priority_fee, U256::from(30));
        assert_eq!(rolling.priority_fee, U256::from(30));
    }
}
