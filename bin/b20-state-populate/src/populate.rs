//! Database write logic for B20 token state population.

use crate::StorageTrieVersion;
use crate::cli::PopulateArgs;
use crate::storage::{
    B20_DECIMALS_SLOT, B20_INITIALIZED_SLOT, B20_MULTIPLIER_SLOT, B20_SUPPLY_CAP_SLOT,
    B20_TOTAL_SUPPLY_SLOT, EVM_TOKEN_ADDRESS, MOCK_B20_ASSET_BYTECODE, address_for_index,
    b20_balance_slot, derive_b20_asset_address, derive_sender_addresses,
};
use alloy_primitives::{Address, B256, Bytes, U256, keccak256};
use eyre::{Result, WrapErr};
use indicatif::{ProgressBar, ProgressStyle};
use rayon::prelude::*;
use reth_db::{ClientVersion, Database, mdbx::DatabaseArguments, open_db};
use reth_db_api::{
    cursor::{DbCursorRO, DbCursorRW, DbDupCursorRW},
    tables,
    transaction::{DbTx, DbTxMut},
};
use reth_primitives_traits::{Account, Bytecode, StorageEntry};
use reth_trie::{HashedPostState, StateRoot, StorageRoot};
use reth_trie_db::{
    DatabaseHashedCursorFactory, DatabaseStateRoot, DatabaseStorageRoot, DatabaseStorageTrieCursor,
    DatabaseTrieCursorFactory, LegacyKeyAdapter, PackedKeyAdapter, TrieTableAdapter,
};
use tracing::{info, warn};

type AdapterStorageRoot<'a, TX, A> =
    StorageRoot<DatabaseTrieCursorFactory<&'a TX, A>, DatabaseHashedCursorFactory<&'a TX>>;

type AdapterStateRoot<'a, TX, A> =
    StateRoot<DatabaseTrieCursorFactory<&'a TX, A>, DatabaseHashedCursorFactory<&'a TX>>;

/// Drives the B20 state population process.
#[derive(Debug)]
pub struct Populator;

impl Populator {
    /// Runs the full populate pipeline against the given datadir.
    pub fn run(args: PopulateArgs) -> Result<()> {
        let token_addr = derive_b20_asset_address(args.creator, args.salt);
        let hashed_token = keccak256(token_addr);
        info!(
            token = %token_addr,
            hashed = %hashed_token,
            count = args.count,
            "starting B20 state population"
        );

        let db = open_db(args.datadir.join("db"), DatabaseArguments::new(ClientVersion::default()))
            .wrap_err("open MDBX database")?;

        let storage_trie_version = {
            let tx = db.tx().wrap_err("begin storage-settings tx")?;
            StorageTrieVersion::detect(&tx).wrap_err("detect storage settings")?
        };

        let evm_count = args.evm_contract.then(|| args.evm_count.unwrap_or(args.count));
        let account_count = if args.populate_accounts {
            let n = args.account_count.or(evm_count).unwrap_or(args.count);
            Some(n)
        } else {
            None
        };

        if let Some(n) = account_count {
            Self::write_plain_accounts(&db, n, args.chunk_size, args.account_balance)
                .wrap_err("write plain accounts")?;
        }

        if !args.skip_precompile {
            if !args.trie_only {
                let total_supply = args.balance.saturating_mul(U256::from(args.count));
                Self::write_account(&db, token_addr, hashed_token, None)
                    .wrap_err("write precompile token account")?;
                Self::clear_token_storage(&db, token_addr, hashed_token)
                    .wrap_err("clear existing precompile token storage")?;
                Self::write_balance_slots(&db, token_addr, hashed_token, args.count, args.balance)
                    .wrap_err("write balance slots")?;
                Self::write_metadata_slots(&db, token_addr, hashed_token, total_supply, false)
                    .wrap_err("write precompile metadata slots")?;
            }
            Self::compute_and_write_storage_trie(&db, hashed_token, storage_trie_version)
                .wrap_err("compute + write storage trie")?;
            if account_count.is_none() {
                Self::update_account_trie(&db, hashed_token, None, storage_trie_version)
                    .wrap_err("update account trie")?;
            }
        } else {
            info!("skipping B20 precompile token (--skip-precompile set)");
        }

        if let Some(evm_n) = evm_count {
            info!(token = %EVM_TOKEN_ADDRESS, count = evm_n, "populating EVM contract token");
            let hashed_evm = keccak256(EVM_TOKEN_ADDRESS);
            let total_supply = args.balance.saturating_mul(U256::from(evm_n));

            let bytecode_hash = keccak256(MOCK_B20_ASSET_BYTECODE);
            if !args.trie_only {
                Self::write_bytecode(&db).wrap_err("write bytecode")?;
                Self::write_account(&db, EVM_TOKEN_ADDRESS, hashed_evm, Some(bytecode_hash))
                    .wrap_err("write EVM token account")?;
                Self::clear_token_storage(&db, EVM_TOKEN_ADDRESS, hashed_evm)
                    .wrap_err("clear existing EVM token storage")?;
                Self::write_balance_slots(&db, EVM_TOKEN_ADDRESS, hashed_evm, evm_n, args.balance)
                    .wrap_err("write EVM balance slots")?;
                if let (Some(seed), Some(sender_count)) = (args.seed, args.sender_count) {
                    Self::write_sender_balances(
                        &db,
                        EVM_TOKEN_ADDRESS,
                        hashed_evm,
                        args.balance,
                        seed,
                        sender_count as usize,
                    )
                    .wrap_err("write sender balance slots")?;
                }
                Self::write_metadata_slots(&db, EVM_TOKEN_ADDRESS, hashed_evm, total_supply, true)
                    .wrap_err("write EVM metadata slots")?;
            }
            Self::compute_and_write_storage_trie(&db, hashed_evm, storage_trie_version)
                .wrap_err("compute + write EVM storage trie")?;
            if account_count.is_none() {
                Self::update_account_trie(
                    &db,
                    hashed_evm,
                    Some(bytecode_hash),
                    storage_trie_version,
                )
                .wrap_err("update EVM account trie")?;
            }
        }

        if account_count.is_some() {
            Self::rebuild_hashed_accounts(&db).wrap_err("rebuild HashedAccounts")?;
            Self::recompute_account_trie(&db, storage_trie_version)
                .wrap_err("recompute full account trie")?;
        }

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
        info!(token = %token_addr, "cleared existing storage entries");
        Ok(())
    }

    fn write_account(
        db: &reth_db::DatabaseEnv,
        token_addr: Address,
        hashed_token: B256,
        bytecode_hash: Option<B256>,
    ) -> Result<()> {
        let account = Account { nonce: 0, balance: U256::ZERO, bytecode_hash };
        let tx = db.tx_mut().wrap_err("begin account tx")?;
        tx.cursor_write::<tables::PlainAccountState>()
            .wrap_err("open PlainAccountState")?
            .upsert(token_addr, &account)
            .wrap_err("upsert token account")?;
        tx.cursor_write::<tables::HashedAccounts>()
            .wrap_err("open HashedAccounts")?
            .upsert(hashed_token, &account)
            .wrap_err("upsert hashed token account")?;
        tx.commit().wrap_err("commit account tx")?;
        info!(token = %token_addr, "wrote token account");
        Ok(())
    }

    fn write_metadata_slots(
        db: &reth_db::DatabaseEnv,
        token_addr: Address,
        hashed_token: B256,
        total_supply: U256,
        is_evm_contract: bool,
    ) -> Result<()> {
        let tx = db.tx_mut().wrap_err("begin metadata tx")?;
        let mut ps_cursor = tx
            .cursor_dup_write::<tables::PlainStorageState>()
            .wrap_err("open PlainStorageState")?;
        let mut hs_cursor =
            tx.cursor_dup_write::<tables::HashedStorages>().wrap_err("open HashedStorages")?;

        let multiplier = U256::from(1u64) * U256::from(10u64).pow(U256::from(18u64));
        let mut meta: Vec<(B256, U256)> = vec![
            (B20_TOTAL_SUPPLY_SLOT, total_supply),
            (B20_SUPPLY_CAP_SLOT, total_supply),
            (B20_DECIMALS_SLOT, U256::from(18u8)),
            (B20_MULTIPLIER_SLOT, multiplier),
        ];
        if is_evm_contract {
            meta.push((B20_INITIALIZED_SLOT, U256::from(1u8)));
        }

        for (slot, value) in &meta {
            ps_cursor
                .upsert(token_addr, &StorageEntry { key: *slot, value: *value })
                .wrap_err("upsert plain meta slot")?;
            hs_cursor
                .upsert(hashed_token, &StorageEntry { key: keccak256(slot), value: *value })
                .wrap_err("upsert hashed meta slot")?;
        }

        tx.commit().wrap_err("commit metadata tx")?;
        info!(token = %token_addr, slots = meta.len(), "wrote metadata slots");
        Ok(())
    }

    /// Writes all `count` balance slots using a single-scan globally-sorted append.
    ///
    /// Generates all (plain_slot, hashed_slot) pairs in one rayon parallel pass,
    /// sorts each list independently, then writes each to the corresponding MDBX table
    /// in commit-sized chunks. Sequential sorted appends let MDBX extend leaf pages
    /// linearly without B-tree splits, avoiding exponential ZFS CoW write amplification.
    ///
    /// Memory: ~(count × 32 bytes) per table. For 700M entries, ~22 GB per table (44 GB
    /// total). Requires sufficient RAM; the machine must have ~50+ GB available.
    fn write_balance_slots(
        db: &reth_db::DatabaseEnv,
        token_addr: Address,
        hashed_token: B256,
        count: u64,
        balance: U256,
    ) -> Result<()> {
        const COMMIT_CHUNK: usize = 10_000_000;

        info!(count, "generating and sorting plain slots");
        let mut plain_entries: Vec<B256> =
            (0..count).into_par_iter().map(|i| b20_balance_slot(address_for_index(i))).collect();
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
            .map(|i| keccak256(b20_balance_slot(address_for_index(i))))
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

        // Drop any stale StoragesTrie nodes for this token first. Otherwise the
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

    fn update_account_trie(
        db: &reth_db::DatabaseEnv,
        hashed_token: B256,
        bytecode_hash: Option<B256>,
        storage_trie_version: StorageTrieVersion,
    ) -> Result<()> {
        match storage_trie_version {
            StorageTrieVersion::V1 => Self::update_account_trie_with_adapter::<LegacyKeyAdapter>(
                db,
                hashed_token,
                bytecode_hash,
            ),
            StorageTrieVersion::V2 => Self::update_account_trie_with_adapter::<PackedKeyAdapter>(
                db,
                hashed_token,
                bytecode_hash,
            ),
        }
    }

    fn update_account_trie_with_adapter<A: TrieTableAdapter>(
        db: &reth_db::DatabaseEnv,
        hashed_token: B256,
        bytecode_hash: Option<B256>,
    ) -> Result<()> {
        info!("updating account trie");
        let tx = db.tx_mut().wrap_err("begin account-trie tx")?;

        let account = Account { nonce: 0, balance: U256::ZERO, bytecode_hash };
        let post_state = HashedPostState::default().with_accounts([(hashed_token, Some(account))]);
        let sorted_post_state = post_state.into_sorted();

        let (new_state_root, acct_trie_updates) =
            AdapterStateRoot::<'_, _, A>::overlay_root_with_updates(&tx, &sorted_post_state)
                .wrap_err("compute account trie overlay")?;

        info!(state_root = %new_state_root, "new state root computed");

        let sorted = acct_trie_updates.into_sorted();
        let mut acct_cursor =
            tx.cursor_write::<A::AccountTrieTable>().wrap_err("open AccountsTrie")?;

        let mut written = 0usize;
        let mut deleted = 0usize;
        for (key, maybe_node) in sorted.account_nodes_ref() {
            if key.is_empty() {
                continue;
            }
            let nibbles = A::AccountKey::from(*key);
            match maybe_node {
                Some(node) => {
                    acct_cursor.upsert(nibbles, node).wrap_err("upsert AccountsTrie node")?;
                    written += 1;
                }
                None => {
                    if acct_cursor
                        .seek_exact(nibbles.clone())
                        .wrap_err("seek AccountsTrie")?
                        .is_some()
                    {
                        acct_cursor.delete_current().wrap_err("delete AccountsTrie node")?;
                        deleted += 1;
                    }
                }
            }
        }

        info!(written, deleted, "account trie nodes updated");
        tx.commit().wrap_err("commit account-trie tx")?;
        Ok(())
    }

    fn write_bytecode(db: &reth_db::DatabaseEnv) -> Result<B256> {
        let bytecode_hash = keccak256(MOCK_B20_ASSET_BYTECODE);
        let bytecode = Bytecode::new_raw(Bytes::from_static(MOCK_B20_ASSET_BYTECODE));
        let tx = db.tx_mut().wrap_err("begin bytecode tx")?;
        tx.put::<tables::Bytecodes>(bytecode_hash, bytecode).wrap_err("write bytecode")?;
        tx.commit().wrap_err("commit bytecode tx")?;
        info!(hash = %bytecode_hash, "wrote MockB20Asset bytecode");
        Ok(bytecode_hash)
    }

    fn write_sender_balances(
        db: &reth_db::DatabaseEnv,
        token_addr: Address,
        hashed_token: B256,
        balance: U256,
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
            let plain_slot = b20_balance_slot(*addr);
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

    /// Phase 1 of account writing: write synthetic EOA accounts to `PlainAccountState` only.
    ///
    /// Writing `HashedAccounts` here would require a sorted scan of all 469K existing leaf
    /// pages per chunk (60 GB of ZFS CoW I/O, repeated 700 times = ~93 hours). Instead,
    /// `rebuild_hashed_accounts` does a single 16-pass scan of `PlainAccountState` to
    /// build `HashedAccounts` in ~9 minutes.
    fn write_plain_accounts(
        db: &reth_db::DatabaseEnv,
        count: u64,
        chunk_size: u64,
        balance: U256,
    ) -> Result<()> {
        info!(count, "writing synthetic EOA accounts to PlainAccountState");
        let total_chunks = count.div_ceil(chunk_size);
        let pb = ProgressBar::new(count);
        pb.set_style(
            ProgressStyle::default_bar()
                .template(
                    "{spinner} [{elapsed_precise}] [{bar:40}] {pos}/{len} accounts ({per_sec})",
                )?
                .progress_chars("##-"),
        );

        let account = Account { nonce: 0, balance, bytecode_hash: None };

        for chunk_idx in 0..total_chunks {
            let start = chunk_idx * chunk_size;
            let end = ((chunk_idx + 1) * chunk_size).min(count);

            let tx = db.tx_mut().wrap_err("begin plain-accounts tx")?;
            let mut plain_cursor = tx
                .cursor_write::<tables::PlainAccountState>()
                .wrap_err("open PlainAccountState")?;
            for i in start..end {
                plain_cursor
                    .upsert(address_for_index(i), &account)
                    .wrap_err("upsert plain account")?;
            }
            tx.commit().wrap_err("commit plain-accounts tx")?;

            pb.inc(end - start);
            info!(
                chunk = chunk_idx + 1,
                total = total_chunks,
                accounts_written = end,
                "plain account chunk committed"
            );
        }

        pb.finish_with_message("plain accounts written");
        Ok(())
    }

    /// Phase 2 of account writing: rebuild `HashedAccounts` by scanning `PlainAccountState`
    /// in 16 passes, one hash-space slice per pass.
    ///
    /// Each pass only touches 1/16 of the existing mainnet pages (3.75 GB of CoW I/O), and
    /// distributes the 60 GB total over 16 passes instead of repeating it 700 times.
    /// Estimated total time: ~9 minutes vs ~93 hours for per-chunk writes.
    fn rebuild_hashed_accounts(db: &reth_db::DatabaseEnv) -> Result<()> {
        const NUM_PASSES: u16 = 16;
        const BUCKETS_PER_PASS: u16 = 256 / NUM_PASSES;

        info!(passes = NUM_PASSES, "rebuilding HashedAccounts from PlainAccountState");

        for pass in 0u16..NUM_PASSES {
            let hash_lo = pass * BUCKETS_PER_PASS;
            let hash_hi = (pass + 1) * BUCKETS_PER_PASS;

            let read_tx = db.tx().wrap_err("begin PlainAccountState scan tx")?;
            let mut entries: Vec<(B256, Account)> = Vec::new();
            {
                let mut cursor = read_tx
                    .cursor_read::<tables::PlainAccountState>()
                    .wrap_err("open PlainAccountState cursor")?;
                let mut walker = cursor.walk(None).wrap_err("walk PlainAccountState")?;
                while let Some((addr, account)) = walker.next().transpose()? {
                    let hash = keccak256(addr);
                    let b = hash[0] as u16;
                    if b >= hash_lo && b < hash_hi {
                        entries.push((hash, account));
                    }
                }
            }
            drop(read_tx);

            entries.par_sort_unstable_by_key(|(h, _)| *h);

            info!(
                pass = pass + 1,
                total = NUM_PASSES,
                entries = entries.len(),
                "inserting HashedAccounts slice"
            );

            let write_tx = db.tx_mut().wrap_err("begin HashedAccounts write tx")?;
            let mut hashed_cursor = write_tx
                .cursor_write::<tables::HashedAccounts>()
                .wrap_err("open HashedAccounts cursor")?;
            for (hash, account) in &entries {
                hashed_cursor.upsert(*hash, account).wrap_err("upsert hashed account")?;
            }
            write_tx.commit().wrap_err("commit HashedAccounts pass tx")?;

            info!(
                pass = pass + 1,
                total = NUM_PASSES,
                entries = entries.len(),
                "HashedAccounts pass committed"
            );
        }

        info!("HashedAccounts rebuild complete");
        Ok(())
    }

    fn recompute_account_trie(
        db: &reth_db::DatabaseEnv,
        storage_trie_version: StorageTrieVersion,
    ) -> Result<()> {
        match storage_trie_version {
            StorageTrieVersion::V1 => {
                Self::recompute_account_trie_with_adapter::<LegacyKeyAdapter>(db)
            }
            StorageTrieVersion::V2 => {
                Self::recompute_account_trie_with_adapter::<PackedKeyAdapter>(db)
            }
        }
    }

    fn recompute_account_trie_with_adapter<A: TrieTableAdapter>(
        db: &reth_db::DatabaseEnv,
    ) -> Result<()> {
        info!("recomputing full account trie from HashedAccounts (this may take a while)");
        let tx = db.tx_mut().wrap_err("begin account-trie recompute tx")?;

        let (new_state_root, trie_updates) = AdapterStateRoot::<'_, _, A>::from_tx(&tx)
            .root_with_updates()
            .wrap_err("compute full state root")?;

        info!(state_root = %new_state_root, "full state root computed");

        let sorted = trie_updates.into_sorted();
        let mut acct_cursor =
            tx.cursor_write::<A::AccountTrieTable>().wrap_err("open AccountsTrie")?;

        let mut written = 0usize;
        let mut deleted = 0usize;
        for (key, maybe_node) in sorted.account_nodes_ref() {
            if key.is_empty() {
                continue;
            }
            let nibbles = A::AccountKey::from(*key);
            match maybe_node {
                Some(node) => {
                    acct_cursor.upsert(nibbles, node).wrap_err("upsert AccountsTrie node")?;
                    written += 1;
                }
                None => {
                    if acct_cursor
                        .seek_exact(nibbles.clone())
                        .wrap_err("seek AccountsTrie")?
                        .is_some()
                    {
                        acct_cursor.delete_current().wrap_err("delete AccountsTrie node")?;
                        deleted += 1;
                    }
                }
            }
        }

        info!(written, deleted, "account trie fully recomputed");
        tx.commit().wrap_err("commit account-trie recompute tx")?;
        Ok(())
    }
}
