//! Metrics emitted by the shadow indexer `ExEx`.

base_metrics::define_metrics! {
    shadow_indexer, struct = ShadowIndexerMetrics,

    #[describe("Total reorged-out shadow blocks emitted by the ExEx. Counted at write time, so it \
                moves even when the metrics reader is down or lagging.")]
    reorged_blocks_total: counter,
}

#[cfg(test)]
mod tests {
    #[test]
    fn reorged_counter_is_registered() {
        crate::ShadowIndexerMetrics::reorged_blocks_total().increment(1);
    }
}
