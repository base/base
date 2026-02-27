//! Priority fee estimation based on resource consumption.
//!
//! This module provides the core algorithm for estimating the priority fee needed
//! to achieve inclusion in a block, based on historical transaction data.

use std::cmp::Reverse;

use alloy_primitives::U256;

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
/// These values define the maximum capacity available per block. The estimator
/// uses these limits to determine when resources are congested.
#[derive(Debug, Clone, Copy, Default)]
pub struct ResourceLimits {
    /// Gas limit per block.
    pub gas_used: Option<u64>,
    /// Execution time budget in microseconds.
    pub execution_time_us: Option<u128>,
    /// State root computation time budget in microseconds.
    pub state_root_time_us: Option<u128>,
    /// Data availability bytes limit per block.
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

/// Resources that influence block inclusion ordering.
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

/// A transaction with its resource consumption metrics.
#[derive(Debug, Clone)]
pub struct MeteredTransaction {
    /// Priority fee per gas paid by this transaction.
    pub priority_fee_per_gas: U256,
    /// Gas consumed.
    pub gas_used: u64,
    /// Execution time in microseconds.
    pub execution_time_us: u128,
    /// State root computation time in microseconds.
    pub state_root_time_us: u128,
    /// Data availability bytes.
    pub data_availability_bytes: u64,
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

/// Estimates priority fees for all configured resources given a list of transactions.
///
/// This is a simple single-block estimation that treats all transactions as a single pool.
///
/// # Arguments
///
/// * `transactions` - Transactions from a block, will be sorted by priority fee descending
/// * `demand` - Resource demand for the bundle being priced
/// * `limits` - Configured resource limits
/// * `percentile` - Percentile for recommended fee calculation
/// * `default_fee` - Fee to return when resources are uncongested
///
/// Returns `Ok(None)` if no transactions are provided.
/// Returns `Err` if bundle demand exceeds any resource limit.
pub(crate) fn estimate_from_transactions(
    transactions: &[MeteredTransaction],
    demand: ResourceDemand,
    limits: &ResourceLimits,
    percentile: f64,
    default_fee: U256,
) -> Result<Option<(ResourceEstimates, U256)>, EstimateError> {
    if transactions.is_empty() {
        return Ok(None);
    }

    // Sort transactions by priority fee descending
    let mut sorted: Vec<&MeteredTransaction> = transactions.iter().collect();
    sorted.sort_by_key(|tx| Reverse(tx.priority_fee_per_gas));

    let mut estimates = ResourceEstimates::default();
    let mut max_fee = U256::ZERO;

    for resource in ResourceKind::all() {
        let Some(demand_value) = demand.demand_for(resource) else {
            continue;
        };
        let Some(limit_value) = limits.limit_for(resource) else {
            continue;
        };

        let estimate = compute_estimate(
            resource,
            &sorted,
            demand_value,
            limit_value,
            usage_extractor(resource),
            percentile,
            default_fee,
        )?;

        max_fee = max_fee.max(estimate.recommended_priority_fee);
        estimates.set(resource, estimate);
    }

    if estimates.is_empty() {
        return Ok(None);
    }

    Ok(Some((estimates, max_fee)))
}

#[cfg(test)]
mod tests {
    use super::*;

    const DEFAULT_FEE: U256 = U256::from_limbs([1, 0, 0, 0]); // 1 wei

    fn tx(priority: u64, usage: u64) -> MeteredTransaction {
        MeteredTransaction {
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
        MeteredTransaction {
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

    #[test]
    fn estimate_from_transactions_basic() {
        let txs = vec![tx(10, 10), tx(5, 10), tx(2, 10)];
        let demand = ResourceDemand { gas_used: Some(15), ..Default::default() };
        let limits = ResourceLimits { gas_used: Some(30), ..Default::default() };

        let result = estimate_from_transactions(&txs, demand, &limits, 0.5, DEFAULT_FEE)
            .expect("no error")
            .expect("has estimates");

        let (estimates, max_fee) = result;
        let gas_estimate = estimates.gas_used.expect("gas estimate present");
        assert_eq!(gas_estimate.threshold_priority_fee, U256::from(10));
        assert_eq!(max_fee, U256::from(10));
    }

    #[test]
    fn estimate_independent_dimensions() {
        // Demonstrate that each resource dimension is evaluated independently.
        //
        // Transactions have low gas but high execution time:
        //   priority=10: gas=5,  exec_time=15
        //   priority=5:  gas=5,  exec_time=15
        //   priority=2:  gas=5,  exec_time=15
        //
        // Gas (limit 100, demand 10):
        //   Total usage = 15, remaining = 85 >= 10 → uncongested → default fee
        //
        // Execution time (limit 30, demand 15):
        //   Include priority=10: remaining = 30-15 = 15 >= 15 → ok
        //   Include priority=5:  remaining = 15-15 = 0 < 15 → stop
        //   Congested → threshold = 10
        let txs =
            vec![tx_multi(10, 5, 15, 0, 0), tx_multi(5, 5, 15, 0, 0), tx_multi(2, 5, 15, 0, 0)];
        let demand = ResourceDemand {
            gas_used: Some(10),
            execution_time_us: Some(15),
            ..Default::default()
        };
        let limits = ResourceLimits {
            gas_used: Some(100),
            execution_time_us: Some(30),
            ..Default::default()
        };

        let (estimates, max_fee) =
            estimate_from_transactions(&txs, demand, &limits, 0.5, DEFAULT_FEE)
                .expect("no error")
                .expect("has estimates");

        // Gas is uncongested → default fee
        let gas_est = estimates.gas_used.expect("gas estimate");
        assert_eq!(gas_est.recommended_priority_fee, DEFAULT_FEE);

        // Execution time is congested → fee driven by competition
        let exec_est = estimates.execution_time.expect("exec estimate");
        assert_eq!(exec_est.threshold_priority_fee, U256::from(10));
        assert!(exec_est.recommended_priority_fee > DEFAULT_FEE);

        // Max fee is driven by the congested dimension (execution time)
        assert_eq!(max_fee, exec_est.recommended_priority_fee);
    }
}
