//! Metrics emitted by the shadow indexer `ExEx`.

base_metrics::define_metrics! {
    shadow_indexer, struct = ShadowIndexerMetrics,

    #[describe("Total reorged-out shadow blocks emitted by the ExEx. Counted at write time so \
                that same-height reorgs collapsed by the number-only primary key on \
                shadow_blocks are still counted exactly.")]
    reorged_blocks_total: counter,
}

#[cfg(test)]
mod tests {
    #[test]
    fn reorged_counter_is_registered() {
        crate::ShadowIndexerMetrics::reorged_blocks_total().increment(1);
    }
}
