use base_execution_state_types::{StaticFileMap, StaticFileSegment};

/// Static files configuration.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(default)]
pub struct StaticFilesConfig {
    /// Number of blocks per file for each segment.
    pub blocks_per_file: BlocksPerFileConfig,
}

/// Configuration for the number of blocks per file for each segment.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(default)]
pub struct BlocksPerFileConfig {
    /// Number of blocks per file for the headers segment.
    pub headers: Option<u64>,
    /// Number of blocks per file for the transactions segment.
    pub transactions: Option<u64>,
    /// Number of blocks per file for the receipts segment.
    pub receipts: Option<u64>,
    /// Number of blocks per file for the transaction senders segment.
    pub transaction_senders: Option<u64>,
    /// Number of blocks per file for the account changesets segment.
    pub account_change_sets: Option<u64>,
    /// Number of blocks per file for the storage changesets segment.
    pub storage_change_sets: Option<u64>,
}

impl StaticFilesConfig {
    /// Validates the static files configuration.
    ///
    /// Returns an error if any blocks per file value is zero.
    pub fn validate(&self) -> eyre::Result<()> {
        eyre::ensure!(
            self.blocks_per_file.headers != Some(0),
            "Headers segment blocks per file must be greater than 0"
        );
        eyre::ensure!(
            self.blocks_per_file.transactions != Some(0),
            "Transactions segment blocks per file must be greater than 0"
        );
        eyre::ensure!(
            self.blocks_per_file.receipts != Some(0),
            "Receipts segment blocks per file must be greater than 0"
        );
        eyre::ensure!(
            self.blocks_per_file.transaction_senders != Some(0),
            "Transaction senders segment blocks per file must be greater than 0"
        );
        eyre::ensure!(
            self.blocks_per_file.account_change_sets != Some(0),
            "Account changesets segment blocks per file must be greater than 0"
        );
        eyre::ensure!(
            self.blocks_per_file.storage_change_sets != Some(0),
            "Storage changesets segment blocks per file must be greater than 0"
        );
        Ok(())
    }

    /// Converts the blocks per file configuration into a [`StaticFileMap`].
    pub fn as_blocks_per_file_map(&self) -> StaticFileMap<u64> {
        let mut map = StaticFileMap::default();
        // Iterating over all possible segments allows us to do an exhaustive match here,
        // to not forget to configure new segments in the future.
        for segment in StaticFileSegment::iter() {
            let blocks_per_file = match segment {
                StaticFileSegment::Headers => self.blocks_per_file.headers,
                StaticFileSegment::Transactions => self.blocks_per_file.transactions,
                StaticFileSegment::Receipts => self.blocks_per_file.receipts,
                StaticFileSegment::TransactionSenders => self.blocks_per_file.transaction_senders,
                StaticFileSegment::AccountChangeSets => self.blocks_per_file.account_change_sets,
                StaticFileSegment::StorageChangeSets => self.blocks_per_file.storage_change_sets,
            };

            if let Some(blocks_per_file) = blocks_per_file {
                map.insert(segment, blocks_per_file);
            }
        }
        map
    }
}
