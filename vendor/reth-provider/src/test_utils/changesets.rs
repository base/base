//! Static-file changeset fixtures for provider tests.

use std::collections::BTreeMap;

use reth_db_api::models::{AccountBeforeTx, StorageBeforeTx};
use reth_static_file_types::StaticFileSegment;
use reth_storage_errors::provider::ProviderResult;

use crate::{StaticFileWriter, providers::StaticFileProvider};

/// Changesets grouped by block, ready to write to static files.
#[derive(Debug, Default)]
pub struct TestChangesets {
    /// Account reverts by block.
    pub accounts: BTreeMap<u64, Vec<AccountBeforeTx>>,
    /// Storage reverts by block.
    pub storage: BTreeMap<u64, Vec<StorageBeforeTx>>,
}

impl TestChangesets {
    /// Writes the fixture, including empty blocks between changesets.
    pub fn write_to(mut self, provider: &StaticFileProvider) -> ProviderResult<()> {
        let last =
            self.accounts.keys().chain(self.storage.keys()).copied().max().unwrap_or_default();
        let mut accounts = provider.latest_writer(StaticFileSegment::AccountChangeSets)?;
        let mut storage = provider.latest_writer(StaticFileSegment::StorageChangeSets)?;
        for block in 0..=last {
            let mut account_entries = self.accounts.remove(&block).unwrap_or_default();
            account_entries.sort_unstable_by_key(|entry| entry.address);
            let mut storage_entries = self.storage.remove(&block).unwrap_or_default();
            storage_entries.sort_unstable_by_key(|entry| (entry.address, entry.key));
            accounts.append_account_changeset(account_entries, block)?;
            storage.append_storage_changeset(storage_entries, block)?;
        }
        accounts.commit()?;
        storage.commit()?;
        Ok(())
    }
}
