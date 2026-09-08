use std::time::{Duration, Instant};

use alloy_primitives::{Address, keccak256};
use base_common_consensus::Compact;
use clap::Parser;
use human_bytes::human_bytes;
use reth_db_api::{
    cursor::DbDupCursorRO, database::Database, database_metrics::DatabaseMetrics, tables,
    transaction::DbTx,
};
use reth_db_common::DbTool;
use tracing::info;

/// Log progress every 5 seconds
const LOG_INTERVAL: Duration = Duration::from_secs(5);

/// The arguments for the `reth db account-storage` command
#[derive(Parser, Debug)]
pub struct Command {
    /// The account address to check storage for
    address: Address,
}

impl Command {
    /// Execute `db account-storage` command
    pub fn execute<DB: Database + DatabaseMetrics + Clone + Unpin + 'static>(
        self,
        tool: &DbTool<DB>,
    ) -> eyre::Result<()> {
        let address = self.address;

        let (slot_count, storage_size) = {
            let hashed_address = keccak256(address);
            tool.provider_factory.db_ref().view(|tx| {
                let mut cursor = tx.cursor_dup_read::<tables::HashedStorages>()?;
                let mut count = 0usize;
                let mut total_value_bytes = 0usize;
                let mut last_log = Instant::now();

                let walker = cursor.walk_dup(Some(hashed_address), None)?;
                for entry in walker {
                    let (_, storage_entry) = entry?;
                    count += 1;
                    let mut buf = Vec::new();
                    let entry_len = storage_entry.to_compact(&mut buf);
                    total_value_bytes += entry_len;

                    if last_log.elapsed() >= LOG_INTERVAL {
                        info!(
                            target: "reth::cli",
                            address = %address,
                            slots = count,
                            key = %storage_entry.key,
                            "Processing hashed storage slots"
                        );
                        last_log = Instant::now();
                    }
                }

                let total_size = if count > 0 { 32 + total_value_bytes } else { 0 };

                Ok::<_, eyre::Report>((count, total_size))
            })??
        };

        let hashed_address = keccak256(address);

        println!("Account: {address}");
        println!("Hashed address: {hashed_address}");
        println!("Storage slots: {slot_count}");

        println!("Hashed storage size: {} (estimated)", human_bytes(storage_size as f64));

        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_address_arg() {
        let cmd = Command::try_parse_from([
            "account-storage",
            "0xd8dA6BF26964aF9D7eEd9e03E53415D37aA96045",
        ])
        .unwrap();
        assert_eq!(
            cmd.address,
            "0xd8dA6BF26964aF9D7eEd9e03E53415D37aA96045".parse::<Address>().unwrap()
        );
    }
}
