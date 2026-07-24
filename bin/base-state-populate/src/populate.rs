//! Database write logic for bulk ERC-20 balance-slot and storage trie population.

use alloy_primitives::{Address, B256, U256, keccak256};
use eyre::{Result, WrapErr};
use indicatif::{ProgressBar, ProgressStyle};
use rayon::prelude::*;
use reth_db::{ClientVersion, Database, mdbx::DatabaseArguments, open_db};
use reth_db_api::{
    cursor::{DbCursorRO, DbCursorRW, DbDupCursorRW},
    tables,
    transaction::{DbTx, DbTxMut},
};
use reth_primitives_traits::StorageEntry;
use reth_trie_db::{
    DatabaseHashedCursorFactory, DatabaseStorageRoot, DatabaseStorageTrieCursor,
    DatabaseTrieCursorFactory, LegacyKeyAdapter, PackedKeyAdapter, TrieTableAdapter,
};
use tracing::{info, warn};

use crate::{
    StorageTrieVersion,
    cli::PopulateArgs,
    storage::{address_for_index, derive_sender_addresses, erc20_balance_slot},
};

type AdapterStorageRoot<'a, TX, A> = reth_trie::StorageRoot<
    DatabaseTrieCursorFactory<&'a TX, A>,
    DatabaseHashedCursorFactory<&'a TX>,
>;

/// Entry point for the `populate` subcommand.
#[derive(Debug)]
pub struct Populator;

impl Populator {
    /// Runs the full populate pipeline against the datadir in `args`.
    pub fn run(args: PopulateArgs) -> Result<()> {
        let token_addr = args.contract;
        let hashed_token = keccak256(token_addr);
        info!(
            contract = %token_addr,
            hashed = %hashed_token,
            count = args.count,
            "starting state population"
        );

        let db = open_db(args.datadir.join("db"), DatabaseArguments::new(ClientVersion::default()))
            .wrap_err("open MDBX database")?;

        let storage_trie_version = {
            let tx = db.tx().wrap_err("begin storage-settings tx")?;
            StorageTrieVersion::detect(&tx).wrap_err("detect storage settings")?
        };
        info!(version = ?storage_trie_version, "detected storage trie version");

        if !args.trie_only {
            Self::clear_token_storage(&db, token_addr, hashed_token)
                .wrap_err("clear existing contract storage")?;
            Self::write_balance_slots(
                &db,
                token_addr,
                hashed_token,
                args.count,
                args.balance,
                args.balance_slot,
            )
            .wrap_err("write balance slots")?;
            if let (Some(seed), Some(sender_count)) = (args.seed, args.sender_count) {
                let sender_count = usize::try_from(sender_count)
                    .wrap_err("sender_count exceeds platform usize range")?;
                Self::write_sender_balances(
                    &db,
                    token_addr,
                    hashed_token,
                    args.balance,
                    args.balance_slot,
                    seed,
                    sender_count,
                )
                .wrap_err("write sender balance slots")?;
            }
        }

        Self::compute_and_write_storage_trie(&db, hashed_token, storage_trie_version)
            .wrap_err("compute + write storage trie")?;

        info!("population complete");
        Ok(())
    }

    fn clear_token_storage(
        db: &reth_db::DatabaseEnv,
        token_addr: Address,
        hashed_token: B256,
    ) -> Result<()> {
        let tx = db.tx_mut().wrap_err("begin clear-storage tx")?;
        {
            let mut ps_cursor = tx
                .cursor_dup_write::<tables::PlainStorageState>()
                .wrap_err("open PlainStorageState")?;
            if ps_cursor.seek_exact(token_addr).wrap_err("seek PlainStorageState")?.is_some() {
                ps_cursor
                    .delete_current_duplicates()
                    .wrap_err("delete PlainStorageState entries")?;
            }
        }
        {
            let mut hs_cursor =
                tx.cursor_dup_write::<tables::HashedStorages>().wrap_err("open HashedStorages")?;
            if hs_cursor.seek_exact(hashed_token).wrap_err("seek HashedStorages")?.is_some() {
                hs_cursor.delete_current_duplicates().wrap_err("delete HashedStorages entries")?;
            }
        }
        tx.commit().wrap_err("commit clear-storage tx")?;
        info!(contract = %token_addr, "cleared existing storage entries");
        Ok(())
    }

    /// Writes all `count` balance slots using a single-scan globally-sorted append.
    ///
    /// Generates all (`plain_slot`, `hashed_slot`) pairs in one rayon parallel pass,
    /// sorts each list independently, then writes each to the corresponding MDBX table
    /// in commit-sized chunks. Sequential sorted appends let MDBX extend leaf pages
    /// linearly without B-tree splits, avoiding exponential ZFS copy-on-write write
    /// amplification.
    ///
    /// Memory: ~(count × 32 bytes) per table. For 700M entries, ~22 GB per table (44 GB
    /// total). Requires sufficient RAM; the machine must have ~50+ GB available.
    fn write_balance_slots(
        db: &reth_db::DatabaseEnv,
        token_addr: Address,
        hashed_token: B256,
        count: u64,
        balance: U256,
        mapping_slot: B256,
    ) -> Result<()> {
        const COMMIT_CHUNK: usize = 10_000_000;

        info!(count, "generating and sorting plain slots");
        let mut plain_entries: Vec<B256> = (0..count)
            .into_par_iter()
            .map(|i| erc20_balance_slot(address_for_index(i), mapping_slot))
            .collect();
        plain_entries.par_sort_unstable();

        info!(count, "writing PlainStorageState");
        let pb = ProgressBar::new(count);
        pb.set_style(
            ProgressStyle::default_bar()
                .template(
                    "{spinner} [{elapsed_precise}] [{bar:40}] {pos}/{len} plain slots ({per_sec})",
                )?
                .progress_chars("##-"),
        );
        for chunk in plain_entries.chunks(COMMIT_CHUNK) {
            let tx = db.tx_mut().wrap_err("begin plain-slots tx")?;
            let mut cursor = tx
                .cursor_dup_write::<tables::PlainStorageState>()
                .wrap_err("open PlainStorageState")?;
            for slot in chunk {
                cursor
                    .append_dup(token_addr, StorageEntry { key: *slot, value: balance })
                    .wrap_err("append plain balance slot")?;
            }
            tx.commit().wrap_err("commit plain-slots chunk")?;
            pb.inc(chunk.len() as u64);
        }
        pb.finish_with_message("plain slots written");
        drop(plain_entries);

        info!(count, "generating and sorting hashed slots");
        let mut hashed_entries: Vec<B256> = (0..count)
            .into_par_iter()
            .map(|i| keccak256(erc20_balance_slot(address_for_index(i), mapping_slot)))
            .collect();
        hashed_entries.par_sort_unstable();

        info!(count, "writing HashedStorages");
        let pb2 = ProgressBar::new(count);
        pb2.set_style(
            ProgressStyle::default_bar()
                .template(
                    "{spinner} [{elapsed_precise}] [{bar:40}] {pos}/{len} hashed slots ({per_sec})",
                )?
                .progress_chars("##-"),
        );
        for chunk in hashed_entries.chunks(COMMIT_CHUNK) {
            let tx = db.tx_mut().wrap_err("begin hashed-slots tx")?;
            let mut cursor =
                tx.cursor_dup_write::<tables::HashedStorages>().wrap_err("open HashedStorages")?;
            for slot in chunk {
                cursor
                    .append_dup(hashed_token, StorageEntry { key: *slot, value: balance })
                    .wrap_err("append hashed balance slot")?;
            }
            tx.commit().wrap_err("commit hashed-slots chunk")?;
            pb2.inc(chunk.len() as u64);
        }
        pb2.finish_with_message("hashed slots written");
        drop(hashed_entries);

        Ok(())
    }

    fn compute_and_write_storage_trie(
        db: &reth_db::DatabaseEnv,
        hashed_token: B256,
        storage_trie_version: StorageTrieVersion,
    ) -> Result<()> {
        match storage_trie_version {
            StorageTrieVersion::V1 => Self::compute_and_write_storage_trie_with_adapter::<
                LegacyKeyAdapter,
            >(db, hashed_token),
            StorageTrieVersion::V2 => Self::compute_and_write_storage_trie_with_adapter::<
                PackedKeyAdapter,
            >(db, hashed_token),
        }
    }

    fn compute_and_write_storage_trie_with_adapter<A: TrieTableAdapter>(
        db: &reth_db::DatabaseEnv,
        hashed_token: B256,
    ) -> Result<()> {
        info!("computing storage trie (this may take a while for large state)");
        let tx = db.tx_mut().wrap_err("begin storage-trie tx")?;

        // Drop any stale StoragesTrie nodes for this contract first. Otherwise the
        // trie walker sees the cached root's hash flag, skips the subtree, and
        // returns the old root instead of rebuilding over the current leaves.
        {
            let mut trie_cursor =
                tx.cursor_dup_write::<A::StorageTrieTable>().wrap_err("open StoragesTrie")?;
            if trie_cursor.seek_exact(hashed_token).wrap_err("seek StoragesTrie")?.is_some() {
                trie_cursor
                    .delete_current_duplicates()
                    .wrap_err("delete stale StoragesTrie nodes")?;
            }
        }

        let (storage_root, _node_count, storage_trie_updates) =
            AdapterStorageRoot::<'_, _, A>::from_tx_hashed(&tx, hashed_token)
                .root_with_updates()
                .wrap_err("compute storage root")?;

        info!(root = %storage_root, "storage root computed");

        if storage_trie_updates.storage_nodes.is_empty() {
            warn!("no storage trie nodes returned — storage may be empty");
        } else {
            let sorted = storage_trie_updates.into_sorted();
            let cursor =
                tx.cursor_dup_write::<A::StorageTrieTable>().wrap_err("open StoragesTrie")?;
            let nodes_written = DatabaseStorageTrieCursor::<_, A>::new(cursor, hashed_token)
                .write_storage_trie_updates_sorted(&sorted)
                .wrap_err("write StoragesTrie nodes")?;
            info!(nodes = nodes_written, "storage trie nodes written");
        }

        tx.commit().wrap_err("commit storage-trie tx")?;
        Ok(())
    }

    fn write_sender_balances(
        db: &reth_db::DatabaseEnv,
        token_addr: Address,
        hashed_token: B256,
        balance: U256,
        mapping_slot: B256,
        seed: u64,
        sender_count: usize,
    ) -> Result<()> {
        let sender_addresses = derive_sender_addresses(seed, sender_count);
        info!(count = sender_count, "writing sender balance slots");

        let tx = db.tx_mut().wrap_err("begin sender-balances tx")?;
        let mut ps_cursor = tx
            .cursor_dup_write::<tables::PlainStorageState>()
            .wrap_err("open PlainStorageState")?;
        let mut hs_cursor =
            tx.cursor_dup_write::<tables::HashedStorages>().wrap_err("open HashedStorages")?;

        for addr in &sender_addresses {
            let plain_slot = erc20_balance_slot(*addr, mapping_slot);
            let hashed_slot = keccak256(plain_slot);
            ps_cursor
                .upsert(token_addr, &StorageEntry { key: plain_slot, value: balance })
                .wrap_err("upsert plain sender balance slot")?;
            hs_cursor
                .upsert(hashed_token, &StorageEntry { key: hashed_slot, value: balance })
                .wrap_err("upsert hashed sender balance slot")?;
        }

        tx.commit().wrap_err("commit sender-balances tx")?;
        info!(count = sender_count, "sender balance slots written");
        Ok(())
    }
}
