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

    #[describe("Total shadow candidate blocks inspected. Same-height reorgs collapse under the \
                number-only primary key on shadow_blocks, so a height reorged repeatedly between \
                two polls is inspected once: this counter is a lower bound whose value depends on \
                poll timing. Use shadow_indexer_reorged_blocks_total for the exact write-time \
                count.")]
    blocks_inspected_total: counter,

    #[describe("Total shadow candidate blocks with no non-deposit transactions")]
    empty_blocks_total: counter,

    #[describe("Total rows reorged out by pipeline unwind rather than shadow reconciliation")]
    reverted_blocks_total: counter,

    #[describe("Highest shadow block number inspected")]
    #[no_zero]
    latest_block_number: gauge,

    #[describe("Total polling iterations that failed")]
    poll_errors_total: counter,
}
