//! Metrics observer for Beryl-native precompile calls.

#[cfg(test)]
use core::sync::atomic::{AtomicUsize, Ordering};

#[cfg(feature = "metrics")]
use base_common_precompiles::PrecompileCallStatus;
use base_common_precompiles::{
    PrecompileCallMetric, PrecompileCallObserver, PrecompileCallOutcome,
};
#[cfg(feature = "metrics")]
use metrics::SharedString;

#[cfg(feature = "metrics")]
base_metrics::define_metrics! {
    base.beryl.precompile,
    struct = BerylPrecompileMetrics,
    #[describe("Total Beryl native precompile calls")]
    #[label(precompile)]
    #[label(method)]
    #[label(variant)]
    #[label(status)]
    calls_total: counter,
    #[describe("Beryl native precompile call duration in seconds")]
    #[label(precompile)]
    #[label(method)]
    #[label(variant)]
    #[label(status)]
    duration_seconds: histogram,
    #[describe("Beryl native precompile calldata byte size")]
    #[label(precompile)]
    #[label(method)]
    #[label(variant)]
    #[label(status)]
    input_bytes: histogram,
    #[describe("Beryl native precompile gas used")]
    #[label(precompile)]
    #[label(method)]
    #[label(variant)]
    #[label(status)]
    gas_used: histogram,
    #[describe("Beryl native precompile state gas used")]
    #[label(precompile)]
    #[label(method)]
    #[label(variant)]
    #[label(status)]
    state_gas_used: histogram,
    #[describe("Beryl native precompile gas refunded")]
    #[label(precompile)]
    #[label(method)]
    #[label(variant)]
    #[label(status)]
    gas_refunded: histogram,
    #[describe("Beryl native precompile errors by bounded class")]
    #[label(precompile)]
    #[label(method)]
    #[label(variant)]
    #[label(error)]
    errors_total: counter,
    #[describe("Successful Beryl native precompile calls that reported zero gas used")]
    #[label(precompile)]
    #[label(method)]
    #[label(variant)]
    zero_gas_success_total: counter,
    #[describe("B-20 tokens created by variant")]
    #[label(name = "variant", default = ["asset", "stablecoin"])]
    b20_created_total: counter,
    #[describe("Beryl native precompile batch item counts")]
    #[label(precompile)]
    #[label(method)]
    #[label(variant)]
    batch_items: histogram,
    #[describe("Beryl native precompile internal call counts")]
    #[label(precompile)]
    #[label(method)]
    #[label(variant)]
    internal_calls: histogram,
    #[describe("Beryl native precompile internal call byte totals")]
    #[label(precompile)]
    #[label(method)]
    #[label(variant)]
    internal_call_bytes: histogram,
}

#[cfg(test)]
static RECORDED_CALLS: AtomicUsize = AtomicUsize::new(0);

/// Concrete observer that emits Beryl precompile metrics through the node metrics recorder.
#[derive(Debug, Default, Clone, Copy)]
pub struct BerylPrecompileMetricsObserver;

impl BerylPrecompileMetricsObserver {
    /// Resets the test-only call counter.
    #[cfg(test)]
    pub fn reset_recorded_calls_for_test() {
        RECORDED_CALLS.store(0, Ordering::SeqCst);
    }

    /// Returns the test-only call counter.
    #[cfg(test)]
    pub fn recorded_calls_for_test() -> usize {
        RECORDED_CALLS.load(Ordering::SeqCst)
    }
}

impl PrecompileCallObserver for BerylPrecompileMetricsObserver {
    fn record_call(&self, call: &PrecompileCallMetric, outcome: &PrecompileCallOutcome) {
        #[cfg(test)]
        RECORDED_CALLS.fetch_add(1, Ordering::SeqCst);

        #[cfg(feature = "metrics")]
        {
            let method: SharedString = call.method.clone().into_owned().into();
            let variant = call.variant_label();
            let status = outcome.status.as_label();
            let gas_refunded = outcome.gas_refunded.max(0) as f64;

            BerylPrecompileMetrics::calls_total(call.precompile, method.clone(), variant, status)
                .increment(1);
            BerylPrecompileMetrics::input_bytes(call.precompile, method.clone(), variant, status)
                .record(call.input_bytes as f64);
            BerylPrecompileMetrics::gas_used(call.precompile, method.clone(), variant, status)
                .record(outcome.gas_used as f64);
            BerylPrecompileMetrics::state_gas_used(
                call.precompile,
                method.clone(),
                variant,
                status,
            )
            .record(outcome.state_gas_used as f64);
            BerylPrecompileMetrics::gas_refunded(call.precompile, method.clone(), variant, status)
                .record(gas_refunded);

            if let Some(duration_seconds) = outcome.duration_seconds {
                BerylPrecompileMetrics::duration_seconds(
                    call.precompile,
                    method.clone(),
                    variant,
                    status,
                )
                .record(duration_seconds);
            }
            if let Some(error) = outcome.error {
                BerylPrecompileMetrics::errors_total(
                    call.precompile,
                    method.clone(),
                    variant,
                    error.as_label(),
                )
                .increment(1);
            }
            if outcome.status == PrecompileCallStatus::Success && outcome.gas_used == 0 {
                BerylPrecompileMetrics::zero_gas_success_total(call.precompile, method, variant)
                    .increment(1);
            }
        }
        #[cfg(not(feature = "metrics"))]
        let _ = (call, outcome);
    }

    fn record_b20_created(&self, variant: &'static str) {
        #[cfg(feature = "metrics")]
        BerylPrecompileMetrics::b20_created_total(variant).increment(1);
        #[cfg(not(feature = "metrics"))]
        let _ = variant;
    }

    fn record_batch_items(&self, call: &PrecompileCallMetric, count: usize) {
        #[cfg(feature = "metrics")]
        BerylPrecompileMetrics::batch_items(
            call.precompile,
            call.method.clone().into_owned(),
            call.variant_label(),
        )
        .record(count as f64);
        #[cfg(not(feature = "metrics"))]
        let _ = (call, count);
    }

    fn record_internal_calls(&self, call: &PrecompileCallMetric, calls: usize, bytes: usize) {
        #[cfg(feature = "metrics")]
        {
            let method = call.method.clone().into_owned();
            let variant = call.variant_label();
            BerylPrecompileMetrics::internal_calls(call.precompile, method.clone(), variant)
                .record(calls as f64);
            BerylPrecompileMetrics::internal_call_bytes(call.precompile, method, variant)
                .record(bytes as f64);
        }
        #[cfg(not(feature = "metrics"))]
        let _ = (call, calls, bytes);
    }
}
