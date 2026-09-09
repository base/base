use std::ops::RangeInclusive;

use alloy_primitives::BlockNumber;
use base_common_types_chain::BaseReceipt;
use base_execution_state_database::{DbCursorRO, DbTx, tables};
use base_execution_state_types::StaticFileSegment;
use base_execution_state_types::{ProviderError, ProviderResult};
use base_execution_state_provider::{BlockReader, DBProvider, StaticFileProviderFactory};

use crate::segments::Segment;

/// Static File segment responsible for [`StaticFileSegment::Receipts`] part of data.
#[derive(Debug, Default)]
pub struct Receipts;

impl<Provider> Segment<Provider> for Receipts
where
    Provider: StaticFileProviderFactory + DBProvider + BlockReader,
{
    fn segment(&self) -> StaticFileSegment {
        StaticFileSegment::Receipts
    }

    fn copy_to_static_files(
        &self,
        provider: Provider,
        block_range: RangeInclusive<BlockNumber>,
    ) -> ProviderResult<()> {
        let mut static_file_writer =
            provider.get_static_file_writer(*block_range.start(), StaticFileSegment::Receipts)?;

        let mut receipts_cursor =
            provider.tx_ref().cursor_read::<tables::Receipts<BaseReceipt>>()?;

        for block in block_range {
            static_file_writer.increment_block(block)?;

            let block_body_indices = provider
                .block_body_indices(block)?
                .ok_or(ProviderError::BlockBodyIndicesNotFound(block))?;

            let receipts_walker = receipts_cursor.walk_range(block_body_indices.tx_num_range())?;

            static_file_writer.append_receipts(
                receipts_walker.map(|result| result.map_err(ProviderError::from)),
            )?;
        }

        Ok(())
    }
}
