use base_execution_state_types::{PruneMode, PrunePurpose, PruneSegment, SegmentOutput};
use reth_db_api::transaction::DbTxMut;
use reth_provider::{
    BlockReader, DBProvider, StaticFileProviderFactory, StorageSettingsCache, TransactionsProvider,
};
use reth_static_file_types::StaticFileSegment;
use tracing::{debug, instrument};

use crate::{
    PrunerError,
    segments::{self, PruneInput, Segment},
};

#[derive(Debug)]
pub struct SenderRecovery {
    mode: PruneMode,
}

impl SenderRecovery {
    pub const fn new(mode: PruneMode) -> Self {
        Self { mode }
    }
}

impl<Provider> Segment<Provider> for SenderRecovery
where
    Provider: DBProvider<Tx: DbTxMut>
        + TransactionsProvider
        + BlockReader
        + StorageSettingsCache
        + StaticFileProviderFactory,
{
    fn segment(&self) -> PruneSegment {
        PruneSegment::SenderRecovery
    }

    fn mode(&self) -> Option<PruneMode> {
        Some(self.mode)
    }

    fn purpose(&self) -> PrunePurpose {
        PrunePurpose::User
    }

    #[instrument(
        name = "SenderRecovery::prune",
        target = "pruner",
        skip(self, provider),
        ret(level = "trace")
    )]
    fn prune(&self, provider: &Provider, input: PruneInput) -> Result<SegmentOutput, PrunerError> {
        debug!(target: "pruner", "Pruning transaction senders from static files.");

        if self.mode.is_full() {
            debug!(target: "pruner", "PruneMode::Full: deleting all transaction senders static files.");
            return segments::delete_static_files_segment(
                provider,
                input,
                StaticFileSegment::TransactionSenders,
            );
        }

        return segments::prune_static_files(
            provider,
            input,
            StaticFileSegment::TransactionSenders,
        );
    }
}
