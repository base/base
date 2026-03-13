use base_execution_trie::MdbxProofsStorage;
use base_execution_trie::db::Tables;
use clap::Parser;
use comfy_table::{Cell, Row, Table as ComfyTable, presets::ASCII_MARKDOWN};
use human_bytes::human_bytes;
use reth_db::Database;

#[derive(Debug, Parser)]
pub(super) struct StatsCommand;

impl StatsCommand {
    pub(super) fn execute(self, storage: &MdbxProofsStorage) -> eyre::Result<()> {
        storage.db_ref().view(|tx| {
            let mut table = ComfyTable::new();
            table.load_preset(ASCII_MARKDOWN);
            table.set_header([
                "Table Name",
                "# Entries",
                "Branch Pages",
                "Leaf Pages",
                "Overflow Pages",
                "Total Size",
            ]);

            let mut total_size = 0usize;
            let mut total_entries = 0usize;

            let mut tables: Vec<_> = Tables::ALL.iter().map(Tables::name).collect();
            tables.sort();

            for table_name in tables {
                let table_db = tx
                    .inner()
                    .open_db(Some(table_name))
                    .map_err(|e| eyre::eyre!("could not open db: {e}"))?;
                let stats = tx
                    .inner()
                    .db_stat(table_db.dbi())
                    .map_err(|e| eyre::eyre!("could not get stats for {table_name}: {e}"))?;

                let page_size = stats.page_size() as usize;
                let leaf_pages = stats.leaf_pages();
                let branch_pages = stats.branch_pages();
                let overflow_pages = stats.overflow_pages();
                let num_pages = leaf_pages + branch_pages + overflow_pages;
                let table_size = page_size * num_pages;
                let entries = stats.entries();

                total_size += table_size;
                total_entries += entries;

                let mut row = Row::new();
                row.add_cell(Cell::new(table_name))
                    .add_cell(Cell::new(entries))
                    .add_cell(Cell::new(branch_pages))
                    .add_cell(Cell::new(leaf_pages))
                    .add_cell(Cell::new(overflow_pages))
                    .add_cell(Cell::new(human_bytes(table_size as f64)));
                table.add_row(row);
            }

            let mut separator = Row::new();
            for _ in 0..6 {
                separator.add_cell(Cell::new("---"));
            }
            table.add_row(separator);

            let mut total_row = Row::new();
            total_row
                .add_cell(Cell::new("Total"))
                .add_cell(Cell::new(total_entries))
                .add_cell(Cell::new(""))
                .add_cell(Cell::new(""))
                .add_cell(Cell::new(""))
                .add_cell(Cell::new(human_bytes(total_size as f64)));
            table.add_row(total_row);

            println!("{table}");

            let freelist = tx
                .inner()
                .env()
                .freelist()
                .map_err(|e| eyre::eyre!("could not get freelist: {e}"))?;
            let page_size = tx
                .inner()
                .open_db(None)
                .and_then(|db| tx.inner().db_stat(db.dbi()))
                .map(|stat| stat.page_size() as usize)
                .unwrap_or(4096);
            let freelist_size = freelist * page_size;
            println!("\nFreelist: {freelist} pages, {}", human_bytes(freelist_size as f64));

            Ok::<(), eyre::Report>(())
        })??;

        Ok(())
    }
}
