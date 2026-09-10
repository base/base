//! This module contains [`Context`] struct and implements [`ContextTr`] trait for it.
use derive_where::derive_where;

use crate::{
    Address, B256, Database, DatabaseRef, EmptyDB, Log, StorageKey, StorageValue, U256,
    WrapDatabaseRef,
    core_machine::{
        AccountInfoLoad, Block, BlockEnv, Cfg, CfgEnv, ContextError, ContextSetters, ContextTr,
        GasParams, Host, JournalTr, LoadError, LocalContext, SStoreResult, SelfDestructResult,
        StateLoad, Transaction, TransactionType, journal::Journal,
    },
    hints_util::cold_path,
};

/// EVM context contains data that EVM needs for execution.
#[derive_where(Clone, Debug; DB, <DB as Database>::Error)]
pub struct Context<DB: Database = EmptyDB> {
    /// Block information.
    pub block: BlockEnv,
    /// Transaction information.
    pub tx: crate::BaseTransaction,
    /// Configurations.
    pub cfg: CfgEnv<crate::BaseSpecId>,
    /// EVM State with journaling support and database.
    pub journaled_state: Journal<DB>,
    /// Inner context.
    pub chain: crate::L1BlockInfo,
    /// Local context that is filled by execution.
    pub local: LocalContext,
    /// Error that happened during execution.
    pub error: Result<(), ContextError<DB::Error>>,
}

#[inline]
fn sync_cfg_to_journal<DB: Database>(cfg: &CfgEnv<crate::BaseSpecId>, journal: &mut Journal<DB>) {
    journal.set_spec_id(cfg.spec.into());
    journal.set_eip7708_config(cfg.is_eip7708_disabled(), cfg.is_eip8246_delayed_clear_disabled());
}

impl<DB: Database> ContextTr for Context<DB> {
    type Tx = crate::BaseTransaction;
    type Cfg = CfgEnv<crate::BaseSpecId>;
    type Db = DB;
    type Chain = crate::L1BlockInfo;

    #[inline]
    fn all(
        &self,
    ) -> (
        &BlockEnv,
        &Self::Tx,
        &Self::Cfg,
        &Self::Db,
        &Journal<Self::Db>,
        &Self::Chain,
        &LocalContext,
    ) {
        let block = &self.block;
        let tx = &self.tx;
        let cfg = &self.cfg;
        let db = self.journaled_state.db();
        let journal = &self.journaled_state;
        let chain = &self.chain;
        let local = &self.local;

        (block, tx, cfg, db, journal, chain, local)
    }

    #[inline]
    fn all_mut(
        &mut self,
    ) -> (
        &BlockEnv,
        &Self::Tx,
        &Self::Cfg,
        &mut Journal<Self::Db>,
        &mut Self::Chain,
        &mut LocalContext,
    ) {
        let block = &self.block;
        let tx = &self.tx;
        let cfg = &self.cfg;
        let journal = &mut self.journaled_state;
        let chain = &mut self.chain;
        let local = &mut self.local;

        (block, tx, cfg, journal, chain, local)
    }

    #[inline]
    fn error(&mut self) -> &mut Result<(), ContextError<<Self::Db as Database>::Error>> {
        &mut self.error
    }
}

impl<DB: Database> ContextSetters for Context<DB> {
    fn set_tx(&mut self, tx: Self::Tx) {
        self.tx = tx;
    }

    fn set_block(&mut self, block: BlockEnv) {
        self.block = block;
    }
}

impl<DB: Database> Context<DB> {
    /// Creates a new context with a new database type.
    ///
    /// This will create a new [`Journal`] object.
    pub fn new(db: DB, spec: crate::BaseSpecId) -> Self {
        let cfg = CfgEnv::new_with_spec(spec);
        let mut journaled_state = Journal::new(db);
        sync_cfg_to_journal(&cfg, &mut journaled_state);
        Self {
            tx: crate::BaseTransaction::default(),
            block: BlockEnv::default(),
            cfg,
            local: LocalContext::default(),
            journaled_state,
            chain: Default::default(),
            error: Ok(()),
        }
    }
}

impl<DB> Context<DB>
where
    DB: Database,
{
    /// Creates a new context with a new database type.
    ///
    /// This will create a new [`Journal`] object.
    pub fn with_db<ODB: Database>(self, db: ODB) -> Context<ODB> {
        let mut journaled_state = Journal::new(db);
        sync_cfg_to_journal(&self.cfg, &mut journaled_state);
        Context {
            tx: self.tx,
            block: self.block,
            cfg: self.cfg,
            journaled_state,
            local: self.local,
            chain: self.chain,
            error: Ok(()),
        }
    }

    /// Creates a new context with a new `DatabaseRef` type.
    pub fn with_ref_db<ODB: DatabaseRef>(self, db: ODB) -> Context<WrapDatabaseRef<ODB>> {
        let mut journaled_state = Journal::new(WrapDatabaseRef(db));
        sync_cfg_to_journal(&self.cfg, &mut journaled_state);
        Context {
            tx: self.tx,
            block: self.block,
            cfg: self.cfg,
            journaled_state,
            local: self.local,
            chain: self.chain,
            error: Ok(()),
        }
    }

    /// Creates a new context with a new block type.
    pub fn with_block(self, block: BlockEnv) -> Context<DB> {
        Context {
            tx: self.tx,
            block,
            cfg: self.cfg,
            journaled_state: self.journaled_state,
            local: self.local,
            chain: self.chain,
            error: Ok(()),
        }
    }
    /// Creates a new context with a new transaction type.
    pub fn with_tx(self, tx: crate::BaseTransaction) -> Context<DB> {
        Context {
            tx,
            block: self.block,
            cfg: self.cfg,
            journaled_state: self.journaled_state,
            local: self.local,
            chain: self.chain,
            error: Ok(()),
        }
    }

    /// Creates a new context with a new chain type.
    pub fn with_chain(self, chain: crate::L1BlockInfo) -> Context<DB> {
        Context {
            tx: self.tx,
            block: self.block,
            cfg: self.cfg,
            journaled_state: self.journaled_state,
            local: self.local,
            chain,
            error: Ok(()),
        }
    }

    /// Creates a new context with a new chain type.
    pub fn with_cfg(mut self, cfg: CfgEnv<crate::BaseSpecId>) -> Context<DB> {
        sync_cfg_to_journal(&cfg, &mut self.journaled_state);
        Context {
            tx: self.tx,
            block: self.block,
            cfg,
            journaled_state: self.journaled_state,
            local: self.local,
            chain: self.chain,
            error: Ok(()),
        }
    }

    /// Modifies the context configuration.
    #[must_use]
    pub fn modify_cfg_chained<F>(mut self, f: F) -> Self
    where
        F: FnOnce(&mut CfgEnv<crate::BaseSpecId>),
    {
        f(&mut self.cfg);
        sync_cfg_to_journal(&self.cfg, &mut self.journaled_state);
        self
    }

    /// Modifies the context block.
    #[must_use]
    pub fn modify_block_chained<F>(mut self, f: F) -> Self
    where
        F: FnOnce(&mut BlockEnv),
    {
        self.modify_block(f);
        self
    }

    /// Modifies the context transaction.
    #[must_use]
    pub fn modify_tx_chained<F>(mut self, f: F) -> Self
    where
        F: FnOnce(&mut crate::BaseTransaction),
    {
        self.modify_tx(f);
        self
    }

    /// Modifies the context chain.
    #[must_use]
    pub fn modify_chain_chained<F>(mut self, f: F) -> Self
    where
        F: FnOnce(&mut crate::L1BlockInfo),
    {
        self.modify_chain(f);
        self
    }

    /// Modifies the context database.
    #[must_use]
    pub fn modify_db_chained<F>(mut self, f: F) -> Self
    where
        F: FnOnce(&mut DB),
    {
        self.modify_db(f);
        self
    }

    /// Modifies the context journal.
    #[must_use]
    pub fn modify_journal_chained<F>(mut self, f: F) -> Self
    where
        F: FnOnce(&mut Journal<DB>),
    {
        self.modify_journal(f);
        self
    }

    /// Modifies the context block.
    pub fn modify_block<F>(&mut self, f: F)
    where
        F: FnOnce(&mut BlockEnv),
    {
        f(&mut self.block);
    }

    /// Modifies the context transaction.
    pub fn modify_tx<F>(&mut self, f: F)
    where
        F: FnOnce(&mut crate::BaseTransaction),
    {
        f(&mut self.tx);
    }

    /// Modifies the context configuration.
    pub fn modify_cfg<F>(&mut self, f: F)
    where
        F: FnOnce(&mut CfgEnv<crate::BaseSpecId>),
    {
        f(&mut self.cfg);
        sync_cfg_to_journal(&self.cfg, &mut self.journaled_state);
    }

    /// Modifies the context chain.
    pub fn modify_chain<F>(&mut self, f: F)
    where
        F: FnOnce(&mut crate::L1BlockInfo),
    {
        f(&mut self.chain);
    }

    /// Modifies the context database.
    pub fn modify_db<F>(&mut self, f: F)
    where
        F: FnOnce(&mut DB),
    {
        f(self.journaled_state.db_mut());
    }

    /// Modifies the context journal.
    pub fn modify_journal<F>(&mut self, f: F)
    where
        F: FnOnce(&mut Journal<DB>),
    {
        f(&mut self.journaled_state);
    }

    /// Modifies the local context.
    pub fn modify_local<F>(&mut self, f: F)
    where
        F: FnOnce(&mut LocalContext),
    {
        f(&mut self.local);
    }
}

impl<DB: Database> Host for Context<DB> {
    /* Block */

    fn basefee(&self) -> U256 {
        U256::from(self.block().basefee())
    }

    fn blob_gasprice(&self) -> U256 {
        U256::from(self.block().blob_gasprice().unwrap_or(0))
    }

    fn gas_limit(&self) -> U256 {
        U256::from(self.block().gas_limit())
    }

    fn difficulty(&self) -> U256 {
        self.block().difficulty()
    }

    fn prevrandao(&self) -> Option<U256> {
        self.block().prevrandao().map(|r| r.into())
    }

    #[inline]
    fn gas_params(&self) -> &GasParams {
        self.cfg().gas_params()
    }

    fn is_amsterdam_eip8037_enabled(&self) -> bool {
        self.cfg().is_amsterdam_eip8037_enabled()
    }

    fn block_number(&self) -> U256 {
        self.block().number()
    }

    fn timestamp(&self) -> U256 {
        U256::from(self.block().timestamp())
    }

    fn beneficiary(&self) -> Address {
        self.block().beneficiary()
    }

    fn slot_num(&self) -> U256 {
        U256::from(self.block().slot_num())
    }

    fn chain_id(&self) -> U256 {
        U256::from(self.cfg().chain_id())
    }

    /* Transaction */

    fn effective_gas_price(&self) -> U256 {
        let basefee = self.block().basefee();
        U256::from(self.tx().effective_gas_price(basefee as u128))
    }

    fn caller(&self) -> Address {
        self.tx().caller()
    }

    fn blob_hash(&self, number: usize) -> Option<U256> {
        let tx = &self.tx();
        if tx.tx_type() != TransactionType::Eip4844 {
            return None;
        }
        tx.blob_versioned_hashes().get(number).map(|t| U256::from_be_bytes(t.0))
    }

    /* Config */

    fn max_initcode_size(&self) -> usize {
        self.cfg().max_initcode_size()
    }

    /* Database */

    fn block_hash(&mut self, requested_number: u64) -> Option<B256> {
        self.db_mut()
            .block_hash(requested_number)
            .map_err(|e| {
                cold_path();
                *self.error() = Err(e.into());
            })
            .ok()
    }

    /* Journal */

    /// Gets the transient storage value of `address` at `index`.
    fn tload(&mut self, address: Address, index: StorageKey) -> StorageValue {
        self.journal_mut().tload(address, index)
    }

    /// Sets the transient storage value of `address` at `index`.
    fn tstore(&mut self, address: Address, index: StorageKey, value: StorageValue) {
        self.journal_mut().tstore(address, index, value)
    }

    /// Emits a log owned by `address` with given `LogData`.
    fn log(&mut self, log: Log) {
        self.journal_mut().log(log);
    }

    /// Marks `address` to be deleted, with funds transferred to `target`.
    #[inline]
    fn selfdestruct(
        &mut self,
        address: Address,
        target: Address,
        skip_cold_load: bool,
    ) -> Result<StateLoad<SelfDestructResult>, LoadError> {
        self.journal_mut().selfdestruct(address, target, skip_cold_load).map_err(|e| {
            cold_path();
            let (ret, err) = e.into_parts();
            if let Some(err) = err {
                *self.error() = Err(err.into());
            }
            ret
        })
    }

    #[inline]
    fn sstore_skip_cold_load(
        &mut self,
        address: Address,
        key: StorageKey,
        value: StorageValue,
        skip_cold_load: bool,
    ) -> Result<StateLoad<SStoreResult>, LoadError> {
        self.journal_mut().sstore_skip_cold_load(address, key, value, skip_cold_load).map_err(|e| {
            cold_path();
            let (ret, err) = e.into_parts();
            if let Some(err) = err {
                *self.error() = Err(err.into());
            }
            ret
        })
    }

    #[inline]
    fn sload_skip_cold_load(
        &mut self,
        address: Address,
        key: StorageKey,
        skip_cold_load: bool,
    ) -> Result<StateLoad<StorageValue>, LoadError> {
        self.journal_mut().sload_skip_cold_load(address, key, skip_cold_load).map_err(|e| {
            cold_path();
            let (ret, err) = e.into_parts();
            if let Some(err) = err {
                *self.error() = Err(err.into());
            }
            ret
        })
    }

    #[inline]
    fn load_account_info_skip_cold_load(
        &mut self,
        address: Address,
        load_code: bool,
        skip_cold_load: bool,
    ) -> Result<AccountInfoLoad<'_>, LoadError> {
        match self.journaled_state.load_account_info_skip_cold_load(
            address,
            load_code,
            skip_cold_load,
        ) {
            Ok(a) => Ok(a),
            Err(e) => {
                cold_path();
                let (ret, err) = e.into_parts();
                if let Some(err) = err {
                    self.error = Err(err.into());
                }
                Err(ret)
            }
        }
    }
}
