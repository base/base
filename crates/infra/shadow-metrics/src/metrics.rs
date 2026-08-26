//! Metrics emitted by shadow block reader.

base_metrics::define_metrics! {
    shadow_metrics, struct = ShadowMetrics,

    #[describe("Total gas used in a shadow candidate block, including deposit transactions")]
    #[label(builder_version)]
    gas_used: histogram,

    #[describe("Number of transactions in a shadow candidate block, including deposit transactions")]
    #[label(builder_version)]
    transaction_count: histogram,

    #[describe("Priority-fee ordering inversions per shadow candidate block over non-deposit \
                transactions. Baseline equals flashblocks per block (10 in production at \
                2000ms block time / 200ms flashblock interval) because the builder refreshes \
                its transaction iterator once per flashblock. Values above that baseline \
                indicate ordering broke within a flashblock.")]
    #[label(builder_version)]
    priority_fee_inversions: histogram,

    #[describe("Total shadow candidate blocks inspected")]
    blocks_inspected_total: counter,

    #[describe("Total shadow candidate blocks with no non-deposit transactions")]
    empty_blocks_total: counter,

    #[describe("Highest shadow block number inspected")]
    #[no_zero]
    latest_block_number: gauge,

    #[describe("Shadow candidate blocks stored without a canonical replacement hash. These are \
                never emitted as metrics, so a backlog that does not drain marks rows the \
                indexer will not revisit.")]
    unresolved_blocks: gauge,

    #[describe("Age in seconds of the oldest shadow candidate block still lacking a canonical \
                replacement hash. Zero when none are outstanding.")]
    unresolved_oldest_age_seconds: gauge,

    #[describe("Total polling iterations that failed")]
    poll_errors_total: counter,
}
