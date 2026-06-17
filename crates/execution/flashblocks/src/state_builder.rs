use std::{
    collections::HashMap,
    sync::{Arc, Mutex, OnceLock},
    time::Instant,
};

use alloy_consensus::{
    Block, Header, TxReceipt,
    transaction::{Recovered, TransactionMeta},
};
use alloy_eips::Encodable2718;
use alloy_evm::{
    Database as AlloyDatabase,
    block::{StateDB, SystemCaller},
};
use alloy_primitives::{Address, B256, U256};
use alloy_rpc_types::TransactionTrait;
use alloy_rpc_types_eth::state::StateOverride;
use base_common_chains::Upgrades;
use base_common_consensus::{BasePrimitives, BaseReceipt, BaseTxEnvelope, Predeploys};
use base_common_evm::{BaseHaltReason, L1BlockInfo, ensure_create2_deployer};
use base_common_flz::tx_estimated_size_fjord as estimate_tx_compressed_size;
use base_common_rpc_types::{BaseTransactionReceipt, Transaction};
use base_execution_rpc::BaseReceiptBuilder as BaseRpcReceiptBuilder;
use reth_evm::{Evm, FromRecoveredTx};
use reth_rpc_convert::transaction::ConvertReceiptInput;
use revm::{
    Database, DatabaseCommit,
    context::{
        Block as _,
        result::{ExecutionResult, ResultAndState},
    },
    state::EvmState,
};

use crate::{
    CapturedReads, ExecutionError, PendingBlocks, ReadLog, StateProcessorError,
    UnifiedReceiptBuilder,
};

/// Represents the result of executing or fetching a cached pending transaction.
#[derive(Debug, Clone)]
pub struct ExecutedPendingTransaction {
    /// The RPC transaction.
    pub rpc_transaction: Transaction,
    /// The receipt of the transaction.
    pub receipt: BaseTransactionReceipt,
    /// The updated EVM state.
    pub state: EvmState,
    /// The execution result of the transaction.
    pub result: ExecutionResult<BaseHaltReason>,
    /// Per-transaction EVM execution time, if known.
    pub execution_time_us: Option<u128>,
    /// Read-set captured during this (original) execution, present only when the tx-cache
    /// shadow-diff diagnostic is enabled and a [`crate::RecordingDb`] is wired. Stored in the
    /// cache so a later rebuild can diff the values this execution read against the values its
    /// reconstructed prefix serves, pinpointing the read that flipped the cached outcome.
    pub created_reads: Option<CapturedReads>,
}

#[derive(Debug)]
struct CachedTransactionExecution {
    receipt: BaseTransactionReceipt,
    state: EvmState,
    result: ExecutionResult<BaseHaltReason>,
    execution_time_us: Option<u128>,
}

/// Executes or fetches cached values for transactions in a flashblock.
#[derive(Debug)]
pub struct PendingStateBuilder<E, ChainSpec> {
    cumulative_gas_used: u64,
    next_log_index: usize,

    evm: E,
    pending_block: Block<BaseTxEnvelope, Header>,
    l1_block_info: L1BlockInfo,
    receipt_builder: UnifiedReceiptBuilder<ChainSpec>,
    chain_spec: ChainSpec,

    prev_pending_blocks: Option<Arc<PendingBlocks>>,
    state_overrides: StateOverride,
    /// Shared read-capture log for the tx-cache shadow-diff (Layer 2). `None` unless wired via
    /// [`PendingStateBuilder::set_read_log`]; the underlying database must be a
    /// [`crate::RecordingDb`] backed by the same log for captures to populate.
    read_log: Option<Arc<Mutex<ReadLog>>>,
}

impl<E, ChainSpec, DB> PendingStateBuilder<E, ChainSpec>
where
    E: Evm<DB = DB, HaltReason = BaseHaltReason>,
    DB: Database + DatabaseCommit,
    E::Tx: FromRecoveredTx<BaseTxEnvelope>,
    ChainSpec: Upgrades + Clone,
{
    /// Creates a new pending state builder.
    pub fn new(
        chain_spec: ChainSpec,
        evm: E,
        pending_block: Block<BaseTxEnvelope, Header>,
        prev_pending_blocks: Option<Arc<PendingBlocks>>,
        l1_block_info: L1BlockInfo,
        state_overrides: StateOverride,
    ) -> Self {
        Self {
            pending_block,
            evm,
            cumulative_gas_used: 0,
            next_log_index: 0,
            prev_pending_blocks,
            l1_block_info,
            state_overrides,
            chain_spec: chain_spec.clone(),
            receipt_builder: UnifiedReceiptBuilder::new(chain_spec),
            read_log: None,
        }
    }

    /// Wires the shared read-capture log used by the tx-cache shadow-diff (Layer 2).
    ///
    /// Only effective when the builder's database is a [`crate::RecordingDb`] backed by the same
    /// log; otherwise captures stay empty and the read-set section of the diagnostic is skipped.
    pub fn set_read_log(&mut self, read_log: Arc<Mutex<ReadLog>>) {
        self.read_log = Some(read_log);
    }

    /// Returns whether the tx-cache shadow-diff diagnostic is enabled via
    /// `BASE_FLASHBLOCKS_TX_CACHE_SHADOW_DIFF`. Cached after the first read.
    fn shadow_diff_enabled() -> bool {
        static SHADOW_DIFF: OnceLock<bool> = OnceLock::new();
        *SHADOW_DIFF
            .get_or_init(|| std::env::var_os("BASE_FLASHBLOCKS_TX_CACHE_SHADOW_DIFF").is_some())
    }

    /// Arms the shared read-capture log so the next EVM call records its read-set. A no-op when no
    /// log is wired (i.e. the database is not a [`crate::RecordingDb`]).
    fn arm_read_capture(&self) {
        if let Some(log) = &self.read_log
            && let Ok(mut log) = log.lock()
        {
            log.enabled = true;
            log.storage.clear();
            log.accounts.clear();
            log.blockhashes.clear();
        }
    }

    /// Disarms read capture and returns the reads recorded since the last [`Self::arm_read_capture`].
    fn take_captured_reads(&self) -> Option<CapturedReads> {
        self.read_log.as_ref().and_then(|log| {
            log.lock().ok().map(|mut log| {
                log.enabled = false;
                CapturedReads {
                    storage: std::mem::take(&mut log.storage),
                    accounts: std::mem::take(&mut log.accounts),
                    blockhashes: std::mem::take(&mut log.blockhashes),
                }
            })
        })
    }

    /// Consumes the builder and returns the database and state overrides.
    pub fn into_db_and_state_overrides(self) -> (DB, StateOverride) {
        (self.evm.into_db(), self.state_overrides)
    }

    /// Returns a mutable reference to the underlying database.
    pub fn db_mut(&mut self) -> &mut DB {
        self.evm.db_mut()
    }

    /// Returns a reference to the configured EVM inspector.
    pub fn inspector(&self) -> &E::Inspector {
        self.evm.inspector()
    }

    /// Enables the configured EVM inspector for subsequent transactions.
    pub fn enable_inspector(&mut self) {
        self.evm.enable_inspector()
    }

    /// Disables the configured EVM inspector for subsequent transactions.
    pub fn disable_inspector(&mut self) {
        self.evm.disable_inspector()
    }

    /// Seeds block-level offsets when appending transactions to an already-executed pending block.
    pub const fn set_execution_offsets(&mut self, cumulative_gas_used: u64, next_log_index: usize) {
        self.cumulative_gas_used = cumulative_gas_used;
        self.next_log_index = next_log_index;
    }

    /// Returns the cumulative gas used for the current pending block.
    pub const fn cumulative_gas_used(&self) -> u64 {
        self.cumulative_gas_used
    }

    /// Returns the next log index for the current pending block.
    pub const fn next_log_index(&self) -> usize {
        self.next_log_index
    }

    /// Executes a single transaction and updates internal state.
    /// Should be called in order for each transaction.
    #[instrument(level = "debug", skip_all, fields(tx_hash = %transaction.tx_hash(), idx = idx))]
    pub fn execute_transaction(
        &mut self,
        idx: usize,
        transaction: Recovered<BaseTxEnvelope>,
    ) -> Result<ExecutedPendingTransaction, StateProcessorError> {
        let tx_hash = transaction.tx_hash();
        static DISABLE_TX_CACHE: OnceLock<bool> = OnceLock::new();
        let disable_tx_cache = *DISABLE_TX_CACHE
            .get_or_init(|| std::env::var_os("BASE_FLASHBLOCKS_DISABLE_TX_CACHE").is_some());

        let effective_gas_price = if transaction.is_deposit() {
            0
        } else {
            self.pending_block
                .base_fee_per_gas
                .map(|base_fee| {
                    transaction.effective_tip_per_gas(base_fee).unwrap_or_default()
                        + base_fee as u128
                })
                .unwrap_or_else(|| transaction.max_fee_per_gas())
        };

        // Check if we have all the data we need to reuse the previous execution.
        let cached_execution = if disable_tx_cache {
            debug!(tx_hash = %tx_hash, idx, "skipping cached pending transaction execution");
            None
        } else {
            let cached_execution = self.prev_pending_blocks.as_ref().and_then(|p| {
                Some(CachedTransactionExecution {
                    receipt: p.get_receipt(tx_hash)?.clone(),
                    state: p.get_transaction_state(&tx_hash)?,
                    result: p.get_transaction_result(&tx_hash)?.clone(),
                    execution_time_us: p.get_execution_time(&tx_hash),
                })
            });

            if cached_execution.is_some() {
                debug!(tx_hash = %tx_hash, idx, "reusing cached pending transaction execution");
            }

            cached_execution
        };

        // If cached, we can fill out pending block data using previous execution results
        // If not cached, we need to execute the transaction and build pending block data from scratch
        if let Some(cached_execution) = cached_execution {
            self.execute_with_cached_data(transaction, cached_execution, idx, effective_gas_price)
        } else {
            self.execute_with_evm(transaction, idx, effective_gas_price)
        }
    }

    /// Applies EIP-4788, EIP-2935, and Canyon create2 deployer pre-execution changes to the EVM.
    ///
    /// Must be called once per block, before executing any transactions. This mirrors the
    /// `apply_pre_execution_changes` behavior of [`base_common_evm::BaseBlockExecutor`] to ensure
    /// that the cached execution results match what the validator computes.
    pub fn apply_pre_execution_changes(
        &mut self,
        parent_hash: B256,
        parent_beacon_block_root: Option<B256>,
    ) -> Result<(), StateProcessorError>
    where
        DB: AlloyDatabase + StateDB,
        ChainSpec: Clone,
    {
        let spec = self.receipt_builder.chain_spec();
        let mut system_caller = SystemCaller::new(spec.clone());
        system_caller
            .apply_blockhashes_contract_call(parent_hash, &mut self.evm)
            .map_err(|e| ExecutionError::EvmEnv(e.to_string()))?;
        system_caller
            .apply_beacon_root_contract_call(parent_beacon_block_root, &mut self.evm)
            .map_err(|e| ExecutionError::EvmEnv(e.to_string()))?;

        ensure_create2_deployer(spec, self.pending_block.timestamp, self.evm.db_mut())
            .map_err(|e| ExecutionError::EvmEnv(e.to_string()))?;

        Ok(())
    }

    /// Builds transaction result from cached receipt and state data.
    fn execute_with_cached_data(
        &mut self,
        transaction: Recovered<BaseTxEnvelope>,
        cached_execution: CachedTransactionExecution,
        idx: usize,
        effective_gas_price: u128,
    ) -> Result<ExecutedPendingTransaction, StateProcessorError> {
        let CachedTransactionExecution { receipt, state, result, execution_time_us } =
            cached_execution;

        // Shadow-diff (#1): when enabled, re-execute the cached transaction against the
        // current pending state and warn if the fresh outcome diverges from the cached
        // (frozen) one. This pinpoints the transaction whose stale cache entry produces a
        // wrong success/revert. Behavior is unchanged; the fresh result is discarded.
        self.shadow_diff_cached_execution(&transaction, idx, &result, &state);

        // Carry the originally-captured read-set forward across rebuilds (shadow-diff only), so a
        // reused cached transaction keeps its read-set available to subsequent rebuilds whose
        // `PendingBlocksBuilder` starts empty. `None` when the diagnostic is off.
        let created_reads = self
            .prev_pending_blocks
            .as_ref()
            .and_then(|p| p.get_transaction_reads(&transaction.tx_hash()).cloned());

        let (deposit_receipt_version, deposit_nonce) = if transaction.is_deposit() {
            let BaseReceipt::Deposit(deposit_receipt) = &receipt.inner.inner.receipt else {
                return Err(ExecutionError::DepositReceiptMismatch.into());
            };

            (deposit_receipt.deposit_receipt_version, deposit_receipt.deposit_nonce)
        } else {
            (None, None)
        };

        let rpc_transaction = Transaction {
            inner: alloy_rpc_types_eth::Transaction {
                inner: transaction,
                block_hash: None,
                block_number: Some(self.pending_block.number),
                block_timestamp: Some(self.pending_block.timestamp),
                transaction_index: Some(idx as u64),
                effective_gas_price: Some(effective_gas_price),
            },
            deposit_nonce,
            deposit_receipt_version,
        };

        self.cumulative_gas_used = self
            .cumulative_gas_used
            .checked_add(receipt.inner.gas_used)
            .ok_or(ExecutionError::GasOverflow)?;
        self.next_log_index += receipt.inner.logs().len();

        for address in state.keys() {
            self.evm.db_mut().basic(*address).map_err(|err| {
                StateProcessorError::Execution(ExecutionError::EvmEnv(err.to_string()))
            })?;
        }
        self.evm.db_mut().commit(state.clone());

        Ok(ExecutedPendingTransaction {
            rpc_transaction,
            receipt,
            state,
            result,
            execution_time_us,
            created_reads,
        })
    }

    /// Re-executes a cached transaction against the current pending state (without committing)
    /// and emits a `warn!` when the fresh execution outcome diverges from the cached (frozen)
    /// one. This is a diagnostic for the transaction-cache staleness bug: it identifies the exact
    /// transaction whose cached status/gas no longer matches a clean re-execution.
    ///
    /// Gated behind `BASE_FLASHBLOCKS_TX_CACHE_SHADOW_DIFF`; a no-op (and zero cost) when unset.
    /// The fresh result is discarded, so committed state and returned values are unchanged.
    fn shadow_diff_cached_execution(
        &mut self,
        transaction: &Recovered<BaseTxEnvelope>,
        idx: usize,
        cached_result: &ExecutionResult<BaseHaltReason>,
        cached_state: &EvmState,
    ) {
        if !Self::shadow_diff_enabled() {
            return;
        }

        let tx_hash = transaction.tx_hash();
        // (Layer 2) Enable read-capture for just this re-execution so we record the diverging
        // transaction's full read-set, including read-only slots absent from the write-set.
        self.arm_read_capture();
        let outcome = self.evm.transact(transaction);
        let reads = self.take_captured_reads();
        match outcome {
            Ok(ResultAndState { result: fresh_result, .. }) => {
                let cached_status = cached_result.is_success();
                let fresh_status = fresh_result.is_success();
                let cached_gas = cached_result.tx_gas_used();
                let fresh_gas = fresh_result.tx_gas_used();
                if cached_status != fresh_status {
                    warn!(
                        tx_hash = %tx_hash,
                        idx,
                        block_number = self.pending_block.number,
                        cached_status,
                        fresh_status,
                        cached_gas,
                        fresh_gas,
                        "tx cache hit diverges from re-execution: status mismatch"
                    );
                    // Layers 1 + 3: pinpoint which storage slot the rebuilt prefix
                    // reconstructed differently than when the cache entry was written, and
                    // attribute it to its predecessor writer in the pending window. Layer 2: dump
                    // the captured read-set (catches read-only divergences invisible to the
                    // write-set). Layer 4: diff the read-set against the one captured at the
                    // cache's original execution to pinpoint the exact read that flipped.
                    let cached_reads = self
                        .prev_pending_blocks
                        .as_ref()
                        .and_then(|p| p.get_transaction_reads(&tx_hash).cloned());
                    self.dump_cache_divergence(
                        tx_hash,
                        idx,
                        cached_state,
                        reads.as_ref(),
                        cached_reads.as_ref(),
                    );
                } else if cached_gas != fresh_gas {
                    debug!(
                        tx_hash = %tx_hash,
                        idx,
                        block_number = self.pending_block.number,
                        cached_gas,
                        fresh_gas,
                        "tx cache hit diverges from re-execution: gas mismatch"
                    );
                }
            }
            Err(err) => {
                warn!(
                    tx_hash = %tx_hash,
                    idx,
                    block_number = self.pending_block.number,
                    error = %err,
                    "tx cache shadow re-execution failed"
                );
            }
        }
    }

    /// Diagnostic for a confirmed tx-cache status divergence (Layers 1 + 2 + 3 + 4).
    ///
    /// First (Layers 1 + 3), for every storage slot the cached execution *wrote*, compares the
    /// value the cache recorded as the slot's pre-state (`original_value`) against the value the
    /// *current* rebuilt pending state holds, attributing any mismatch to the slot's last
    /// *predecessor* writer in the pending window (stale cached predecessor vs. base-state
    /// mismatch).
    ///
    /// Then (Layer 2), walks the diverging re-execution's captured read-set (`fresh_reads`) —
    /// which includes read-only slots absent from the write-set — and attributes each storage read
    /// to its last predecessor writer. `fresh_reads` is `None` (and these sections are skipped)
    /// unless a [`crate::RecordingDb`] was wired via [`PendingStateBuilder::set_read_log`].
    ///
    /// Finally (Layer 4), diffs the read-set captured at the cache's *original* execution
    /// (`cached_reads`) against `fresh_reads`. A storage slot or account whose value differs
    /// between the two executions is the exact read that flipped the outcome — the conclusive
    /// root-cause locator, since it is independent of writer attribution heuristics.
    fn dump_cache_divergence(
        &mut self,
        tx_hash: B256,
        idx: usize,
        cached_state: &EvmState,
        fresh_reads: Option<&CapturedReads>,
        cached_reads: Option<&CapturedReads>,
    ) {
        let prev = self.prev_pending_blocks.clone();
        let block_number = self.pending_block.number;
        let writers_before = |addr: Address, slot: U256| {
            prev.as_ref().map(|p| p.slot_writers_before(addr, slot, tx_hash)).unwrap_or_default()
        };

        // Layers 1 + 3: write-set slots whose recorded pre-state no longer matches the rebuild.
        for (addr, acct) in cached_state {
            for (slot, sslot) in &acct.storage {
                let current = match self.evm.db_mut().storage(*addr, *slot) {
                    Ok(value) => value,
                    Err(_) => continue,
                };
                if current == sslot.original_value {
                    continue;
                }
                let writers = writers_before(*addr, *slot);
                let last_writer = writers.last();
                warn!(
                    tx_hash = %tx_hash,
                    idx,
                    block_number,
                    address = %addr,
                    slot = %slot,
                    cached_read = %sslot.original_value,
                    rebuild_now = %current,
                    cached_write = %sslot.present_value,
                    writers = writers.len(),
                    last_writer_tx = ?last_writer.map(|w| w.0),
                    last_writer_block = ?last_writer.map(|w| w.1),
                    last_writer_value = ?last_writer.map(|w| w.2),
                    "tx cache divergence: prefix slot reconstructed differently"
                );
            }
        }

        // Layer 4: conclusive diff of cached-vs-fresh read-sets. Independent of writer attribution.
        match (cached_reads, fresh_reads) {
            (Some(cached), Some(fresh)) => {
                Self::diff_read_sets(tx_hash, idx, block_number, cached, fresh);
            }
            (None, _) => {
                debug!(
                    tx_hash = %tx_hash,
                    idx,
                    "tx cache divergence: original read-set unavailable (cache predates capture)"
                );
            }
            _ => {}
        }

        // Layer 2: full read-set of the diverging re-execution. Each storage read is attributed to
        // its last predecessor writer in the pending window; a read whose value disagrees with that
        // writer's committed value (or a base read with no writer) is the read-only divergence
        // candidate.
        let Some(reads) = fresh_reads else {
            debug!(
                tx_hash = %tx_hash,
                idx,
                "tx cache divergence: read-set capture unavailable (db not recording)"
            );
            return;
        };
        for (addr, slot, value) in &reads.storage {
            let writers = writers_before(*addr, *slot);
            let last_writer = writers.last();
            let inconsistent = last_writer.is_some_and(|w| w.2 != *value);
            if inconsistent {
                warn!(
                    tx_hash = %tx_hash,
                    idx,
                    block_number,
                    address = %addr,
                    slot = %slot,
                    read_value = %value,
                    writers = writers.len(),
                    last_writer_tx = ?last_writer.map(|w| w.0),
                    last_writer_block = ?last_writer.map(|w| w.1),
                    last_writer_value = ?last_writer.map(|w| w.2),
                    "tx cache divergence: read-set slot disagrees with predecessor writer"
                );
            } else {
                debug!(
                    tx_hash = %tx_hash,
                    idx,
                    address = %addr,
                    slot = %slot,
                    read_value = %value,
                    writers = writers.len(),
                    last_writer_value = ?last_writer.map(|w| w.2),
                    "tx cache divergence: read-set slot"
                );
            }
        }
        warn!(
            tx_hash = %tx_hash,
            idx,
            block_number,
            storage_reads = reads.storage.len(),
            account_reads = reads.accounts.len(),
            "tx cache divergence: read-set captured"
        );
    }

    /// Layer 4: warns on every storage slot / account whose value differs between the read-set
    /// captured at the cache's original execution and the read-set of the diverging re-execution.
    /// Each warning isolates a single read that the rebuilt prefix served differently — the
    /// conclusive cause of the status flip.
    fn diff_read_sets(
        tx_hash: B256,
        idx: usize,
        block_number: u64,
        cached: &CapturedReads,
        fresh: &CapturedReads,
    ) {
        // First-occurrence value per (address, slot): the pre-state the transaction observed.
        let mut cached_storage: HashMap<(Address, U256), U256> = HashMap::new();
        for (addr, slot, value) in &cached.storage {
            cached_storage.entry((*addr, *slot)).or_insert(*value);
        }
        let mut fresh_storage: HashMap<(Address, U256), U256> = HashMap::new();
        for (addr, slot, value) in &fresh.storage {
            fresh_storage.entry((*addr, *slot)).or_insert(*value);
        }
        let mut storage_flips = 0usize;
        for ((addr, slot), cached_value) in &cached_storage {
            if let Some(fresh_value) = fresh_storage.get(&(*addr, *slot))
                && fresh_value != cached_value
            {
                storage_flips += 1;
                warn!(
                    tx_hash = %tx_hash,
                    idx,
                    block_number,
                    address = %addr,
                    slot = %slot,
                    cached_read = %cached_value,
                    fresh_read = %fresh_value,
                    "tx cache divergence: storage read flipped between cached and rebuild execution"
                );
            }
        }

        let mut cached_accounts: HashMap<Address, (U256, u64)> = HashMap::new();
        for (addr, balance, nonce) in &cached.accounts {
            cached_accounts.entry(*addr).or_insert((*balance, *nonce));
        }
        let mut fresh_accounts: HashMap<Address, (U256, u64)> = HashMap::new();
        for (addr, balance, nonce) in &fresh.accounts {
            fresh_accounts.entry(*addr).or_insert((*balance, *nonce));
        }
        let mut account_flips = 0usize;
        for (addr, cached_acct) in &cached_accounts {
            if let Some(fresh_acct) = fresh_accounts.get(addr)
                && fresh_acct != cached_acct
            {
                account_flips += 1;
                warn!(
                    tx_hash = %tx_hash,
                    idx,
                    block_number,
                    address = %addr,
                    cached_balance = %cached_acct.0,
                    fresh_balance = %fresh_acct.0,
                    cached_nonce = cached_acct.1,
                    fresh_nonce = fresh_acct.1,
                    "tx cache divergence: account read flipped between cached and rebuild execution"
                );
            }
        }

        // `BLOCKHASH` reads: not part of account/storage state, but a non-deterministic-looking
        // input if the two executions resolve the same block number to different hashes.
        for (number, cached_hash) in &cached.blockhashes {
            if let Some((_, fresh_hash)) =
                fresh.blockhashes.iter().find(|(fresh_number, _)| fresh_number == number)
                && fresh_hash != cached_hash
            {
                warn!(
                    tx_hash = %tx_hash,
                    idx,
                    block_number,
                    queried_block = number,
                    cached_hash = %cached_hash,
                    fresh_hash = %fresh_hash,
                    "tx cache divergence: blockhash read flipped between cached and rebuild execution"
                );
            }
        }

        // Ordered storage-read diff: the value-flip pass above only compares the *first* read of
        // each slot on the intersection, so it misses (a) a slot whose *later* read differs after
        // an intervening write (read-after-write within the tx) and (b) a path split where the two
        // executions start reading *different* slots. Walking both ordered sequences in lockstep
        // pinpoints the exact first divergence: a same-slot value difference is a read-after-write
        // divergence; a different-slot entry means the paths split just before it (the preceding
        // read is the last common point), implicating a non-storage branch input (gas, returndata,
        // call success, etc.).
        let first_divergence =
            cached.storage.iter().zip(fresh.storage.iter()).position(|(a, b)| a != b);
        match first_divergence {
            Some(pos) => {
                let (caddr, cslot, cval) = cached.storage[pos];
                let (faddr, fslot, fval) = fresh.storage[pos];
                let same_slot = caddr == faddr && cslot == fslot;
                let prev = pos.checked_sub(1).map(|p| cached.storage[p]);
                warn!(
                    tx_hash = %tx_hash,
                    idx,
                    block_number,
                    seq_index = pos,
                    same_slot,
                    cached_address = %caddr,
                    cached_slot = %cslot,
                    cached_value = %cval,
                    fresh_address = %faddr,
                    fresh_slot = %fslot,
                    fresh_value = %fval,
                    prev_common_address = ?prev.map(|p| p.0),
                    prev_common_slot = ?prev.map(|p| p.1),
                    "tx cache divergence: first storage read sequence divergence"
                );
            }
            None if cached.storage.len() != fresh.storage.len() => {
                // Sequences agree on the common prefix but one execution read further: the path
                // split at the end of the shorter sequence (e.g. one side reverted earlier).
                let common = cached.storage.len().min(fresh.storage.len());
                let extra = cached.storage.get(common).or_else(|| fresh.storage.get(common));
                warn!(
                    tx_hash = %tx_hash,
                    idx,
                    block_number,
                    common_prefix_len = common,
                    cached_len = cached.storage.len(),
                    fresh_len = fresh.storage.len(),
                    next_address = ?extra.map(|e| e.0),
                    next_slot = ?extra.map(|e| e.1),
                    "tx cache divergence: storage read sequences diverge in length only"
                );
            }
            None => {}
        }

        warn!(
            tx_hash = %tx_hash,
            idx,
            block_number,
            storage_flips,
            account_flips,
            cached_storage_reads = cached.storage.len(),
            fresh_storage_reads = fresh.storage.len(),
            cached_blockhash_reads = cached.blockhashes.len(),
            fresh_blockhash_reads = fresh.blockhashes.len(),
            "tx cache divergence: read-set diff complete"
        );
    }

    fn jovian_da_footprint_estimation(
        &mut self,
        tx_env: &Recovered<BaseTxEnvelope>,
    ) -> Result<u64, StateProcessorError> {
        // Try to use the enveloped tx if it exists, otherwise use the encoded 2718 bytes
        let encoded = estimate_tx_compressed_size(tx_env.into_encoded().encoded_bytes())
            .saturating_div(1_000_000);

        // Load the L1 block contract into the cache. If the L1 block contract is not pre-loaded the
        // database will panic when trying to fetch the DA footprint gas scalar.
        self.evm.db_mut().basic(Predeploys::L1_BLOCK_INFO).map_err(|err| {
            StateProcessorError::Execution(ExecutionError::DaFootprintEstimation(err.to_string()))
        })?;

        let da_footprint_gas_scalar = L1BlockInfo::fetch_da_footprint_gas_scalar(self.evm.db_mut())
            .map_err(|err| {
                StateProcessorError::Execution(ExecutionError::DaFootprintEstimation(
                    err.to_string(),
                ))
            })?
            .into();

        Ok(encoded.saturating_mul(da_footprint_gas_scalar))
    }

    /// Executes the transaction through the EVM and builds the result from scratch.
    fn execute_with_evm(
        &mut self,
        transaction: Recovered<BaseTxEnvelope>,
        idx: usize,
        effective_gas_price: u128,
    ) -> Result<ExecutedPendingTransaction, StateProcessorError> {
        let tx_hash = transaction.tx_hash();

        let is_deposit = transaction.is_deposit();

        let da_footprint_used = if self
            .chain_spec
            .is_jovian_active_at_timestamp(self.evm.block().timestamp().saturating_to())
            && !is_deposit
        {
            self.jovian_da_footprint_estimation(&transaction)?
        } else {
            0
        };

        // When the shadow-diff diagnostic is on, capture this original execution's read-set so a
        // later rebuild can diff it against the re-execution's read-set (Layer 4). Off by default.
        let capture_reads = Self::shadow_diff_enabled();
        if capture_reads {
            self.arm_read_capture();
        }
        let start = Instant::now();
        let transact_result = self.evm.transact(&transaction);
        let elapsed_us = start.elapsed().as_micros();
        let created_reads = if capture_reads { self.take_captured_reads() } else { None };

        match transact_result {
            Ok(ResultAndState { state, result }) => {
                let gas_used = result.tx_gas_used();
                for (addr, acc) in &state {
                    let existing_override = self.state_overrides.entry(*addr).or_default();
                    existing_override.balance = Some(acc.info.balance);
                    existing_override.nonce = Some(acc.info.nonce);
                    existing_override.code = acc.info.code.clone().map(|code| code.bytes());

                    let existing =
                        existing_override.state_diff.get_or_insert_with(Default::default);
                    let changed_slots = acc
                        .storage
                        .iter()
                        .map(|(&key, slot)| (B256::from(key), B256::from(slot.present_value)));

                    existing.extend(changed_slots);
                }

                self.cumulative_gas_used = self
                    .cumulative_gas_used
                    .checked_add(gas_used)
                    .ok_or(ExecutionError::GasOverflow)?;

                // Build receipt using the unified receipt builder - handles both
                // deposit and non-deposit transactions seamlessly
                let receipt = self.receipt_builder.build(
                    &mut self.evm,
                    &transaction,
                    &result,
                    self.cumulative_gas_used,
                    self.pending_block.timestamp,
                )?;

                let meta = TransactionMeta {
                    tx_hash,
                    index: idx as u64,
                    block_hash: B256::ZERO, // block hash is not available yet for flashblocks
                    block_number: self.pending_block.number,
                    base_fee: self.pending_block.base_fee_per_gas,
                    excess_blob_gas: self.pending_block.excess_blob_gas,
                    timestamp: self.pending_block.timestamp,
                };

                let sender = transaction.signer();
                let input: ConvertReceiptInput<'_, BasePrimitives> = ConvertReceiptInput {
                    receipt: receipt.clone(),
                    tx: Recovered::new_unchecked(&transaction, sender),
                    gas_used,
                    next_log_index: self.next_log_index,
                    meta,
                };

                let mut base_receipt = BaseRpcReceiptBuilder::new(
                    self.receipt_builder.chain_spec(),
                    input,
                    &mut self.l1_block_info,
                )
                .map_err(|e| ExecutionError::RpcReceiptBuild(e.to_string()))?
                .build();

                base_receipt.inner.blob_gas_used = Some(da_footprint_used);
                self.next_log_index += receipt.logs().len();

                let (deposit_receipt_version, deposit_nonce) = if transaction.is_deposit() {
                    let BaseReceipt::Deposit(deposit_receipt) = &base_receipt.inner.inner.receipt
                    else {
                        return Err(ExecutionError::DepositReceiptMismatch.into());
                    };

                    (deposit_receipt.deposit_receipt_version, deposit_receipt.deposit_nonce)
                } else {
                    (None, None)
                };

                let rpc_transaction = Transaction {
                    inner: alloy_rpc_types_eth::Transaction {
                        inner: transaction,
                        block_hash: None,
                        block_number: Some(self.pending_block.number),
                        block_timestamp: Some(self.pending_block.timestamp),
                        transaction_index: Some(idx as u64),
                        effective_gas_price: Some(effective_gas_price),
                    },
                    deposit_nonce,
                    deposit_receipt_version,
                };
                self.evm.db_mut().commit(state.clone());

                Ok(ExecutedPendingTransaction {
                    rpc_transaction,
                    receipt: base_receipt,
                    state,
                    result,
                    execution_time_us: Some(elapsed_us),
                    created_reads,
                })
            }
            Err(e) => Err(ExecutionError::TransactionFailed {
                tx_hash,
                sender: transaction.signer(),
                reason: format!("{e:?}"),
            }
            .into()),
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use alloy_consensus::{Block, BlockBody, Header, Signed};
    use alloy_eips::eip4788::{BEACON_ROOTS_ADDRESS, BEACON_ROOTS_CODE};
    use alloy_primitives::{Address, B256, TxKind, U256, address, uint};
    use alloy_rpc_types_engine::PayloadId;
    use base_common_consensus::BaseTxEnvelope;
    use base_common_evm::L1BlockInfo;
    use base_common_flashblocks::{
        ExecutionPayloadBaseV1, ExecutionPayloadFlashblockDeltaV1, Flashblock, Metadata,
    };
    use base_execution_chainspec::BaseChainSpecBuilder;
    use base_execution_evm::BaseEvmConfig;
    use reth_evm::ConfigureEvm;
    use reth_revm::State;
    use revm::{
        database::InMemoryDB,
        state::{AccountInfo, Bytecode},
    };

    use super::*;

    // Base mainnet Ecotone activation timestamp, after which EIP-4788 is active.
    const BASE_MAINNET_ECOTONE_TIMESTAMP: u64 = 1_710_374_401;
    // A timestamp just after Ecotone activation.
    const POST_ECOTONE_TIMESTAMP: u64 = BASE_MAINNET_ECOTONE_TIMESTAMP + 1;
    // EIP-4788 ring buffer length (hardcoded in the contract bytecode).
    const BEACON_ROOTS_HISTORY_BUFFER_LENGTH: u64 = 8191;

    fn make_db_with_beacon_roots_contract() -> State<InMemoryDB> {
        let mut db = State::builder().with_database(InMemoryDB::default()).build();
        let code = Bytecode::new_raw(BEACON_ROOTS_CODE.clone());
        let code_hash = code.hash_slow();
        db.insert_account(
            BEACON_ROOTS_ADDRESS,
            AccountInfo { code: Some(code), code_hash, nonce: 1, ..Default::default() },
        );
        db
    }

    #[test]
    fn apply_pre_execution_changes_stores_beacon_root_in_eip4788_contract() {
        let db = make_db_with_beacon_roots_contract();

        let chain_spec = Arc::new(BaseChainSpecBuilder::base_mainnet().build());
        let evm_config = BaseEvmConfig::base(Arc::clone(&chain_spec));
        let header = Header { timestamp: POST_ECOTONE_TIMESTAMP, number: 1, ..Default::default() };
        let evm_env = evm_config.evm_env(&header).expect("failed to build evm env");
        let evm = evm_config.evm_with_env(db, evm_env);
        let pending_block = Block {
            header: Header { timestamp: POST_ECOTONE_TIMESTAMP, number: 1, ..Default::default() },
            body: BlockBody::<BaseTxEnvelope>::default(),
        };
        let mut builder = PendingStateBuilder::new(
            chain_spec,
            evm,
            pending_block,
            None,
            L1BlockInfo::default(),
            Default::default(),
        );

        let parent_beacon_block_root = B256::from([0xab; 32]);
        builder
            .apply_pre_execution_changes(B256::ZERO, Some(parent_beacon_block_root))
            .expect("apply_pre_execution_changes should succeed");

        let (db, _) = builder.into_db_and_state_overrides();

        // EIP-4788 stores parent_beacon_block_root at:
        //   slot = timestamp % HISTORY_BUFFER_LENGTH + HISTORY_BUFFER_LENGTH
        let timestamp_idx = POST_ECOTONE_TIMESTAMP % BEACON_ROOTS_HISTORY_BUFFER_LENGTH;
        let root_slot = U256::from(timestamp_idx + BEACON_ROOTS_HISTORY_BUFFER_LENGTH);
        let beacon_account = db
            .cache
            .accounts
            .get(&BEACON_ROOTS_ADDRESS)
            .expect("beacon roots contract should be in cache after commit");
        let storage = &beacon_account
            .account
            .as_ref()
            .expect("beacon roots account should be populated")
            .storage;
        let stored_root = *storage.get(&root_slot).expect("beacon root slot should be written");

        assert_eq!(
            stored_root,
            U256::from_be_bytes(parent_beacon_block_root.0),
            "EIP-4788 should store parent_beacon_block_root at timestamp-indexed slot"
        );
    }

    #[test]
    fn apply_pre_execution_changes_pre_ecotone_with_no_beacon_root_is_noop_for_eip4788() {
        let db = make_db_with_beacon_roots_contract();

        // Use a timestamp before Ecotone activation so EIP-4788 is inactive.
        // In this regime None is valid (no beacon root contract call is made).
        let pre_ecotone_timestamp = BASE_MAINNET_ECOTONE_TIMESTAMP - 1;

        let chain_spec = Arc::new(BaseChainSpecBuilder::base_mainnet().build());
        let evm_config = BaseEvmConfig::base(Arc::clone(&chain_spec));
        let header = Header { timestamp: pre_ecotone_timestamp, number: 1, ..Default::default() };
        let evm_env = evm_config.evm_env(&header).expect("failed to build evm env");
        let evm = evm_config.evm_with_env(db, evm_env);
        let pending_block = Block {
            header: Header { timestamp: pre_ecotone_timestamp, number: 1, ..Default::default() },
            body: BlockBody::<BaseTxEnvelope>::default(),
        };
        let mut builder = PendingStateBuilder::new(
            chain_spec,
            evm,
            pending_block,
            None,
            L1BlockInfo::default(),
            Default::default(),
        );

        builder
            .apply_pre_execution_changes(B256::ZERO, None)
            .expect("apply_pre_execution_changes should succeed pre-Ecotone with no beacon root");

        let (db, _) = builder.into_db_and_state_overrides();

        // EIP-4788 is inactive pre-Ecotone, so the contract should have no storage writes.
        let beacon_account = db.cache.accounts.get(&BEACON_ROOTS_ADDRESS);
        let has_storage_writes =
            beacon_account.and_then(|a| a.account.as_ref()).is_some_and(|a| !a.storage.is_empty());
        assert!(
            !has_storage_writes,
            "EIP-4788 contract should not be called and have no storage writes pre-Ecotone"
        );
    }

    const L1_BLOCK_ADDRESS: Address =
        Address::new([0x42, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0x15]);

    const DA_FOOTPRINT_GAS_SCALAR_SLOT: U256 = uint!(8_U256);

    fn create_legacy_tx() -> alloy_consensus::transaction::Recovered<BaseTxEnvelope> {
        let tx = alloy_consensus::TxLegacy {
            chain_id: Some(8453),
            nonce: 0,
            gas_price: 1_000_000_000,
            gas_limit: 21_000,
            to: TxKind::Call(Address::ZERO),
            value: U256::ZERO,
            input: Default::default(),
        };

        let envelope = BaseTxEnvelope::Legacy(Signed::new_unchecked(
            tx,
            alloy_primitives::Signature::test_signature(),
            B256::ZERO,
        ));

        alloy_consensus::transaction::Recovered::new_unchecked(envelope, Address::ZERO)
    }

    #[test]
    fn cached_execute_transaction_preserves_timing_from_prev_pending_blocks() {
        let chain_spec = Arc::new(BaseChainSpecBuilder::base_mainnet().build());
        let evm_config = BaseEvmConfig::base(Arc::clone(&chain_spec));

        let header = Header {
            number: 1,
            timestamp: 100,
            gas_limit: 30_000_000,
            base_fee_per_gas: Some(1_000_000_000),
            ..Default::default()
        };

        let mut db = InMemoryDB::default();
        db.insert_account_info(
            Address::ZERO,
            AccountInfo {
                balance: U256::from(1_000_000_000_000_000_000u128),
                ..Default::default()
            },
        );

        let evm_env = evm_config.evm_env(&header).expect("failed to create evm env");
        let evm = evm_config.evm_with_env(db, evm_env);
        let pending_block = Block { header: header.clone(), body: Default::default() };
        let mut first_builder = PendingStateBuilder::new(
            (*chain_spec).clone(),
            evm,
            pending_block,
            None,
            L1BlockInfo::default(),
            StateOverride::default(),
        );

        let tx = create_legacy_tx();
        let tx_hash = tx.tx_hash();
        let first_result =
            first_builder.execute_transaction(0, tx).expect("transaction execution failed");

        let mut pending_blocks_builder = crate::PendingBlocksBuilder::new();
        pending_blocks_builder
            .with_header(alloy_consensus::Sealed::new_unchecked(header.clone(), B256::ZERO));
        pending_blocks_builder.with_flashblocks([Flashblock {
            payload_id: PayloadId::default(),
            index: 0,
            base: Some(ExecutionPayloadBaseV1 {
                parent_beacon_block_root: B256::ZERO,
                parent_hash: B256::ZERO,
                fee_recipient: Address::ZERO,
                prev_randao: B256::ZERO,
                block_number: header.number,
                gas_limit: header.gas_limit,
                timestamp: header.timestamp,
                extra_data: Default::default(),
                base_fee_per_gas: U256::from(header.base_fee_per_gas.unwrap_or_default()),
            }),
            diff: ExecutionPayloadFlashblockDeltaV1 {
                state_root: B256::ZERO,
                receipts_root: B256::ZERO,
                logs_bloom: Default::default(),
                gas_used: first_result.receipt.inner.gas_used,
                block_hash: B256::ZERO,
                transactions: vec![],
                withdrawals: vec![],
                withdrawals_root: B256::ZERO,
                blob_gas_used: None,
            },
            metadata: Metadata { block_number: header.number },
        }]);
        pending_blocks_builder.with_receipt(tx_hash, first_result.receipt.clone());
        pending_blocks_builder.with_transaction_state(tx_hash, first_result.state.clone());
        pending_blocks_builder.with_transaction_result(tx_hash, first_result.result);
        pending_blocks_builder.with_execution_time(tx_hash, 1_234);

        let prev_pending_blocks =
            Arc::new(pending_blocks_builder.build().expect("should build cached pending blocks"));

        let second_evm_env = evm_config.evm_env(&header).expect("failed to create evm env");
        let second_evm = evm_config.evm_with_env(InMemoryDB::default(), second_evm_env);
        let second_pending_block = Block { header, body: Default::default() };
        let mut second_builder = PendingStateBuilder::new(
            (*chain_spec).clone(),
            second_evm,
            second_pending_block,
            Some(prev_pending_blocks),
            L1BlockInfo::default(),
            StateOverride::default(),
        );

        let cached_result = second_builder
            .execute_transaction(0, create_legacy_tx())
            .expect("cached transaction execution failed");

        assert_eq!(cached_result.execution_time_us, Some(1_234));
    }

    #[test]
    fn flashblock_tx_has_nonzero_blob_gas_used_when_jovian_active() {
        let chain_spec = Arc::new(BaseChainSpecBuilder::base_mainnet().jovian_activated().build());
        let mut db = InMemoryDB::default();

        let sender_info = AccountInfo {
            balance: U256::from(1_000_000_000_000_000_000u128),
            ..Default::default()
        };
        db.insert_account_info(Address::ZERO, sender_info);

        // Seed L1 block contract slot 8 with DA footprint gas scalar at bytes [18..20] (big-endian u16).
        let da_scalar: u16 = 100;
        let mut slot_value = [0u8; 32];
        slot_value[18..20].copy_from_slice(&da_scalar.to_be_bytes());
        db.insert_account_info(L1_BLOCK_ADDRESS, revm::state::AccountInfo::default());
        db.insert_account_storage(
            L1_BLOCK_ADDRESS,
            DA_FOOTPRINT_GAS_SCALAR_SLOT,
            U256::from_be_bytes(slot_value),
        )
        .expect("failed to insert L1 block storage");

        let header = Header {
            timestamp: 100,
            gas_limit: 30_000_000,
            base_fee_per_gas: Some(1_000_000_000),
            ..Default::default()
        };
        let evm_config = BaseEvmConfig::base(Arc::clone(&chain_spec));
        let evm_env = evm_config.evm_env(&header).expect("failed to create evm env");
        let evm = evm_config.evm_with_env(db, evm_env);

        let pending_block = Block { header, body: Default::default() };

        let mut builder = PendingStateBuilder::new(
            (*chain_spec).clone(),
            evm,
            pending_block,
            None,
            L1BlockInfo::default(),
            StateOverride::default(),
        );

        let tx = create_legacy_tx();
        let result = builder.execute_transaction(0, tx).expect("transaction execution failed");

        let blob_gas_used =
            result.receipt.inner.blob_gas_used.expect("blob_gas_used should be set");
        assert!(
            blob_gas_used > 0,
            "blob_gas_used should be > 0 when Jovian is active for non-deposit tx, got {blob_gas_used}"
        );
    }

    #[test]
    fn flashblock_tx_has_zero_blob_gas_used_when_jovian_inactive() {
        let chain_spec = Arc::new(BaseChainSpecBuilder::base_mainnet().build());
        let mut db = InMemoryDB::default();

        let sender_info = AccountInfo {
            balance: U256::from(1_000_000_000_000_000_000u128),
            ..Default::default()
        };
        db.insert_account_info(Address::ZERO, sender_info);

        let header = Header {
            timestamp: 100,
            gas_limit: 30_000_000,
            base_fee_per_gas: Some(1_000_000_000),
            ..Default::default()
        };
        let evm_config = BaseEvmConfig::base(Arc::clone(&chain_spec));
        let evm_env = evm_config.evm_env(&header).expect("failed to create evm env");
        let evm = evm_config.evm_with_env(db, evm_env);

        let pending_block = Block { header, body: Default::default() };

        let mut builder = PendingStateBuilder::new(
            (*chain_spec).clone(),
            evm,
            pending_block,
            None,
            L1BlockInfo::default(),
            StateOverride::default(),
        );

        let tx = create_legacy_tx();
        let result = builder.execute_transaction(0, tx).expect("transaction execution failed");

        let blob_gas_used =
            result.receipt.inner.blob_gas_used.expect("blob_gas_used should be set");
        assert_eq!(
            blob_gas_used, 0,
            "blob_gas_used should be 0 when Jovian is inactive, got {blob_gas_used}"
        );
    }

    #[test]
    fn flashblock_deposit_tx_has_zero_blob_gas_used_when_jovian_active() {
        let chain_spec = Arc::new(BaseChainSpecBuilder::base_mainnet().jovian_activated().build());
        let mut db = InMemoryDB::default();

        let deposit_sender: Address = address!("0x1234567890123456789012345678901234567890");
        let sender_info = AccountInfo {
            balance: U256::from(1_000_000_000_000_000_000u128),
            ..Default::default()
        };
        db.insert_account_info(deposit_sender, sender_info);

        // Seed L1 block contract slot 8 with DA footprint gas scalar at bytes [18..20] (big-endian u16).
        let da_scalar: u16 = 100;
        let mut slot_value = [0u8; 32];
        slot_value[18..20].copy_from_slice(&da_scalar.to_be_bytes());
        db.insert_account_info(L1_BLOCK_ADDRESS, AccountInfo::default());
        db.insert_account_storage(
            L1_BLOCK_ADDRESS,
            DA_FOOTPRINT_GAS_SCALAR_SLOT,
            U256::from_be_bytes(slot_value),
        )
        .expect("failed to insert L1 block storage");

        let header = Header {
            timestamp: 100,
            gas_limit: 30_000_000,
            base_fee_per_gas: Some(1_000_000_000),
            ..Default::default()
        };
        let evm_config = BaseEvmConfig::base(Arc::clone(&chain_spec));
        let evm_env = evm_config.evm_env(&header).expect("failed to create evm env");
        let evm = evm_config.evm_with_env(db, evm_env);

        let pending_block = Block { header, body: Default::default() };

        let mut builder = PendingStateBuilder::new(
            (*chain_spec).clone(),
            evm,
            pending_block,
            None,
            L1BlockInfo::default(),
            StateOverride::default(),
        );

        let deposit_tx = base_common_consensus::TxDeposit {
            source_hash: B256::ZERO,
            from: deposit_sender,
            to: TxKind::Call(Address::ZERO),
            mint: 0,
            value: U256::ZERO,
            gas_limit: 21_000,
            is_system_transaction: false,
            input: Default::default(),
        };

        let sealed = alloy_consensus::Sealed::new_unchecked(deposit_tx, B256::ZERO);
        let envelope = BaseTxEnvelope::Deposit(sealed);
        let tx = alloy_consensus::transaction::Recovered::new_unchecked(envelope, deposit_sender);

        let result = builder.execute_transaction(0, tx).expect("deposit execution failed");

        let blob_gas_used =
            result.receipt.inner.blob_gas_used.expect("blob_gas_used should be set");
        assert_eq!(
            blob_gas_used, 0,
            "blob_gas_used should be 0 for deposit tx even when Jovian is active"
        );
    }

    /// Regression test: `execute_with_cached_data` must commit the cached `EvmState` to the EVM
    /// database so that subsequent transactions see the correct post-tx state.
    ///
    /// Without the commit, a fresh tx executed after a cached one runs against stale state
    /// (e.g. missing nonce increment, stale storage), producing logs that differ from what
    /// the final block-building executor produces. This causes a receipt root mismatch
    /// during block validation because the sequencer re-executes everything from scratch.
    #[test]
    fn cached_execute_commits_state_so_subsequent_fresh_txs_see_updated_nonce() {
        // Phase 1: execute tx A freshly to obtain the real EvmState and receipt
        // that would be stored in PendingBlocks after the first flashblock round.
        let chain_spec = Arc::new(BaseChainSpecBuilder::base_mainnet().build());
        let evm_config = BaseEvmConfig::base(Arc::clone(&chain_spec));
        let sender = Address::ZERO;

        let header = Header {
            number: 1,
            timestamp: 100,
            gas_limit: 30_000_000,
            base_fee_per_gas: Some(1_000_000_000),
            ..Default::default()
        };

        let mut inner_db = InMemoryDB::default();
        inner_db.insert_account_info(
            sender,
            AccountInfo {
                balance: U256::from(1_000_000_000_000_000_000u128),
                nonce: 0,
                ..Default::default()
            },
        );
        let db = State::builder().with_database(inner_db).build();

        let evm_env = evm_config.evm_env(&header).expect("failed to create evm env");
        let evm = evm_config.evm_with_env(db, evm_env);
        let pending_block = Block { header: header.clone(), body: Default::default() };
        let mut first_builder = PendingStateBuilder::new(
            (*chain_spec).clone(),
            evm,
            pending_block,
            None,
            L1BlockInfo::default(),
            StateOverride::default(),
        );

        let tx_a = create_legacy_tx();
        let tx_a_hash = tx_a.tx_hash();
        let first_result =
            first_builder.execute_transaction(0, tx_a).expect("first execution failed");

        // Sanity-check: fresh execution increments the sender nonce from 0 to 1.
        let (first_db, _) = first_builder.into_db_and_state_overrides();
        let sender_nonce_after_tx_a = first_db
            .cache
            .accounts
            .get(&sender)
            .and_then(|a| a.account_info())
            .map(|info| info.nonce)
            .expect("sender should be in cache after tx A");

        assert_eq!(sender_nonce_after_tx_a, 1, "tx A should increment nonce to 1");

        // Phase 2: store the result of tx A in PendingBlocks, simulating what the
        // processor does after the first flashblock is built.
        let mut pending_blocks_builder = crate::PendingBlocksBuilder::new();
        pending_blocks_builder
            .with_header(alloy_consensus::Sealed::new_unchecked(header.clone(), B256::ZERO));
        pending_blocks_builder.with_flashblocks([Flashblock {
            payload_id: PayloadId::default(),
            index: 0,
            base: Some(ExecutionPayloadBaseV1 {
                parent_beacon_block_root: B256::ZERO,
                parent_hash: B256::ZERO,
                fee_recipient: Address::ZERO,
                prev_randao: B256::ZERO,
                block_number: header.number,
                gas_limit: header.gas_limit,
                timestamp: header.timestamp,
                extra_data: Default::default(),
                base_fee_per_gas: U256::from(header.base_fee_per_gas.unwrap_or_default()),
            }),
            diff: ExecutionPayloadFlashblockDeltaV1 {
                state_root: B256::ZERO,
                receipts_root: B256::ZERO,
                logs_bloom: Default::default(),
                gas_used: first_result.receipt.inner.gas_used,
                block_hash: B256::ZERO,
                transactions: vec![],
                withdrawals: vec![],
                withdrawals_root: B256::ZERO,
                blob_gas_used: None,
            },
            metadata: Metadata { block_number: header.number },
        }]);
        pending_blocks_builder.with_transaction_sender(tx_a_hash, sender);
        pending_blocks_builder.with_receipt(tx_a_hash, first_result.receipt.clone());
        pending_blocks_builder.with_transaction_state(tx_a_hash, first_result.state.clone());
        pending_blocks_builder.with_transaction_result(tx_a_hash, first_result.result);

        let prev_pending_blocks =
            Arc::new(pending_blocks_builder.build().expect("should build pending blocks"));

        // Phase 3: build a second flashblock whose EVM starts from scratch (nonce 0).
        // tx A is now in prev_pending_blocks so execute_transaction will take the cached
        // path (execute_with_cached_data). After that call the EVM database must reflect
        // the committed state of tx A (nonce 1) so any subsequent fresh tx executes
        // against the correct state.
        let mut inner_db2 = InMemoryDB::default();
        inner_db2.insert_account_info(
            sender,
            AccountInfo {
                balance: U256::from(1_000_000_000_000_000_000u128),
                nonce: 0,
                ..Default::default()
            },
        );
        let db2 = State::builder().with_database(inner_db2).build();
        let second_evm_env = evm_config.evm_env(&header).expect("failed to create evm env");
        let second_evm = evm_config.evm_with_env(db2, second_evm_env);
        let second_pending_block = Block { header, body: Default::default() };
        let mut second_builder = PendingStateBuilder::new(
            (*chain_spec).clone(),
            second_evm,
            second_pending_block,
            Some(prev_pending_blocks),
            L1BlockInfo::default(),
            StateOverride::default(),
        );

        second_builder
            .execute_transaction(0, create_legacy_tx())
            .expect("cached tx A execution failed");

        // The EVM database must now show nonce 1 for the sender, proving that
        // execute_with_cached_data committed the state before returning.
        let (second_db_after, _) = second_builder.into_db_and_state_overrides();
        let sender_nonce_after_cached_tx_a = second_db_after
            .cache
            .accounts
            .get(&sender)
            .and_then(|a| a.account_info())
            .map(|info| info.nonce)
            .expect("sender should be in cache after cached tx A");

        assert_eq!(
            sender_nonce_after_cached_tx_a, 1,
            "cached tx A must commit state so the sender nonce is 1 (not 0)"
        );
    }
}
