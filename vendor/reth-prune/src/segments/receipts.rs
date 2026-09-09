//! Common receipts pruning logic.
//!
//! - [`crate::segments::user::Receipts`] is responsible for pruning receipts according to the
//!   user-configured settings (for example, on a full node or with a custom prune config)

use base_common_types_chain::BaseReceipt;
use base_execution_state_types::{
    PruneCheckpoint, PruneSegment, SegmentOutput, SegmentOutputCheckpoint,
};
use reth_db_api::{tables, transaction::DbTxMut};
use reth_provider::{
    BlockReader, DBProvider, EitherWriter, PruneCheckpointWriter, StaticFileProviderFactory,
    StorageSettingsCache, TransactionsProvider, errors::provider::ProviderResult,
};
use reth_static_file_types::StaticFileSegment;
use tracing::{debug, trace};

use crate::{
    PrunerError,
    db_ext::DbTxPruneExt,
    segments::{self, PruneInput},
};

pub(crate) fn prune<Provider>(
    provider: &Provider,
    input: PruneInput,
) -> Result<SegmentOutput, PrunerError>
where
    Provider: DBProvider<Tx: DbTxMut>
        + TransactionsProvider
        + BlockReader
        + StorageSettingsCache
        + StaticFileProviderFactory,
{
    if EitherWriter::receipts_destination(provider).is_static_file() {
        debug!(target: "pruner", "Pruning receipts from static files.");
        return segments::prune_static_files(provider, input, StaticFileSegment::Receipts);
    }
    debug!(target: "pruner", "Pruning receipts from database.");

    // Original database implementation for when receipts are not on static files (old nodes)
    let tx_range = match input.get_next_tx_num_range(provider)? {
        Some(range) => range,
        None => {
            trace!(target: "pruner", "No receipts to prune");
            return Ok(SegmentOutput::done());
        }
    };
    let tx_range_end = *tx_range.end();

    let mut limiter = input.limiter;

    let mut last_pruned_transaction = tx_range_end;
    let (pruned, done) =
        provider.tx_ref().prune_table_with_range::<tables::Receipts<BaseReceipt>>(
            tx_range,
            &mut limiter,
            |_| false,
            |row| last_pruned_transaction = row.0,
        )?;
    trace!(target: "pruner", %pruned, %done, "Pruned receipts");

    let last_pruned_block = provider
        .block_by_transaction_id(last_pruned_transaction)?
        .ok_or(PrunerError::InconsistentData("Block for transaction is not found"))?
        // If there's more receipts to prune, set the checkpoint block number to previous,
        // so we could finish pruning its receipts on the next run.
        .checked_sub(if done { 0 } else { 1 });

    let progress = limiter.progress(done);

    Ok(SegmentOutput {
        progress,
        pruned,
        checkpoint: Some(SegmentOutputCheckpoint {
            block_number: last_pruned_block,
            tx_number: Some(last_pruned_transaction),
        }),
    })
}

pub(crate) fn save_checkpoint(
    provider: impl PruneCheckpointWriter,
    checkpoint: PruneCheckpoint,
) -> ProviderResult<()> {
    provider.save_prune_checkpoint(PruneSegment::Receipts, checkpoint)?;

    // `PruneSegment::Receipts` overrides `PruneSegment::ContractLogs`, so we can preemptively
    // limit their pruning start point.
    provider.save_prune_checkpoint(PruneSegment::ContractLogs, checkpoint)?;

    Ok(())
}
