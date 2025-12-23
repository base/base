//! Response types for the metered priority fee RPC endpoint.

use alloy_primitives::U256;
use tips_core::types::MeterBundleResponse;

use crate::{ResourceEstimates, RollingPriorityEstimate};

/// Human-friendly representation of a resource fee quote.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ResourceFeeEstimateResponse {
    /// Resource name (gasUsed, executionTime, etc).
    pub resource: String,
    /// Minimum fee to displace enough capacity.
    pub threshold_priority_fee: U256,
    /// Recommended fee with safety margin.
    pub recommended_priority_fee: U256,
    /// Cumulative resource usage above threshold.
    pub cumulative_usage: U256,
    /// Number of transactions above threshold.
    pub threshold_tx_count: u64,
    /// Total transactions considered.
    pub total_transactions: u64,
}

/// Response payload for `base_meteredPriorityFeePerGas`.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct MeteredPriorityFeeResponse {
    /// Bundled metering results.
    #[serde(flatten)]
    pub meter_bundle: MeterBundleResponse,
    /// Recommended priority fee (max across all resources and median across recent blocks).
    pub priority_fee: U256,
    /// Number of recent blocks used to compute the rolling estimate.
    pub blocks_sampled: u64,
    /// Per-resource estimates (median across sampled blocks).
    pub resource_estimates: Vec<ResourceFeeEstimateResponse>,
}

/// Converts a rolling estimate to the response format.
pub fn build_priority_fee_response(
    meter_bundle: MeterBundleResponse,
    estimate: RollingPriorityEstimate,
) -> MeteredPriorityFeeResponse {
    let resource_estimates = build_resource_estimate_responses(&estimate.estimates);

    MeteredPriorityFeeResponse {
        meter_bundle,
        priority_fee: estimate.priority_fee,
        blocks_sampled: estimate.blocks_sampled as u64,
        resource_estimates,
    }
}

fn build_resource_estimate_responses(
    estimates: &ResourceEstimates,
) -> Vec<ResourceFeeEstimateResponse> {
    estimates
        .iter()
        .map(|(kind, est)| ResourceFeeEstimateResponse {
            resource: kind.as_camel_case().to_string(),
            threshold_priority_fee: est.threshold_priority_fee,
            recommended_priority_fee: est.recommended_priority_fee,
            cumulative_usage: U256::from(est.cumulative_usage),
            threshold_tx_count: est.threshold_tx_count.try_into().unwrap_or(u64::MAX),
            total_transactions: est.total_transactions.try_into().unwrap_or(u64::MAX),
        })
        .collect()
}
