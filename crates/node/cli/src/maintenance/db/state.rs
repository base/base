use std::{
    collections::BTreeSet,
    time::{Duration, Instant},
};

use alloy_primitives::{Address, B256, BlockNumber, U256, keccak256};
use base_execution_state_api::BlockNumReader;
use base_execution_state_database::{Database, DbDupCursorRO, DbTx, tables};
use base_execution_state_maintenance::DbTool;
use base_execution_state_provider::StaticFileProviderFactory;
use clap::Parser;
use tracing::info;

/// Log progress every 30 seconds
const LOG_INTERVAL: Duration = Duration::from_secs(30);

/// The arguments for the `reth db state` command
#[derive(Parser, Debug)]
pub struct Command {
    /// The account address to get state for
    address: Address,

    /// Block number to query state at (uses current state if not provided)
    #[arg(long, short)]
    block: Option<BlockNumber>,

    /// Maximum number of storage slots to display
    #[arg(long, short, default_value = "100")]
    limit: usize,

    /// Output format (table, json, csv)
    #[arg(long, short, default_value = "table")]
    format: OutputFormat,
}

impl Command {
    /// Execute `db state` command
    pub fn execute(self, tool: &DbTool) -> eyre::Result<()> {
        let address = self.address;
        let limit = self.limit;

        if let Some(block) = self.block {
            self.execute_historical(tool, address, block, limit)
        } else {
            self.execute_current(tool, address, limit)
        }
    }

    fn execute_current(&self, tool: &DbTool, address: Address, limit: usize) -> eyre::Result<()> {
        let entries = tool.provider_factory.db_ref().view(|tx| {
            let (account, walker_entries) = {
                let hashed_address = keccak256(address);
                let account = tx.get::<tables::HashedAccounts>(hashed_address)?;
                let mut cursor = tx.cursor_dup_read::<tables::HashedStorages>()?;
                let walker = cursor.walk_dup(Some(hashed_address), None)?;
                let mut entries = Vec::new();
                let mut last_log = Instant::now();
                for (idx, entry) in walker.enumerate() {
                    let (_, storage_entry) = entry?;
                    if storage_entry.value != U256::ZERO {
                        entries.push((storage_entry.key, storage_entry.value));
                    }
                    if entries.len() >= limit {
                        break;
                    }
                    if last_log.elapsed() >= LOG_INTERVAL {
                        info!(
                            target: "reth::cli",
                            address = %address,
                            slots_scanned = idx,
                            "Scanning storage slots"
                        );
                        last_log = Instant::now();
                    }
                }
                (account, entries)
            };

            Ok::<_, eyre::Report>((account, walker_entries))
        })??;

        let (account, storage_entries) = entries;

        self.print_results(address, None, account, &storage_entries);

        Ok(())
    }

    fn execute_historical(
        &self,
        tool: &DbTool,
        address: Address,
        block: BlockNumber,
        limit: usize,
    ) -> eyre::Result<()> {
        let provider = tool.provider_factory.history_by_block_number(block)?;

        // Get account info at that block
        let account = provider.basic_account(&address)?;

        // Check storage settings to determine where history is stored

        // For historical queries, enumerate keys from history indices only
        // (not PlainStorageState, which reflects current state)
        let mut storage_keys = BTreeSet::new();

        self.collect_staticfile_storage_keys(tool, address, &mut storage_keys)?;

        info!(
            target: "reth::cli",
            address = %address,
            block = block,
            total_keys = storage_keys.len(),
            "Found storage keys to query"
        );

        // Now query each key at the historical block using the StateProvider
        // This handles both MDBX and RocksDB backends transparently
        let mut entries = Vec::new();
        let mut last_log = Instant::now();

        for (idx, key) in storage_keys.iter().enumerate() {
            match provider.storage(address, *key) {
                Ok(Some(value)) if value != U256::ZERO => {
                    entries.push((*key, value));
                }
                _ => {}
            }

            if entries.len() >= limit {
                break;
            }

            if last_log.elapsed() >= LOG_INTERVAL {
                info!(
                    target: "reth::cli",
                    address = %address,
                    block = block,
                    keys_total = storage_keys.len(),
                    slots_scanned = idx,
                    slots_found = entries.len(),
                    "Scanning historical storage slots"
                );
                last_log = Instant::now();
            }
        }

        self.print_results(address, Some(block), account, &entries);

        Ok(())
    }

    /// Collects storage keys from static file StorageChangeSets (storage_v2).
    fn collect_staticfile_storage_keys(
        &self,
        tool: &DbTool,
        address: Address,
        keys: &mut BTreeSet<B256>,
    ) -> eyre::Result<()> {
        let tip = tool.provider_factory.provider()?.best_block_number()?;

        if tip == 0 {
            return Ok(());
        }

        info!(
            target: "reth::cli",
            address = %address,
            tip,
            "Scanning static file storage changesets"
        );

        let static_file_provider = tool.provider_factory.static_file_provider();
        let walker = static_file_provider.walk_storage_changeset_range(0..=tip);

        let mut total_scanned = 0usize;
        let mut last_log = Instant::now();

        for changeset_result in walker {
            let (block_addr, storage_entry) = changeset_result?;
            total_scanned += 1;

            if block_addr.address() == address {
                keys.insert(storage_entry.key);
            }

            if last_log.elapsed() >= LOG_INTERVAL {
                info!(
                    target: "reth::cli",
                    address = %address,
                    entries_scanned = total_scanned,
                    unique_keys = keys.len(),
                    "Scanning static file storage changesets"
                );
                last_log = Instant::now();
            }
        }

        info!(
            target: "reth::cli",
            address = %address,
            total_entries = total_scanned,
            unique_keys = keys.len(),
            "Finished static file storage changeset scan"
        );

        Ok(())
    }

    fn print_results(
        &self,
        address: Address,
        block: Option<BlockNumber>,
        account: Option<base_execution_state_memory::StoredAccount>,
        storage: &[(alloy_primitives::B256, U256)],
    ) {
        match self.format {
            OutputFormat::Table => {
                println!("Account: {address}");
                if let Some(b) = block {
                    println!("Block: {b}");
                } else {
                    println!("Block: latest");
                }
                println!();

                if let Some(acc) = account {
                    println!("Nonce: {}", acc.nonce);
                    println!("Balance: {} wei", acc.balance);
                    if let Some(code_hash) = acc.bytecode_hash {
                        println!("Code hash: {code_hash}");
                    }
                } else {
                    println!("Account not found");
                }

                println!();
                println!("Storage ({} slots):", storage.len());
                println!("{:-<130}", "");
                println!("{:<66} | {:<64}", "Slot", "Value");
                println!("{:-<130}", "");
                for (key, value) in storage {
                    println!("{key} | {value:#066x}");
                }
            }
            OutputFormat::Json => {
                let output = serde_json::json!({
                    "address": address.to_string(),
                    "block": block,
                    "account": account.map(|a| serde_json::json!({
                        "nonce": a.nonce,
                        "balance": a.balance.to_string(),
                        "code_hash": a.bytecode_hash.map(|h| h.to_string()),
                    })),
                    "storage": storage.iter().map(|(k, v)| {
                        serde_json::json!({
                            "key": k.to_string(),
                            "value": format!("{v:#066x}"),
                        })
                    }).collect::<Vec<_>>(),
                });
                println!("{}", serde_json::to_string_pretty(&output).unwrap());
            }
            OutputFormat::Csv => {
                println!("slot,value");
                for (key, value) in storage {
                    println!("{key},{value:#066x}");
                }
            }
        }
    }
}

#[derive(Debug, Clone, Default, clap::ValueEnum)]
pub enum OutputFormat {
    #[default]
    Table,
    Json,
    Csv,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_state_args() {
        let cmd = Command::try_parse_from([
            "state",
            "0xd8dA6BF26964aF9D7eEd9e03E53415D37aA96045",
            "--block",
            "1000000",
        ])
        .unwrap();
        assert_eq!(
            cmd.address,
            "0xd8dA6BF26964aF9D7eEd9e03E53415D37aA96045".parse::<Address>().unwrap()
        );
        assert_eq!(cmd.block, Some(1000000));
    }

    #[test]
    fn parse_state_args_no_block() {
        let cmd = Command::try_parse_from(["state", "0xd8dA6BF26964aF9D7eEd9e03E53415D37aA96045"])
            .unwrap();
        assert_eq!(cmd.block, None);
    }
}
