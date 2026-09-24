use std::{
    any::Any,
    collections::{BTreeMap, BTreeSet},
    sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
    },
    time::Instant,
};

use alloy_consensus::{BlockHeader, Transaction, constants::KECCAK_EMPTY};
use alloy_eips::eip2718::Encodable2718;
use alloy_primitives::{Address, B256, LogData, U256, map::AddressSet};
use base_common_chains::Upgrades;
use base_common_consensus::{
    AccountChange, Eip8130Constants, Eip8130Signed, Eip8130TimestampError,
};
use base_common_evm::{BaseSpecId, L1BlockInfo};
use base_common_genesis::DaFootprintGasScalarUpdate;
use base_common_precompiles::NonceManagerStorage;
use base_execution_eip8130::{
    ApplyError, AuthError, FeeCheck, IntrinsicGas, IntrinsicGasInput, NonceError, NonceMode,
    NonceValidator, TransactionAuthorizer, TxAuthError,
};
use base_precompile_storage::{
    BasePrecompileError, PrecompileStorageProvider, StorageCtx, validate_loaded_code_presence,
};
use parking_lot::RwLock;
use reth_chainspec::{ChainSpecProvider, EthChainSpec};
use reth_evm::ConfigureEvm;
use reth_primitives_traits::{
    Block, BlockBody, BlockTy, GotExpected, SealedBlock,
    transaction::error::InvalidTransactionError,
};
use reth_storage_api::{
    AccountInfoReader, AccountReader, BlockReaderIdExt, StateProvider, StateProviderFactory,
};
use reth_transaction_pool::{
    EthPoolTransaction, EthTransactionValidator, TransactionOrigin, TransactionValidationOutcome,
    TransactionValidator,
    error::{InvalidPoolTransactionError, PoolTransactionError},
    validate::ValidTransaction,
};
use revm::{
    context::journaled_state::JournalCheckpoint,
    state::{AccountInfo, Bytecode},
};

use crate::{BasePooledTx, InvalidationKey, LimitClass, ValidatorMetrics, WatchManifest, WatchSet};

/// Base-specific transaction pool validation errors.
#[derive(Debug, thiserror::Error)]
pub enum BaseTxPoolError {
    /// The transaction's DA footprint exceeds the block gas limit.
    #[error(
        "transaction DA footprint ({transaction_da_footprint}) exceeds block gas limit ({block_gas_limit})"
    )]
    DaFootprintExceedsBlockGasLimit {
        /// The computed DA footprint of the transaction (`estimated_da_size` * `da_footprint_gas_scalar`).
        transaction_da_footprint: u64,
        /// The current block gas limit.
        block_gas_limit: u64,
    },
    /// The transaction failed EIP-8130-specific stateful validation.
    #[error("EIP-8130 validation failed: {reason}")]
    Eip8130Validation {
        /// Static validation label for the failure.
        reason: &'static str,
    },
}

/// Resolved EIP-8130 actors and state data required to build the pool outcome.
#[derive(Debug, Clone)]
struct Eip8130ValidationState {
    sender: Address,
    payer: Address,
    classification_generation: u64,
    payer_balance: U256,
    payer_balance_after_auth: U256,
    sender_nonce: u64,
    sender_bytecode_hash: Option<B256>,
    /// Payer-authentication gas metered on top of `gas_limit`. The execution
    /// path charges the operator fee on `gas_limit + payer_auth`, so admission
    /// must do the same to avoid admitting operator-fee-underfunded sponsored
    /// transactions. Zero for self-pay transactions.
    payer_auth: u64,
    watch_set: WatchSet,
    sender_locked: bool,
    payer_locked: bool,
    payer_trusted: bool,
    payer_max_cost: U256,
    /// Authorization reads and predicates used for build-time revalidation.
    manifest: WatchManifest,
}

/// Read-only precompile storage adapter backed by a reth state provider.
struct StateProviderPrecompileStorage<'a> {
    state: &'a dyn StateProvider,
    chain_id: u64,
    timestamp: u64,
}

impl<'a> StateProviderPrecompileStorage<'a> {
    fn new(state: &'a dyn StateProvider, chain_id: u64, timestamp: u64) -> Self {
        Self { state, chain_id, timestamp }
    }

    fn provider_error(error: impl core::fmt::Display) -> BasePrecompileError {
        BasePrecompileError::Fatal(error.to_string())
    }
}

impl PrecompileStorageProvider for StateProviderPrecompileStorage<'_> {
    fn chain_id(&self) -> u64 {
        self.chain_id
    }

    fn timestamp(&self) -> U256 {
        U256::from(self.timestamp)
    }

    fn beneficiary(&self) -> Address {
        Address::ZERO
    }

    fn block_number(&self) -> u64 {
        0
    }

    fn origin(&self) -> Address {
        Address::ZERO
    }

    fn set_code(&mut self, _address: Address, _code: Bytecode) -> Result<(), BasePrecompileError> {
        Err(BasePrecompileError::StaticCallViolation)
    }

    fn with_account_info(
        &mut self,
        address: Address,
        f: &mut dyn FnMut(&AccountInfo),
    ) -> Result<(), BasePrecompileError> {
        let account =
            self.state.basic_account(&address).map_err(Self::provider_error)?.unwrap_or_default();
        let account_info = AccountInfo::from(account);
        f(&account_info);
        Ok(())
    }

    fn with_account_code(
        &mut self,
        address: Address,
        f: &mut dyn FnMut(&Bytecode),
    ) -> Result<(), BasePrecompileError> {
        let expected_hash = self
            .state
            .basic_account(&address)
            .map_err(Self::provider_error)?
            .and_then(|account| account.bytecode_hash)
            .unwrap_or(B256::ZERO);
        let code = if expected_hash == B256::ZERO || expected_hash == KECCAK_EMPTY {
            Bytecode::default()
        } else {
            self.state
                .bytecode_by_hash(&expected_hash)
                .map_err(Self::provider_error)?
                .ok_or_else(|| {
                    BasePrecompileError::Fatal(
                        "account code unavailable for non-empty code hash".into(),
                    )
                })?
                .0
        };
        validate_loaded_code_presence(expected_hash, &code)?;
        f(&code);
        Ok(())
    }

    fn sload(&mut self, address: Address, key: U256) -> Result<U256, BasePrecompileError> {
        self.state
            .storage(address, B256::from(key.to_be_bytes()))
            .map_err(Self::provider_error)
            .map(|value| value.unwrap_or_default())
    }

    fn tload(&mut self, _address: Address, _key: U256) -> Result<U256, BasePrecompileError> {
        Ok(U256::ZERO)
    }

    fn tload_unmetered(
        &mut self,
        _address: Address,
        _key: U256,
    ) -> Result<U256, BasePrecompileError> {
        // No transient state during validation; the read is trivially unmetered.
        Ok(U256::ZERO)
    }

    fn sstore(
        &mut self,
        _address: Address,
        _key: U256,
        _value: U256,
    ) -> Result<(), BasePrecompileError> {
        Err(BasePrecompileError::StaticCallViolation)
    }

    fn tstore(
        &mut self,
        _address: Address,
        _key: U256,
        _value: U256,
    ) -> Result<(), BasePrecompileError> {
        Err(BasePrecompileError::StaticCallViolation)
    }

    fn emit_event(
        &mut self,
        _address: Address,
        _event: LogData,
    ) -> Result<(), BasePrecompileError> {
        Err(BasePrecompileError::StaticCallViolation)
    }

    fn deduct_gas(&mut self, _gas: u64) -> Result<(), BasePrecompileError> {
        Ok(())
    }

    fn deduct_state_gas(&mut self, _gas: u64) -> Result<(), BasePrecompileError> {
        Ok(())
    }

    fn refund_gas(&mut self, _gas: i64) {}

    fn gas_limit(&self) -> u64 {
        u64::MAX
    }

    fn gas_used(&self) -> u64 {
        0
    }

    fn state_gas_used(&self) -> u64 {
        0
    }

    fn gas_refunded(&self) -> i64 {
        0
    }

    fn reservoir(&self) -> u64 {
        0
    }

    fn is_static(&self) -> bool {
        true
    }

    fn call_value(&self) -> U256 {
        U256::ZERO
    }

    fn caller(&self) -> Address {
        Address::ZERO
    }

    // Per the trait contract, returns the *previous* caller. This provider does
    // not track a mutable caller (`caller()` is always `Address::ZERO`), so the
    // previous value is always `Address::ZERO` — not the `caller` argument.
    fn replace_caller(&mut self, _caller: Address) -> Address {
        Address::ZERO
    }

    fn checkpoint(&mut self) -> JournalCheckpoint {
        JournalCheckpoint::default()
    }

    fn commit_latest_checkpoint(&mut self) {}

    fn checkpoint_revert(&mut self, _checkpoint: JournalCheckpoint) {}

    fn metered_keccak256(&mut self, data: &[u8]) -> Result<B256, BasePrecompileError> {
        Ok(alloy_primitives::keccak256(data))
    }
}

/// Writable in-memory overlay over a read-only [`StateProviderPrecompileStorage`].
///
/// EIP-8130 admission authorizes a transaction's account changes by *applying*
/// them against the evolving state — a create installs its initial actors before
/// the next change authenticates against them, a config change advances the
/// channel sequence the next same-channel entry reads, and the sender is
/// authenticated against the resulting post-apply state. This mirrors block
/// execution exactly (both run [`TransactionAuthorizer::authorize_and_apply`]),
/// so the pool accepts exactly what the builder will include.
///
/// The pool's state snapshot is read-only, so this overlay buffers `SSTORE`s in
/// memory and serves them back on `SLOAD`, falling through to the snapshot for
/// unbuffered slots. The buffered writes are scoped to a single validation and
/// dropped with the overlay: admission never mutates canonical state. Deferred
/// account-code effects validate the canonical code through `with_account_code`,
/// while the subsequent `set_code` is accepted and discarded.
// `BTreeMap` (not `HashMap`) for deterministic iteration order. The overlay
// only performs point reads/writes today so ordering is not observed, but
// precompile storage feeds consensus-relevant state — a `BTreeMap` keeps a
// future iteration-sensitive change from silently depending on
// `HashMap`'s non-deterministic order.
struct OverlayPrecompileStorage<'a> {
    inner: StateProviderPrecompileStorage<'a>,
    storage: BTreeMap<(Address, U256), U256>,
    transient: BTreeMap<(Address, U256), U256>,
    code_reads: BTreeSet<Address>,
}

impl<'a> OverlayPrecompileStorage<'a> {
    const fn new(inner: StateProviderPrecompileStorage<'a>) -> Self {
        Self {
            inner,
            storage: BTreeMap::new(),
            transient: BTreeMap::new(),
            code_reads: BTreeSet::new(),
        }
    }
}

impl PrecompileStorageProvider for OverlayPrecompileStorage<'_> {
    fn chain_id(&self) -> u64 {
        self.inner.chain_id()
    }

    fn timestamp(&self) -> U256 {
        self.inner.timestamp()
    }

    fn beneficiary(&self) -> Address {
        self.inner.beneficiary()
    }

    fn block_number(&self) -> u64 {
        self.inner.block_number()
    }

    fn origin(&self) -> Address {
        self.inner.origin()
    }

    fn set_code(&mut self, _address: Address, _code: Bytecode) -> Result<(), BasePrecompileError> {
        // Delegation installation validates canonical code before this deferred
        // write, which admission intentionally discards.
        Ok(())
    }

    // NOTE: account *info* (nonce, balance, code hash) is intentionally not
    // overlaid — it delegates to the read-only inner provider, so a
    // counterfactual-create account reads back as empty/default here. This is
    // sound because account-configuration state uses `sload`/`sstore` (which
    // the overlay buffers), while delegation code reads below intentionally use
    // the canonical snapshot. If a future change needs created-account info in
    // this flow, the overlay would need to buffer account info too.
    fn with_account_info(
        &mut self,
        address: Address,
        f: &mut dyn FnMut(&AccountInfo),
    ) -> Result<(), BasePrecompileError> {
        self.inner.with_account_info(address, f)
    }

    // Delegation installation validates the canonical code; deferred code
    // writes are discarded by `set_code` above and cannot affect this read.
    fn with_account_code(
        &mut self,
        address: Address,
        f: &mut dyn FnMut(&Bytecode),
    ) -> Result<(), BasePrecompileError> {
        self.inner.with_account_code(address, f)?;
        self.code_reads.insert(address);
        Ok(())
    }

    fn sload(&mut self, address: Address, key: U256) -> Result<U256, BasePrecompileError> {
        // Overlay hits are this transaction's buffered writes, not canonical
        // dependencies. Recording one would make later manifest validation
        // compare a transaction's own effect against pre-state and reject it.
        if let Some(value) = self.storage.get(&(address, key)) {
            return Ok(*value);
        }
        self.inner.sload(address, key)
    }

    fn tload(&mut self, address: Address, key: U256) -> Result<U256, BasePrecompileError> {
        Ok(self.transient.get(&(address, key)).copied().unwrap_or_default())
    }

    fn tload_unmetered(
        &mut self,
        address: Address,
        key: U256,
    ) -> Result<U256, BasePrecompileError> {
        // Overlay backend: `tload` never deducts gas, so the raw read is unmetered.
        Ok(self.transient.get(&(address, key)).copied().unwrap_or_default())
    }

    fn sstore(
        &mut self,
        address: Address,
        key: U256,
        value: U256,
    ) -> Result<(), BasePrecompileError> {
        self.storage.insert((address, key), value);
        Ok(())
    }

    fn tstore(
        &mut self,
        address: Address,
        key: U256,
        value: U256,
    ) -> Result<(), BasePrecompileError> {
        self.transient.insert((address, key), value);
        Ok(())
    }

    fn emit_event(
        &mut self,
        _address: Address,
        _event: LogData,
    ) -> Result<(), BasePrecompileError> {
        Ok(())
    }

    fn deduct_gas(&mut self, _gas: u64) -> Result<(), BasePrecompileError> {
        Ok(())
    }

    fn deduct_state_gas(&mut self, _gas: u64) -> Result<(), BasePrecompileError> {
        Ok(())
    }

    fn refund_gas(&mut self, _gas: i64) {}

    fn gas_limit(&self) -> u64 {
        u64::MAX
    }

    fn gas_used(&self) -> u64 {
        0
    }

    fn state_gas_used(&self) -> u64 {
        0
    }

    fn gas_refunded(&self) -> i64 {
        0
    }

    fn reservoir(&self) -> u64 {
        0
    }

    fn is_static(&self) -> bool {
        false
    }

    fn call_value(&self) -> U256 {
        U256::ZERO
    }

    fn caller(&self) -> Address {
        Address::ZERO
    }

    // Per the trait contract, returns the *previous* caller. The overlay does
    // not track a mutable caller (`caller()` is always `Address::ZERO`), so the
    // previous value is always `Address::ZERO` — not the `caller` argument.
    fn replace_caller(&mut self, _caller: Address) -> Address {
        Address::ZERO
    }

    // The overlay deliberately does not journal: `checkpoint`/`checkpoint_revert`
    // are no-ops. This is sound only because the admission flow
    // (`TransactionAuthorizer::authorize_and_apply`, and the
    // `ConfigChangeAuthorizer` / `AccountChangeApplier` steps it drives) never
    // performs an internal checkpoint/revert cycle: it either succeeds and the
    // overlay's buffered writes are read back as the evolving state, or it
    // returns an error and the entire overlay is dropped by the caller. If a
    // future change introduces an internal checkpoint/revert within that flow,
    // partial writes would leak within the overlay — this storage would then
    // need real journalling (snapshot the `storage`/`transient` maps on
    // `checkpoint` and restore them on `checkpoint_revert`).
    fn checkpoint(&mut self) -> JournalCheckpoint {
        JournalCheckpoint::default()
    }

    fn commit_latest_checkpoint(&mut self) {}

    // A `checkpoint_revert` would silently leak partial writes (the overlay
    // cannot roll back), so trip loudly in debug/test builds if the admission
    // flow ever introduces an internal revert. In release this stays a no-op:
    // the overlay relies on being dropped wholesale on error, never on
    // fine-grained rollback.
    fn checkpoint_revert(&mut self, _checkpoint: JournalCheckpoint) {
        debug_assert!(
            false,
            "OverlayPrecompileStorage does not support checkpoint_revert; the admission \
             authorize-and-apply flow must abort wholesale (drop the overlay), not revert \
             internally. A nested revert here would silently leak partial writes — the overlay \
             needs real journalling before this path is used."
        );
    }

    fn metered_keccak256(&mut self, data: &[u8]) -> Result<B256, BasePrecompileError> {
        Ok(alloy_primitives::keccak256(data))
    }
}

impl PoolTransactionError for BaseTxPoolError {
    fn is_bad_transaction(&self) -> bool {
        true
    }

    fn as_any(&self) -> &dyn Any {
        self
    }
}

/// Tracks additional infos for the current block.
#[derive(Debug, Default)]
pub struct BaseL1BlockInfo {
    /// The current L1 block info.
    l1_block_info: RwLock<L1BlockInfo>,
    /// Current block timestamp.
    timestamp: AtomicU64,
}

impl BaseL1BlockInfo {
    /// Returns the most recent timestamp
    pub fn timestamp(&self) -> u64 {
        self.timestamp.load(Ordering::Relaxed)
    }
}

/// Validator for Base transactions.
#[derive(Debug, Clone)]
pub struct BaseTransactionValidator<Client, Tx, Evm> {
    /// The type that performs the actual validation.
    inner: Arc<EthTransactionValidator<Client, Tx, Evm>>,
    /// Additional block info required for validation.
    block_info: Arc<BaseL1BlockInfo>,
    /// If true, ensure that the transaction's sender has enough balance to cover the L1 gas fee
    /// derived from the tracked L1 block info that is extracted from the first transaction in the
    /// L2 block.
    require_l1_data_gas_fee: bool,
    /// Generation counter bumped when the state-diff feed is cleared, so pool
    /// admission can detect a stale classification snapshot. With the Keystore
    /// removed there is no lock/trusted classification to cache, so this only
    /// advances on a feed-gap clear.
    limit_class_cache_generation: Arc<AtomicU64>,
}

impl<Client, Tx, Evm> BaseTransactionValidator<Client, Tx, Evm> {
    /// Returns the configured chain spec
    pub fn chain_spec(&self) -> Arc<Client::ChainSpec>
    where
        Client: ChainSpecProvider,
    {
        self.inner.chain_spec()
    }

    /// Returns the configured client
    pub fn client(&self) -> &Client {
        self.inner.client()
    }

    /// Returns the current block timestamp.
    fn block_timestamp(&self) -> u64 {
        self.block_info.timestamp.load(Ordering::Relaxed)
    }

    /// Whether to ensure that the transaction's sender has enough balance to also cover the L1 gas
    /// fee.
    pub fn require_l1_data_gas_fee(self, require_l1_data_gas_fee: bool) -> Self {
        Self { require_l1_data_gas_fee, ..self }
    }

    /// Returns whether this validator also requires the transaction's sender to have enough balance
    /// to cover the L1 gas fee.
    pub const fn requires_l1_data_gas_fee(&self) -> bool {
        self.require_l1_data_gas_fee
    }

    /// Returns the canonical trusted delegation target set.
    ///
    /// The high-rate (balance-bounded) payer tier was removed with the Keystore,
    /// so there are no canonical trusted targets; this returns an empty set. The
    /// method (and [`Self::with_additional_trusted_delegation_targets`]) is
    /// retained as a no-op so node wiring that passes trusted targets keeps
    /// compiling.
    pub fn default_trusted_delegation_targets() -> AddressSet {
        AddressSet::default()
    }

    /// Retained no-op: the high-rate payer tier was removed, so trusted
    /// delegation targets no longer influence admission.
    pub fn with_additional_trusted_delegation_targets(self, _targets: AddressSet) -> Self {
        self
    }

    /// Returns the cache generation used to close validation/invalidation races.
    pub fn limit_class_cache_generation(&self) -> u64 {
        self.limit_class_cache_generation.load(Ordering::Acquire)
    }

    /// Retained for the state-diff feed: with no lock/trusted classification to
    /// cache there is nothing to invalidate per diff, so this is a no-op.
    pub const fn invalidate_limit_class_cache(&self, diffs: &[crate::AccountStateDiff]) {
        let _ = diffs;
    }

    /// Clears classifications after a state-diff feed gap. With no cached
    /// classification, this only advances the generation so any in-flight
    /// admission re-validates.
    pub fn clear_limit_class_cache(&self) {
        self.limit_class_cache_generation.fetch_add(1, Ordering::Release);
    }
}

impl<Client, Tx, Evm> BaseTransactionValidator<Client, Tx, Evm>
where
    Client: ChainSpecProvider<ChainSpec: Upgrades> + StateProviderFactory + BlockReaderIdExt + Sync,
    Tx: EthPoolTransaction + BasePooledTx,
    Evm: ConfigureEvm,
{
    /// Create a new [`BaseTransactionValidator`].
    pub fn new(inner: EthTransactionValidator<Client, Tx, Evm>) -> Self {
        let this = Self::with_block_info(inner, BaseL1BlockInfo::default());
        if let Ok(Some(block)) =
            this.inner.client().block_by_number_or_tag(alloy_eips::BlockNumberOrTag::Latest)
        {
            // genesis block has no txs, so we can't extract L1 info, we set the block info to empty
            // so that we will accept txs into the pool before the first block
            if block.header().number() == 0 {
                this.block_info.timestamp.store(block.header().timestamp(), Ordering::Relaxed);
            } else {
                this.update_l1_block_info(block.header(), block.body().transactions().first());
            }
        }

        this
    }

    /// Create a new [`BaseTransactionValidator`] with the given [`BaseL1BlockInfo`].
    pub fn with_block_info(
        inner: EthTransactionValidator<Client, Tx, Evm>,
        block_info: BaseL1BlockInfo,
    ) -> Self {
        Self {
            inner: Arc::new(inner),
            block_info: Arc::new(block_info),
            require_l1_data_gas_fee: true,
            limit_class_cache_generation: Arc::default(),
        }
    }

    /// Update the L1 block info for the given header and system transaction, if any.
    ///
    /// Note: this supports optional system transaction, in case this is used in a dev setup
    pub fn update_l1_block_info<H, T>(&self, header: &H, tx: Option<&T>)
    where
        H: BlockHeader,
        T: Transaction,
    {
        self.block_info.timestamp.store(header.timestamp(), Ordering::Relaxed);

        if let Some(Ok(l1_block_info)) = tx.map(base_execution_evm::extract_l1_info_from_tx) {
            *self.block_info.l1_block_info.write() = l1_block_info;
        }
    }

    /// Validates a single transaction.
    ///
    /// See also [`TransactionValidator::validate_transaction`]
    ///
    /// This behaves the same as [`BaseTransactionValidator::validate_one_with_state`], but creates
    /// a new state provider internally.
    pub async fn validate_one(
        &self,
        origin: TransactionOrigin,
        transaction: Tx,
    ) -> TransactionValidationOutcome<Tx> {
        self.validate_one_with_state(origin, transaction, &mut None).await
    }

    /// Validates a single transaction with a provided state provider.
    ///
    /// This allows reusing the same state provider across multiple transaction validations.
    ///
    /// See also [`TransactionValidator::validate_transaction`]
    ///
    /// This behaves the same as [`EthTransactionValidator::validate_one_with_state`], but in
    /// addition applies Base-specific validity checks:
    /// - ensures tx is not eip4844
    /// - for eip8130 (account abstraction): rejects submissions before the Everest upgrade is
    ///   active, runs structural checks, then runs EIP-8130-specific stateful validation for
    ///   actor authorization, nonce/replay state, intrinsic gas, create/delegation safety, and
    ///   payer funding instead of using the inner Eth validator
    /// - ensures that the account has enough balance to cover the L1 gas cost
    pub async fn validate_one_with_state(
        &self,
        origin: TransactionOrigin,
        transaction: Tx,
        state: &mut Option<Box<dyn AccountInfoReader + Send>>,
    ) -> TransactionValidationOutcome<Tx> {
        let kind = if transaction.as_eip8130().is_some() { "eip8130" } else { "standard" };
        let start = Instant::now();
        let outcome = self.validate_one_with_state_inner(origin, transaction, state);
        ValidatorMetrics::validate_seconds(kind).record(start.elapsed().as_secs_f64());
        outcome
    }

    fn validate_one_with_state_inner(
        &self,
        origin: TransactionOrigin,
        transaction: Tx,
        state: &mut Option<Box<dyn AccountInfoReader + Send>>,
    ) -> TransactionValidationOutcome<Tx> {
        if transaction.is_eip4844() {
            return TransactionValidationOutcome::Invalid(
                transaction,
                InvalidTransactionError::TxTypeNotSupported.into(),
            );
        }

        if transaction.as_eip8130().is_some() {
            let validation = {
                let signed = transaction.as_eip8130().expect("checked above");
                self.validate_eip8130_structural(signed)
                    .and_then(|()| self.validate_eip8130_full(signed))
            };
            let state = match validation {
                Ok(state) => state,
                Err(err) => return TransactionValidationOutcome::Invalid(transaction, err),
            };
            let propagate =
                matches!(origin, TransactionOrigin::External | TransactionOrigin::Local);
            transaction.set_watch_set(state.watch_set.clone());
            transaction.set_watch_manifest(state.manifest.clone());
            transaction.set_limit_class(LimitClass {
                sender: state.sender,
                payer: state.payer,
                classification_generation: state.classification_generation,
                sender_locked: state.sender_locked,
                payer_locked: state.payer_locked,
                payer_trusted: state.payer_trusted,
                payer_balance: state.payer_balance,
                max_cost: state.payer_max_cost,
            });
            let outcome = TransactionValidationOutcome::Valid {
                balance: state.payer_balance_after_auth,
                state_nonce: state.sender_nonce,
                transaction: ValidTransaction::new(transaction, None),
                propagate,
                bytecode_hash: state.sender_bytecode_hash,
                authorities: (state.payer != state.sender).then_some(vec![state.payer]),
            };
            return self.apply_base_checks(outcome, state.payer_auth);
        }
        let outcome = self.inner.validate_one_with_state(origin, transaction, state);
        self.apply_base_checks(outcome, 0)
    }

    /// Returns a low-cardinality sender authenticator label for metrics.
    fn sender_sig_type(signed: &Eip8130Signed) -> &'static str {
        if signed.explicit_sender().is_none() {
            return "k1";
        }
        Self::classify_authenticator(signed.sender_auth())
    }

    fn classify_authenticator(auth: &[u8]) -> &'static str {
        let Some(selector) = auth.get(..20).map(Address::from_slice) else {
            return "other";
        };
        if selector == Eip8130Constants::K1_AUTHENTICATOR { "k1" } else { "other" }
    }

    /// Runs full EIP-8130 admission checks that require account/precompile state:
    /// actor authorization, nonce/replay state, intrinsic gas, create-entry safety,
    /// and payer balance. This deliberately bypasses the inner Eth validator for
    /// EIP-8130 because configured senders may be smart contracts and sponsored
    /// transactions charge a payer instead of the sender.
    ///
    /// The `validate_one_with_state` snapshot is only an `AccountInfoReader`; EIP-8130 needs
    /// storage/code reads for account config, nonce channels, and delegation checks, so this path
    /// takes its own full state snapshot.
    fn validate_eip8130_full(
        &self,
        signed: &Eip8130Signed,
    ) -> Result<Eip8130ValidationState, InvalidPoolTransactionError> {
        let classification_generation = self.limit_class_cache_generation();
        let local_chain_id = self.inner.chain_spec().chain().id();
        let now = self.block_timestamp();
        let state = self.client().latest().map_err(|error| Self::provider_unavailable(error))?;

        // Authenticate the sender/payer as secp256k1 owners and record the
        // (single, optional) delegation code effect. With the Keystore removed
        // this needs no account-config storage — recovery and the named-actor
        // address match run against the transaction body alone.
        let auth_start = Instant::now();
        let applied =
            TransactionAuthorizer::authorize_and_apply(signed).map_err(Self::map_tx_auth_error)?;
        ValidatorMetrics::auth_seconds(Self::sender_sig_type(signed))
            .record(auth_start.elapsed().as_secs_f64());
        let sender = applied.actors.sender.account;
        let payer = applied.actors.payer.map_or(sender, |actor| actor.account);

        // Validate delegation delegatability (EOA-shaped code only) against a
        // discarded overlay so admission matches inclusion; the overlay records
        // the sender's code read for the invalidation watch set.
        let mut authorization_code_reads: Vec<Address> = Vec::new();
        if let Some(delegation) = applied.applied.delegation {
            let mut overlay = OverlayPrecompileStorage::new(StateProviderPrecompileStorage::new(
                &*state,
                local_chain_id,
                now,
            ));
            StorageCtx::enter(&mut overlay, |ctx| {
                delegation.install(ctx).map_err(TxAuthError::from)
            })
            .map_err(Self::map_tx_auth_error)?;
            authorization_code_reads = overlay.code_reads.into_iter().collect();
        }

        let sender_account = state
            .basic_account(&sender)
            .map_err(|error| Self::state_read_error(error, "sender account read failed"))?
            .unwrap_or_default();
        let protocol_nonce = sender_account.nonce;

        // Nonce validity is checked against canonical state.
        let mut storage = StateProviderPrecompileStorage::new(&*state, local_chain_id, now);
        // `NonceValidator::validate` compares the nonce-free replay ring's stored
        // `valid_before` (Unix milliseconds) against `now`, so it must be passed in
        // milliseconds (`block.timestamp * 1000`) — the storage overlay above keeps
        // `now` in seconds for `block.timestamp`. Passing raw seconds here would
        // make every replay entry look unexpired ~1000x too long and reject valid
        // nonce-free re-submissions.
        let now_ms = now.saturating_mul(1_000);
        StorageCtx::enter(&mut storage, |ctx| {
            let nonce_storage = NonceManagerStorage::new(ctx);
            NonceValidator::validate(
                signed.tx(),
                sender,
                protocol_nonce,
                &nonce_storage,
                NonceMode::Pool,
                now_ms,
            )
            .map(|_| ())
            .map_err(Self::map_nonce_error)
        })?;

        let (nonce_key_first_use, sender_nonce) =
            self.eip8130_nonce_state(&*state, local_chain_id, now, signed, sender, protocol_nonce)?;
        let encoded = self.eip8130_encoded(signed);
        // Admission uses the same safe ceiling as `eth_estimateGas`, so a tx whose
        // `gas_limit` was set from the estimate is never rejected here and can
        // never be admitted only to OOG at inclusion.
        let intrinsic = IntrinsicGas::compute(
            signed,
            encoded.as_ref(),
            &IntrinsicGasInput::worst_case(sender, nonce_key_first_use),
        )
        .map_err(|_| Self::eip8130_error("intrinsic gas computation failed"))?;
        // EIP-7623 calldata floor: a transaction whose `gas_limit` is below the
        // floor branch is invalid. `sender_floor >= sender_intrinsic`, so this
        // check subsumes the underfunded sender-intrinsic case.
        if signed.tx().gas_limit < intrinsic.sender_floor() {
            return Err(InvalidTransactionError::GasTooLow.into());
        }

        let payer_account = state
            .basic_account(&payer)
            .map_err(|error| Self::state_read_error(error, "payer account read failed"))?
            .unwrap_or_default();
        FeeCheck::validate_balance(
            payer_account.balance,
            signed.tx().gas_limit,
            intrinsic.payer_auth,
            signed.tx().max_fee_per_gas,
        )
        .map_err(|_| {
            InvalidPoolTransactionError::from(InvalidTransactionError::InsufficientFunds(
                GotExpected {
                    got: payer_account.balance,
                    expected: FeeCheck::max_fee_charge(
                        signed.tx().gas_limit,
                        intrinsic.payer_auth,
                        signed.tx().max_fee_per_gas,
                    ),
                }
                .into(),
            ))
        })?;
        let payer_auth_charge = U256::from(intrinsic.payer_auth)
            .saturating_mul(U256::from(signed.tx().max_fee_per_gas));

        let nonce_free = signed.tx().nonce_key == Eip8130Constants::NONCE_KEY_MAX;
        let transaction_expiry =
            Self::tx_valid_before_secs(signed.tx().valid_before_ms(), nonce_free);
        let mut watch_set = WatchSet::new().watch(InvalidationKey::Balance(payer));
        for address in authorization_code_reads {
            watch_set.push(InvalidationKey::CodeHash(address));
        }
        if transaction_expiry != u64::MAX {
            watch_set.push(InvalidationKey::expiry_bucket(transaction_expiry));
        }
        let nonce_key = signed.tx().nonce_key;
        if nonce_key.is_zero() {
            watch_set.push(InvalidationKey::ProtocolNonce(sender));
        } else if nonce_key != Eip8130Constants::NONCE_KEY_MAX
            && let Ok(slot) = NonceManagerStorage::nonce_slot(sender, nonce_key)
        {
            watch_set.push(InvalidationKey::Slot {
                address: NonceManagerStorage::ADDRESS,
                slot: B256::from(slot),
            });
        }

        let gas_charge = FeeCheck::max_fee_charge(
            signed.tx().gas_limit,
            intrinsic.payer_auth,
            signed.tx().max_fee_per_gas,
        );
        let additional_fee = if self.requires_l1_data_gas_fee() {
            let mut info = self.block_info.l1_block_info.read().clone();
            let spec_id = BaseSpecId::from_timestamp(self.chain_spec(), now);
            info.tx_cost(
                &encoded,
                U256::from(FeeCheck::max_chargeable_gas(
                    signed.tx().gas_limit,
                    intrinsic.payer_auth,
                )),
                spec_id,
            )
        } else {
            U256::ZERO
        };
        // Calls move `call.value` out of the sender, not the payer. A self-paying
        // sender reserves it alongside gas; a sponsored sender must hold it alone.
        let call_value = signed.tx().total_call_value();
        let payer_max_cost = gas_charge
            .saturating_add(additional_fee)
            .saturating_add(if payer == sender { call_value } else { U256::ZERO });
        let sender_obligation = if payer == sender { payer_max_cost } else { call_value };
        if sender_account.balance < sender_obligation {
            return Err(InvalidTransactionError::InsufficientFunds(
                GotExpected { got: sender_account.balance, expected: sender_obligation }.into(),
            )
            .into());
        }
        // The transaction's millisecond validity window folded onto the inclusive
        // block-timestamp *second* axis (`now_secs <= bound`) used by the
        // invalidation buckets and the manifest boundary.
        let manifest = WatchManifest::new(Vec::new(), payer, payer_max_cost, transaction_expiry);

        Ok(Eip8130ValidationState {
            sender,
            payer,
            classification_generation,
            payer_balance: payer_account.balance,
            payer_balance_after_auth: payer_account.balance.saturating_sub(payer_auth_charge),
            sender_nonce,
            sender_bytecode_hash: sender_account.bytecode_hash,
            payer_auth: intrinsic.payer_auth,
            watch_set,
            sender_locked: false,
            payer_locked: false,
            payer_trusted: false,
            payer_max_cost,
            manifest,
        })
    }

    /// Converts a transaction's `valid_before` (Unix **milliseconds**; `0` = no
    /// expiry) to the last block-timestamp *second* at which it is still
    /// includable, folding onto the inclusive-seconds axis (`now_secs <= bound`)
    /// used by the invalidation buckets and the manifest boundary.
    ///
    /// The boundary depends on the nonce mode, matching `validate_timestamp`:
    /// - **Nonce-bearing** (`nonce_free == false`): the upper bound is
    ///   **inclusive** — valid while `block.timestamp * 1000 <= valid_before` — so
    ///   the last includable second is `floor(valid_before / 1000)`.
    /// - **Nonce-free** (`nonce_free == true`): the nonce-manager replay ring
    ///   admits only a strictly-future `valid_before`, so the bound is
    ///   **exclusive** — valid while `block.timestamp * 1000 < valid_before` — and
    ///   the last includable second is `floor((valid_before - 1) / 1000)`. Without
    ///   the `- 1` a `valid_before` that is an exact multiple of 1000 would linger
    ///   one second past its on-chain expiry.
    const fn tx_valid_before_secs(valid_before: u64, nonce_free: bool) -> u64 {
        if valid_before == 0 {
            u64::MAX
        } else if nonce_free {
            (valid_before - 1) / 1_000
        } else {
            valid_before / 1_000
        }
    }

    fn eip8130_nonce_state(
        &self,
        state: &dyn StateProvider,
        local_chain_id: u64,
        now: u64,
        signed: &Eip8130Signed,
        sender: Address,
        protocol_nonce: u64,
    ) -> Result<(bool, u64), InvalidPoolTransactionError> {
        let nonce_key = signed.tx().nonce_key;
        if nonce_key == Eip8130Constants::NONCE_KEY_MAX {
            return Ok((false, protocol_nonce));
        }
        if nonce_key.is_zero() {
            return Ok((protocol_nonce == 0, protocol_nonce));
        }
        let mut storage = StateProviderPrecompileStorage::new(state, local_chain_id, now);
        StorageCtx::enter(&mut storage, |ctx| {
            NonceManagerStorage::new(ctx)
                .get_nonce(sender, nonce_key)
                .map(|nonce| (nonce == 0, nonce))
                .map_err(|error| Self::precompile_storage_error(error, "nonce manager read failed"))
        })
    }

    fn eip8130_encoded(&self, signed: &Eip8130Signed) -> Vec<u8> {
        let mut encoded = Vec::with_capacity(signed.encode_2718_len());
        signed.encode_2718(&mut encoded);
        encoded
    }

    fn map_tx_auth_error(error: TxAuthError) -> InvalidPoolTransactionError {
        tracing::debug!(error = ?error, "EIP-8130 actor authorization failed");
        let reason = match error {
            TxAuthError::Authenticate(AuthError::MalformedAuth) => "actor authentication malformed",
            TxAuthError::Authenticate(AuthError::NotCanonical(_)) => {
                "actor authenticator is not canonical"
            }
            TxAuthError::Authenticate(AuthError::InvalidSignature) => "actor signature invalid",
            TxAuthError::SenderRecovery => "EOA sender recovery failed",
            TxAuthError::PayerRecovery => "open payer recovery failed",
            TxAuthError::SenderMismatch { .. } => "sender auth does not match the named sender",
            TxAuthError::PayerMismatch { .. } => "payer auth does not match the named payer",
            TxAuthError::Apply(apply) => Self::map_apply_error(apply),
        };
        Self::eip8130_error(reason)
    }

    /// Maps an [`ApplyError`] (surfaced via [`TxAuthError::Apply`] when a
    /// delegation fails to apply) to a named pool-rejection reason. The
    /// structured error is still logged in [`Self::map_tx_auth_error`].
    fn map_apply_error(error: ApplyError) -> &'static str {
        match error {
            ApplyError::Storage(_) => "EIP-8130 state access failed",
            ApplyError::MultipleDelegations => "at most one delegation is allowed",
            ApplyError::NonDelegatableCode { .. } => "delegation sender has non-delegation code",
        }
    }

    fn map_nonce_error(error: NonceError) -> InvalidPoolTransactionError {
        match error {
            NonceError::TooLow { channel, got } | NonceError::TooHigh { channel, got } => {
                InvalidTransactionError::NonceNotConsistent { tx: got, state: channel }.into()
            }
            NonceError::Replay => Self::eip8130_error("nonce-free replay detected"),
            NonceError::Storage(_) => Self::eip8130_error("nonce state read failed"),
        }
    }

    /// Maps an [`Eip8130TimestampError`] (from
    /// [`Eip8130Signed::validate_timestamp`]) to a named pool-rejection reason
    /// and logs the mismatch. This deliberately does *not* collapse into
    /// `TxTypeNotSupported`: the transaction type is supported, its validity
    /// window is simply outside this node's admission window relative to `now`
    /// (the head-block timestamp, in **milliseconds**). Emitting the reason plus
    /// `now`/window bounds here makes the otherwise-silent, node-local rejection
    /// greppable.
    fn map_timestamp_error(
        error: Eip8130TimestampError,
        signed: &Eip8130Signed,
        now: u64,
    ) -> InvalidPoolTransactionError {
        let reason = match error {
            Eip8130TimestampError::NonceFreeMalformed => {
                "nonce-free transaction must set a non-zero valid_before and a zero nonce sequence"
            }
            Eip8130TimestampError::NonceFreeExpired => {
                "nonce-free transaction validity window has elapsed"
            }
            Eip8130TimestampError::NonceFreeExpiryTooFar => {
                "nonce-free transaction validity window exceeds the admission window"
            }
            Eip8130TimestampError::NotYetValid => "transaction is not yet valid",
            Eip8130TimestampError::Expired => "transaction validity window has elapsed",
        };
        let tx = signed.tx();
        // The `window` bound only governs the nonce-free "too far in the future"
        // rejection; logging it for the other variants (e.g. a nonce-bearing
        // `Expired`) would wrongly imply the nonce-free window was involved.
        // Log the *normalized* (millisecond) bounds that `validate_timestamp`
        // actually compared against `now` (also milliseconds); the raw fields may
        // be seconds-denominated, so logging them beside a millisecond `now` reads
        // as decades of skew. The raw values are kept under distinct names for
        // debugging the seconds/milliseconds auto-detection itself.
        if matches!(error, Eip8130TimestampError::NonceFreeExpiryTooFar) {
            tracing::debug!(
                reason,
                now,
                valid_after = tx.valid_after_ms(),
                valid_before = tx.valid_before_ms(),
                valid_after_raw = tx.valid_after,
                valid_before_raw = tx.valid_before,
                nonce_key = %tx.nonce_key,
                window = Eip8130Constants::NONCE_FREE_MAX_EXPIRY_WINDOW,
                "EIP-8130 timestamp validation failed",
            );
        } else {
            tracing::debug!(
                reason,
                now,
                valid_after = tx.valid_after_ms(),
                valid_before = tx.valid_before_ms(),
                valid_after_raw = tx.valid_after,
                valid_before_raw = tx.valid_before,
                nonce_key = %tx.nonce_key,
                "EIP-8130 timestamp validation failed",
            );
        }
        Self::eip8130_error(reason)
    }

    fn eip8130_error(reason: &'static str) -> InvalidPoolTransactionError {
        InvalidPoolTransactionError::other(BaseTxPoolError::Eip8130Validation { reason })
    }

    fn provider_unavailable(error: impl core::fmt::Display) -> InvalidPoolTransactionError {
        tracing::debug!(error = %error, "EIP-8130 state provider unavailable");
        Self::eip8130_error("state provider unavailable")
    }

    fn state_read_error(
        error: impl core::fmt::Display,
        reason: &'static str,
    ) -> InvalidPoolTransactionError {
        tracing::debug!(error = %error, reason = reason, "EIP-8130 state read failed");
        Self::eip8130_error(reason)
    }

    fn precompile_storage_error(
        error: impl core::fmt::Display,
        reason: &'static str,
    ) -> InvalidPoolTransactionError {
        tracing::debug!(error = %error, reason = reason, "EIP-8130 precompile storage read failed");
        Self::eip8130_error(reason)
    }

    /// Runs the mempool admission checks that apply to EIP-8130 (account
    /// abstraction) transactions without requiring authenticator dispatch or account
    /// state lookups. Enforces the Everest fork gate and the structural
    /// invariants listed in EIP-8130 § Validation and § Nonce-Free Mode.
    fn validate_eip8130_structural(
        &self,
        signed: &Eip8130Signed,
    ) -> Result<(), InvalidPoolTransactionError> {
        let size = signed.encode_2718_len();
        let limit = self.inner.max_tx_input_bytes();
        if size > limit {
            return Err(InvalidPoolTransactionError::OversizedData { size, limit });
        }
        if signed.tx().calls.len() > Eip8130Constants::MAX_CALL_PHASES_PER_TX {
            return Err(Self::eip8130_error("call phase count exceeds maximum"));
        }

        // Single read of the head-block timestamp so the fork gate and the
        // expiry check see the same value even when `on_new_head_block` updates
        // the atomic concurrently.
        let now = self.block_timestamp();
        // Fork gate: EIP-8130 (account abstraction) transactions are only
        // admissible to the pool once the Everest upgrade is active.
        if !self.chain_spec().is_everest_active_at_timestamp(now) {
            return Err(InvalidTransactionError::TxTypeNotSupported.into());
        }
        let local_chain_id = self.inner.chain_spec().chain().id();
        signed.validate_static(local_chain_id).map_err(InvalidPoolTransactionError::from)?;
        // The validity window is evaluated in milliseconds against
        // `block.timestamp * 1000`; the fork gate above uses seconds.
        let now_ms = now.saturating_mul(1_000);
        signed
            .validate_timestamp(now_ms)
            .map_err(|error| Self::map_timestamp_error(error, signed, now_ms))?;
        Self::validate_eoa_sender_signature(signed)?;
        Self::validate_sender_auth(signed)?;
        Self::validate_payer_auth(signed)?;
        Self::validate_account_changes(signed, local_chain_id)?;
        Ok(())
    }

    /// Checks the implicit EOA-path signature is recoverable before admitting it
    /// to the pool. Configured-actor transactions are authenticated through their
    /// explicit `authenticator || data` blob and are checked by selector policy.
    fn validate_eoa_sender_signature(
        signed: &Eip8130Signed,
    ) -> Result<(), InvalidPoolTransactionError> {
        if signed.explicit_sender().is_some() {
            return Ok(());
        }
        signed
            .recover_eoa_sender()
            .map_err(|_| Self::eip8130_error("EOA sender signature recovery failed"))?
            .ok_or_else(|| Self::eip8130_error("EOA sender signature recovery failed"))?;
        Ok(())
    }

    /// Checks the `sender_auth` field carries enough bytes for either the EOA
    /// recovery path (65-byte signature) or the configured-actor auth path
    /// (`authenticator_address || authenticator_payload`) and that the authenticator address
    /// is not the sentinel revoked marker.
    fn validate_sender_auth(signed: &Eip8130Signed) -> Result<(), InvalidPoolTransactionError> {
        let auth = signed.sender_auth();
        if auth.is_empty() {
            return Err(InvalidTransactionError::TxTypeNotSupported.into());
        }
        if signed.explicit_sender().is_none() {
            // EOA path: must carry exactly the secp256k1 signature.
            if auth.len() != 65 {
                return Err(InvalidTransactionError::TxTypeNotSupported.into());
            }
        } else {
            // Named-account path: `K1_AUTHENTICATOR(20) || r||s||v(65)`.
            if !Self::named_k1_auth_well_formed(auth) {
                return Err(InvalidTransactionError::TxTypeNotSupported.into());
            }
        }
        Ok(())
    }

    /// Ensures `payer_auth` is present iff a `payer` is set. Open payer mode
    /// carries a raw 65-byte signature; a named payer carries a
    /// `K1_AUTHENTICATOR || r||s||v` blob.
    fn validate_payer_auth(signed: &Eip8130Signed) -> Result<(), InvalidPoolTransactionError> {
        let payer_present = signed.tx().payer.is_some();
        let auth = signed.payer_auth();
        // XOR: presence must match.
        if payer_present == auth.is_empty() {
            return Err(InvalidTransactionError::TxTypeNotSupported.into());
        }
        if signed.tx().is_open_payer() {
            if auth.len() != 65 {
                return Err(InvalidTransactionError::TxTypeNotSupported.into());
            }
        } else if payer_present && !Self::named_k1_auth_well_formed(auth) {
            return Err(InvalidTransactionError::TxTypeNotSupported.into());
        }
        Ok(())
    }

    /// Whether a named-actor `sender_auth`/`payer_auth` blob is a well-formed
    /// `K1_AUTHENTICATOR(20) || r||s||v(65)`: exactly the native secp256k1
    /// authenticator selector followed by a 65-byte signature. Non-k1
    /// authenticators (removed Keystore selectors) are rejected.
    fn named_k1_auth_well_formed(auth: &[u8]) -> bool {
        auth.len() == 85 && Address::from_slice(&auth[..20]) == Eip8130Constants::K1_AUTHENTICATOR
    }

    /// Enforces the per-transaction call-phase and delegation structural
    /// invariants: at most one delegation entry (the only supported account
    /// change on the launch wire).
    fn validate_account_changes(
        signed: &Eip8130Signed,
        _local_chain_id: u64,
    ) -> Result<(), InvalidPoolTransactionError> {
        let mut delegation_count = 0usize;
        for change in &signed.tx().account_changes {
            match change {
                AccountChange::Delegation(_) => {
                    delegation_count += 1;
                    if delegation_count > 1 {
                        return Err(InvalidTransactionError::TxTypeNotSupported.into());
                    }
                }
            }
        }
        Ok(())
    }

    /// Performs the necessary Base-specific checks based on top of the regular eth outcome.
    ///
    /// `operator_fee_gas_addition` is gas charged the operator fee on top of the
    /// transaction's signed `gas_limit`. It is zero for ordinary transactions; for
    /// EIP-8130 it is the payer-authentication gas, because the execution path meters
    /// the operator fee on `gas_limit + payer_auth` (the gas-price portion of that
    /// payer-auth gas is already reflected in the reduced `balance`). Mirroring it here
    /// prevents admitting sponsored transactions that are operator-fee-underfunded and
    /// would never execute.
    fn apply_base_checks(
        &self,
        outcome: TransactionValidationOutcome<Tx>,
        operator_fee_gas_addition: u64,
    ) -> TransactionValidationOutcome<Tx> {
        if !self.requires_l1_data_gas_fee() {
            // no need to check L1 gas fee
            return outcome;
        }
        // ensure that the account has enough balance to cover the L1 gas cost
        if let TransactionValidationOutcome::Valid {
            balance,
            state_nonce,
            transaction: valid_tx,
            propagate,
            bytecode_hash,
            authorities,
        } = outcome
        {
            let mut l1_block_info = self.block_info.l1_block_info.read().clone();

            // Check to ensure tx doesn't exceed the DA footprint limit
            if self.chain_spec().is_jovian_active_at_timestamp(self.block_timestamp()) {
                let da_footprint = valid_tx.transaction().estimated_da_size().saturating_mul(
                    l1_block_info
                        .da_footprint_gas_scalar
                        .unwrap_or(DaFootprintGasScalarUpdate::DEFAULT_DA_FOOTPRINT_GAS_SCALAR)
                        as u64,
                );
                let block_gas_limit = self.inner.block_gas_limit();
                if da_footprint > block_gas_limit {
                    return TransactionValidationOutcome::Invalid(
                        valid_tx.into_transaction(),
                        InvalidPoolTransactionError::other(
                            BaseTxPoolError::DaFootprintExceedsBlockGasLimit {
                                transaction_da_footprint: da_footprint,
                                block_gas_limit,
                            },
                        ),
                    );
                }
            }

            let encoded = valid_tx.transaction().encoded_2718();

            // Must mirror the execution-side cost in `BaseHandler` (L1 data fee + operator fee
            // post-Isthmus); otherwise operator-fee-underfunded txs get admitted but never execute.
            let spec_id = BaseSpecId::from_timestamp(self.chain_spec(), self.block_timestamp());
            let cost_addition = l1_block_info.tx_cost(
                &encoded,
                U256::from(
                    valid_tx.transaction().gas_limit().saturating_add(operator_fee_gas_addition),
                ),
                spec_id,
            );
            let cost = valid_tx.transaction().cost().saturating_add(cost_addition);

            // Checks for max cost
            if cost > balance {
                return TransactionValidationOutcome::Invalid(
                    valid_tx.into_transaction(),
                    InvalidTransactionError::InsufficientFunds(
                        GotExpected { got: balance, expected: cost }.into(),
                    )
                    .into(),
                );
            }

            return TransactionValidationOutcome::Valid {
                balance,
                state_nonce,
                transaction: valid_tx,
                propagate,
                bytecode_hash,
                authorities,
            };
        }
        outcome
    }
}

impl<Client, Tx, Evm> TransactionValidator for BaseTransactionValidator<Client, Tx, Evm>
where
    Client: ChainSpecProvider<ChainSpec: Upgrades> + StateProviderFactory + BlockReaderIdExt + Sync,
    Tx: EthPoolTransaction + BasePooledTx,
    Evm: ConfigureEvm,
{
    type Transaction = Tx;
    type Block = BlockTy<Evm::Primitives>;

    async fn validate_transaction(
        &self,
        origin: TransactionOrigin,
        transaction: Self::Transaction,
    ) -> TransactionValidationOutcome<Self::Transaction> {
        self.validate_one(origin, transaction).await
    }

    fn on_new_head_block(&self, new_tip_block: &SealedBlock<Self::Block>) {
        self.inner.on_new_head_block(new_tip_block);
        self.update_l1_block_info(
            new_tip_block.header(),
            new_tip_block.body().transactions().first(),
        );
    }
}

#[cfg(test)]
mod tests {
    use alloy_consensus::{SignableTransaction, TxEip1559, transaction::SignerRecoverable};
    use alloy_eips::eip2718::Encodable2718;
    use alloy_primitives::{Address, B256, Bytes, TxKind, U256, bytes, hex::decode};
    use alloy_signer::SignerSync;
    use alloy_signer_local::PrivateKeySigner;
    use base_common_chains::ChainConfig;
    use base_common_consensus::{
        AccountChange, BasePrimitives, BaseTransactionSigned, BaseTxEnvelope, Call, Delegation,
        Eip8130Constants, Eip8130Signed, TxDeposit, TxEip8130,
    };
    use base_execution_chainspec::{BaseChainSpec, BaseChainSpecBuilder};
    use base_execution_evm::BaseEvmConfig;
    use base_test_utils::{Account, build_test_genesis_everest};
    use reth_provider::test_utils::{ExtendedAccount, MockEthProvider};
    use reth_transaction_pool::{
        TransactionOrigin, TransactionValidationOutcome, blobstore::InMemoryBlobStore,
        validate::EthTransactionValidatorBuilder,
    };

    use super::*;
    use crate::BasePooledTransaction;

    type TestValidator = BaseTransactionValidator<
        MockEthProvider<BasePrimitives, Arc<BaseChainSpec>>,
        BasePooledTransaction,
        BaseEvmConfig,
    >;

    fn everest_chain_spec() -> Arc<BaseChainSpec> {
        let mut genesis = build_test_genesis_everest();
        genesis.config.chain_id = test_chain_id();
        Arc::new(BaseChainSpec::from_genesis(genesis))
    }

    /// Builds a [`BaseTransactionValidator`] configured against the given chain spec with
    /// no accounts seeded.
    fn build_test_validator_with_spec(chain_spec: Arc<BaseChainSpec>) -> TestValidator {
        let client = MockEthProvider::<BasePrimitives>::new()
            .with_chain_spec(Arc::clone(&chain_spec))
            .with_genesis_block();
        let evm_config = BaseEvmConfig::base(Arc::clone(&chain_spec));
        let inner = EthTransactionValidatorBuilder::new(client, evm_config)
            .no_shanghai()
            .no_cancun()
            .build(InMemoryBlobStore::default());
        BaseTransactionValidator::with_block_info(inner, BaseL1BlockInfo::default())
    }

    /// Builds a [`BaseTransactionValidator`] against an Everest-activated test chain spec with
    /// no accounts seeded. EIP-8130 admission is fork-gated on Everest, so the structural-gate
    /// tests run with Everest active (at genesis) to exercise the checks past the fork gate.
    fn build_test_validator() -> TestValidator {
        build_test_validator_with_spec(everest_chain_spec())
    }

    /// Builds an Everest-activated validator with a custom encoded transaction-size limit.
    fn build_test_validator_with_max_tx_input_bytes(max_tx_input_bytes: usize) -> TestValidator {
        let chain_spec = everest_chain_spec();
        let client = MockEthProvider::<BasePrimitives>::new()
            .with_chain_spec(Arc::clone(&chain_spec))
            .with_genesis_block();
        let evm_config = BaseEvmConfig::base(Arc::clone(&chain_spec));
        let inner = EthTransactionValidatorBuilder::new(client, evm_config)
            .no_shanghai()
            .no_cancun()
            .with_max_tx_input_bytes(max_tx_input_bytes)
            .build(InMemoryBlobStore::default());
        BaseTransactionValidator::with_block_info(inner, BaseL1BlockInfo::default())
    }

    /// Builds an Everest-activated validator with one canonical account seeded.
    fn build_test_validator_with_account(
        address: Address,
        account: ExtendedAccount,
    ) -> TestValidator {
        let chain_spec = everest_chain_spec();
        let client = MockEthProvider::<BasePrimitives>::new()
            .with_chain_spec(Arc::clone(&chain_spec))
            .with_genesis_block();
        client.add_account(address, account);
        let evm_config = BaseEvmConfig::base(Arc::clone(&chain_spec));
        let inner = EthTransactionValidatorBuilder::new(client, evm_config)
            .no_shanghai()
            .no_cancun()
            .build(InMemoryBlobStore::default());
        BaseTransactionValidator::with_block_info(inner, BaseL1BlockInfo::default())
    }

    #[test]
    fn classify_authenticator_uses_bounded_labels() {
        let mut k1 = Eip8130Constants::K1_AUTHENTICATOR.as_slice().to_vec();
        k1.extend_from_slice(&[0; 65]);
        assert_eq!(TestValidator::classify_authenticator(&k1), "k1");
        // Any non-k1 selector (removed Keystore authenticators) is "other".
        let mut other = Address::repeat_byte(0xbb).as_slice().to_vec();
        other.extend_from_slice(&[0; 65]);
        assert_eq!(TestValidator::classify_authenticator(&other), "other");
        assert_eq!(TestValidator::classify_authenticator(&[0; 10]), "other");
    }

    #[test]
    fn sender_sig_type_identifies_eoa_path_as_k1() {
        assert_eq!(TestValidator::sender_sig_type(&sign_eoa_eip8130(minimal_valid_eoa_tx())), "k1");
    }

    /// Returns the chain id the [`build_test_validator`] is configured against.
    fn test_chain_id() -> u64 {
        ChainConfig::mainnet().chain_id
    }

    /// Signs `tx` as an EOA-path EIP-8130 transaction and returns the resulting
    /// [`Eip8130Signed`] with a valid 65-byte secp256k1 `sender_auth`.
    fn sign_eoa_eip8130(tx: TxEip8130) -> Eip8130Signed {
        let signer = PrivateKeySigner::random();
        let signature = signer.sign_hash_sync(&tx.sender_signature_hash()).unwrap();
        let sig_bytes: Bytes = signature.as_bytes().to_vec().into();
        Eip8130Signed::new(tx, sig_bytes, Bytes::new())
    }

    /// Returns a minimal, structurally valid EOA-path [`TxEip8130`] bound to the
    /// test chain. `sender` is left as `None` so the EOA recovery path is exercised.
    fn minimal_valid_eoa_tx() -> TxEip8130 {
        TxEip8130 {
            chain_id: test_chain_id(),
            sender: None,
            nonce_key: U256::ZERO,
            nonce_sequence: 1,
            valid_after: 0,
            valid_before: 0,
            max_priority_fee_per_gas: 0,
            max_fee_per_gas: 1_000,
            gas_limit: 50_000,
            account_changes: Vec::new(),
            calls: Vec::new(),
            metadata: Bytes::new(),
            payer: None,
        }
    }

    /// Helper: assert structural validation returns `Invalid` with `TxTypeNotSupported`.
    #[track_caller]
    fn assert_unsupported(result: Result<(), InvalidPoolTransactionError>) {
        match result {
            Err(InvalidPoolTransactionError::Consensus(
                InvalidTransactionError::TxTypeNotSupported,
            )) => {}
            other => panic!("expected TxTypeNotSupported, got {other:?}"),
        }
    }

    /// Helper: assert structural validation returns `Invalid` with `ChainIdMismatch`.
    #[track_caller]
    fn assert_chain_id_mismatch(result: Result<(), InvalidPoolTransactionError>) {
        match result {
            Err(InvalidPoolTransactionError::Consensus(
                InvalidTransactionError::ChainIdMismatch,
            )) => {}
            other => panic!("expected ChainIdMismatch, got {other:?}"),
        }
    }

    /// Helper: assert structural validation returns `Invalid` with `TipAboveFeeCap`.
    #[track_caller]
    fn assert_tip_above_fee_cap(result: Result<(), InvalidPoolTransactionError>) {
        match result {
            Err(InvalidPoolTransactionError::Consensus(
                InvalidTransactionError::TipAboveFeeCap,
            )) => {}
            other => panic!("expected TipAboveFeeCap, got {other:?}"),
        }
    }

    /// Helper: assert a structural (`()`-returning) validation failed with a
    /// named [`BaseTxPoolError::Eip8130Validation`] reason.
    #[track_caller]
    fn assert_structural_reason(
        result: Result<(), InvalidPoolTransactionError>,
        expected: &'static str,
    ) {
        match result {
            Err(InvalidPoolTransactionError::Other(error)) => {
                match error.as_any().downcast_ref::<BaseTxPoolError>() {
                    Some(BaseTxPoolError::Eip8130Validation { reason }) => {
                        assert_eq!(*reason, expected);
                    }
                    other => panic!("expected Eip8130Validation, got {other:?}"),
                }
            }
            other => panic!("expected Eip8130Validation, got {other:?}"),
        }
    }

    #[track_caller]
    fn assert_eip8130_validation_reason(
        result: Result<Eip8130ValidationState, InvalidPoolTransactionError>,
        expected: &'static str,
    ) {
        match result {
            Err(InvalidPoolTransactionError::Other(error)) => {
                match error.as_any().downcast_ref::<BaseTxPoolError>() {
                    Some(BaseTxPoolError::Eip8130Validation { reason }) => {
                        assert_eq!(*reason, expected);
                    }
                    other => panic!("expected Eip8130Validation, got {other:?}"),
                }
            }
            other => panic!("expected Eip8130Validation, got {other:?}"),
        }
    }

    #[test]
    fn accepts_eip8130_with_minimum_valid_eoa_shape() {
        let validator = build_test_validator();
        let signed = sign_eoa_eip8130(minimal_valid_eoa_tx());
        assert!(validator.validate_eip8130_structural(&signed).is_ok());
    }

    #[test]
    fn accepts_eip8130_at_encoded_size_limit() {
        let signed = sign_eoa_eip8130(minimal_valid_eoa_tx());
        let validator = build_test_validator_with_max_tx_input_bytes(signed.encode_2718_len());

        assert!(validator.validate_eip8130_structural(&signed).is_ok());
    }

    #[test]
    fn rejects_eip8130_over_encoded_size_limit() {
        let signed = sign_eoa_eip8130(minimal_valid_eoa_tx());
        let size = signed.encode_2718_len();
        let limit = size - 1;
        let validator = build_test_validator_with_max_tx_input_bytes(limit);

        assert!(matches!(
            validator.validate_eip8130_structural(&signed),
            Err(InvalidPoolTransactionError::OversizedData {
                size: rejected_size,
                limit: rejected_limit,
            }) if rejected_size == size && rejected_limit == limit
        ));
    }

    #[test]
    fn rejects_constructed_eip8130_over_call_phase_limit() {
        let validator = build_test_validator();
        let tx = TxEip8130 {
            calls: vec![Vec::new(); Eip8130Constants::MAX_CALL_PHASES_PER_TX + 1],
            ..minimal_valid_eoa_tx()
        };
        let signed = sign_eoa_eip8130(tx);

        assert_structural_reason(
            validator.validate_eip8130_structural(&signed),
            "call phase count exceeds maximum",
        );
    }

    #[test]
    fn rejects_eip8130_before_everest_activation() {
        // Cobalt alone does not open the EIP-8130 gate.
        let chain_spec = BaseChainSpecBuilder::base_mainnet().cobalt_activated().build();
        let validator = build_test_validator_with_spec(Arc::new(chain_spec));
        let signed = sign_eoa_eip8130(minimal_valid_eoa_tx());
        assert_unsupported(validator.validate_eip8130_structural(&signed));
    }

    #[test]
    fn structural_eip8130_validation_is_origin_independent() {
        let validator = build_test_validator();
        let signed = sign_eoa_eip8130(minimal_valid_eoa_tx());
        assert!(validator.validate_eip8130_structural(&signed).is_ok());
    }

    #[test]
    fn rejects_eip8130_with_wrong_chain_id() {
        let validator = build_test_validator();
        let tx = TxEip8130 { chain_id: test_chain_id() + 1, ..minimal_valid_eoa_tx() };
        let signed = sign_eoa_eip8130(tx);
        assert_chain_id_mismatch(validator.validate_eip8130_structural(&signed));
    }

    #[test]
    fn rejects_eip8130_with_tip_above_fee_cap() {
        let validator = build_test_validator();
        let tx = TxEip8130 {
            max_fee_per_gas: 100,
            max_priority_fee_per_gas: 200,
            ..minimal_valid_eoa_tx()
        };
        let signed = sign_eoa_eip8130(tx);
        assert_tip_above_fee_cap(validator.validate_eip8130_structural(&signed));
    }

    #[test]
    fn rejects_eip8130_with_zero_gas_limit() {
        let validator = build_test_validator();
        let tx = TxEip8130 { gas_limit: 0, ..minimal_valid_eoa_tx() };
        let signed = sign_eoa_eip8130(tx);
        assert_unsupported(validator.validate_eip8130_structural(&signed));
    }

    #[test]
    fn rejects_eip8130_with_zero_fee_cap() {
        let validator = build_test_validator();
        let tx = TxEip8130 { max_fee_per_gas: 0, ..minimal_valid_eoa_tx() };
        let signed = sign_eoa_eip8130(tx);
        assert_unsupported(validator.validate_eip8130_structural(&signed));
    }

    #[test]
    fn rejects_eip8130_nonce_free_without_expiry() {
        let validator = build_test_validator();
        let tx = TxEip8130 {
            nonce_key: Eip8130Constants::NONCE_KEY_MAX,
            nonce_sequence: 0,
            valid_before: 0,
            ..minimal_valid_eoa_tx()
        };
        let signed = sign_eoa_eip8130(tx);
        assert_structural_reason(
            validator.validate_eip8130_structural(&signed),
            "nonce-free transaction must set a non-zero valid_before and a zero nonce sequence",
        );
    }

    #[test]
    fn rejects_eip8130_nonce_free_with_nonzero_sequence() {
        let validator = build_test_validator();
        let tx = TxEip8130 {
            nonce_key: Eip8130Constants::NONCE_KEY_MAX,
            nonce_sequence: 1,
            valid_before: 5,
            ..minimal_valid_eoa_tx()
        };
        let signed = sign_eoa_eip8130(tx);
        assert_structural_reason(
            validator.validate_eip8130_structural(&signed),
            "nonce-free transaction must set a non-zero valid_before and a zero nonce sequence",
        );
    }

    #[test]
    fn rejects_eip8130_nonce_free_already_expired() {
        // Advance the validator's tracked block timestamp to a realistic clock
        // (now_ms = 1_700_000_100_000) so `valid_before` (a millisecond value one
        // second in the past) is strictly elapsed. Both values are >=
        // TIMESTAMP_MS_THRESHOLD, so normalization is a no-op and this exercises
        // the raw expiry comparison.
        let validator = build_test_validator();
        let header = alloy_consensus::Header { timestamp: 1_700_000_100, ..Default::default() };
        validator.update_l1_block_info::<_, TxEip1559>(&header, None);
        let tx = TxEip8130 {
            nonce_key: Eip8130Constants::NONCE_KEY_MAX,
            nonce_sequence: 0,
            valid_before: 1_700_000_099_000,
            ..minimal_valid_eoa_tx()
        };
        let signed = sign_eoa_eip8130(tx);
        assert_structural_reason(
            validator.validate_eip8130_structural(&signed),
            "nonce-free transaction validity window has elapsed",
        );
    }

    #[test]
    fn rejects_eip8130_nonce_free_not_yet_valid() {
        // Realistic millisecond clock (both bounds >= TIMESTAMP_MS_THRESHOLD, so
        // normalization is a no-op). A future `valid_after` opens the window
        // later, so the nonce-free branch must reject with `NotYetValid` before
        // the expiry checks (which `validate_timestamp` evaluates afterward).
        let validator = build_test_validator();
        let header = alloy_consensus::Header { timestamp: 1_700_000_000, ..Default::default() };
        validator.update_l1_block_info::<_, TxEip1559>(&header, None);
        let now_ms = 1_700_000_000_000;
        let tx = TxEip8130 {
            nonce_key: Eip8130Constants::NONCE_KEY_MAX,
            nonce_sequence: 0,
            valid_after: now_ms + 50_000,
            valid_before: now_ms + 60_000,
            ..minimal_valid_eoa_tx()
        };
        let signed = sign_eoa_eip8130(tx);
        assert_structural_reason(
            validator.validate_eip8130_structural(&signed),
            "transaction is not yet valid",
        );
    }

    #[test]
    fn rejects_eip8130_nonce_bearing_not_yet_valid() {
        // Sequenced (nonce-bearing) transaction on a realistic millisecond clock
        // with a future `valid_after` (>= TIMESTAMP_MS_THRESHOLD, so normalization
        // is a no-op): the else-branch of `validate_timestamp` must reject with
        // `NotYetValid`.
        let validator = build_test_validator();
        let header = alloy_consensus::Header { timestamp: 1_700_000_000, ..Default::default() };
        validator.update_l1_block_info::<_, TxEip1559>(&header, None);
        let now_ms = 1_700_000_000_000;
        let tx = TxEip8130 { valid_after: now_ms + 50_000, ..minimal_valid_eoa_tx() };
        let signed = sign_eoa_eip8130(tx);
        assert_structural_reason(
            validator.validate_eip8130_structural(&signed),
            "transaction is not yet valid",
        );
    }

    #[test]
    fn rejects_eip8130_nonce_free_expiry_too_far_in_future() {
        // Seed a realistic millisecond clock and place `valid_before` exactly one
        // millisecond past the admission-window edge (`now_ms +
        // NONCE_FREE_MAX_EXPIRY_WINDOW`). Both bounds are >= TIMESTAMP_MS_THRESHOLD
        // so normalization is a no-op; this exercises the true edge+1 rejection,
        // the mirror of `accepts_eip8130_nonce_free_at_expiry_window_edge`.
        let validator = build_test_validator();
        let header = alloy_consensus::Header { timestamp: 1_700_000_000, ..Default::default() };
        validator.update_l1_block_info::<_, TxEip1559>(&header, None);
        let now_ms = 1_700_000_000_000;
        let tx = TxEip8130 {
            nonce_key: Eip8130Constants::NONCE_KEY_MAX,
            nonce_sequence: 0,
            valid_before: now_ms + Eip8130Constants::NONCE_FREE_MAX_EXPIRY_WINDOW + 1,
            ..minimal_valid_eoa_tx()
        };
        let signed = sign_eoa_eip8130(tx);
        assert_structural_reason(
            validator.validate_eip8130_structural(&signed),
            "nonce-free transaction validity window exceeds the admission window",
        );
    }

    #[test]
    fn accepts_eip8130_nonce_free_at_expiry_window_edge() {
        // Seed a realistic millisecond clock and place `valid_before` exactly at
        // the admission-window edge (`now_ms + NONCE_FREE_MAX_EXPIRY_WINDOW`).
        // Both bounds are >= TIMESTAMP_MS_THRESHOLD so normalization is a no-op;
        // this checks the inclusive edge in true milliseconds.
        let validator = build_test_validator();
        let header = alloy_consensus::Header { timestamp: 1_700_000_000, ..Default::default() };
        validator.update_l1_block_info::<_, TxEip1559>(&header, None);
        let now_ms = 1_700_000_000_000;
        let tx = TxEip8130 {
            nonce_key: Eip8130Constants::NONCE_KEY_MAX,
            nonce_sequence: 0,
            valid_before: now_ms + Eip8130Constants::NONCE_FREE_MAX_EXPIRY_WINDOW,
            ..minimal_valid_eoa_tx()
        };
        let signed = sign_eoa_eip8130(tx);
        assert!(validator.validate_eip8130_structural(&signed).is_ok());
    }

    /// The mempool pre-filter window must never exceed the authoritative,
    /// consensus-critical on-chain inclusion window. If it did, the pool would
    /// admit nonce-free transactions whose `expiry` the block-inclusion replay
    /// check (`NonceManagerStorage::check_and_mark_expiring_nonce`) rejects,
    /// wasting block space on transactions that can never land. See the note on
    /// `Eip8130Constants::NONCE_FREE_MAX_EXPIRY_WINDOW`.
    #[test]
    fn mempool_expiry_window_within_onchain_inclusion_window() {
        const {
            assert!(
                Eip8130Constants::NONCE_FREE_MAX_EXPIRY_WINDOW
                    <= NonceManagerStorage::NONCE_FREE_EXPIRY_WINDOW,
                "mempool expiry window exceeds the on-chain inclusion window; raising it is a \
                 fork-level change (bump NONCE_FREE_EXPIRY_WINDOW and resize \
                 REPLAY_BUFFER_CAPACITY)",
            );
        }
    }

    #[test]
    fn rejects_eip8130_with_invalid_sender_auth_length_eoa_path() {
        // EOA path requires exactly 65 bytes; anything else is rejected.
        let tx = minimal_valid_eoa_tx();
        let signed = Eip8130Signed::new(tx, Bytes::from_static(&[0u8; 32]), Bytes::new());
        assert_unsupported(TestValidator::validate_sender_auth(&signed));
    }

    #[test]
    fn rejects_eip8130_with_empty_sender_auth() {
        let tx = minimal_valid_eoa_tx();
        let signed = Eip8130Signed::new(tx, Bytes::new(), Bytes::new());
        assert_unsupported(TestValidator::validate_sender_auth(&signed));
    }

    // Regression: configured-actor path must reject the reserved authenticator
    // range below `K1_AUTHENTICATOR`, matching `validate_actor_changes`.
    // `address(0)` is the only reserved value (the empty sentinel).
    #[test]
    fn rejects_eip8130_configured_actor_with_reserved_authenticator() {
        let tx = TxEip8130 { sender: Some(Address::repeat_byte(0xaa)), ..minimal_valid_eoa_tx() };
        let auth = Bytes::from(Address::ZERO.to_vec());
        let signed = Eip8130Signed::new(tx, auth, Bytes::new());
        assert_unsupported(TestValidator::validate_sender_auth(&signed));
    }

    #[test]
    fn rejects_eip8130_configured_actor_with_short_auth() {
        let tx = TxEip8130 { sender: Some(Address::repeat_byte(0xaa)), ..minimal_valid_eoa_tx() };
        let signed = Eip8130Signed::new(tx, Bytes::from_static(&[0u8; 5]), Bytes::new());
        assert_unsupported(TestValidator::validate_sender_auth(&signed));
    }

    #[test]
    fn rejects_eip8130_payer_present_without_auth() {
        let tx = TxEip8130 { payer: Some(Address::repeat_byte(0x11)), ..minimal_valid_eoa_tx() };
        let signed = Eip8130Signed::new(tx, Bytes::from_static(&[0u8; 65]), Bytes::new());
        assert_unsupported(TestValidator::validate_payer_auth(&signed));
    }

    #[test]
    fn rejects_eip8130_payer_absent_with_auth() {
        let tx = minimal_valid_eoa_tx();
        let signed =
            Eip8130Signed::new(tx, Bytes::from_static(&[0u8; 65]), Bytes::from_static(&[0u8; 20]));
        assert_unsupported(TestValidator::validate_payer_auth(&signed));
    }

    #[test]
    fn rejects_eip8130_payer_authenticator_reserved() {
        let tx = TxEip8130 { payer: Some(Address::repeat_byte(0x11)), ..minimal_valid_eoa_tx() };
        let signed = Eip8130Signed::new(
            tx,
            Bytes::from_static(&[0u8; 65]),
            Bytes::from(Address::ZERO.to_vec()),
        );
        assert_unsupported(TestValidator::validate_payer_auth(&signed));
    }

    /// A configured sender naming a non-k1 authenticator is rejected at
    /// admission (only the native secp256k1 authenticator is accepted).
    #[test]
    fn rejects_eip8130_non_k1_sender_authenticator() {
        let tx = TxEip8130 { sender: Some(Address::repeat_byte(0xaa)), ..minimal_valid_eoa_tx() };
        let mut auth = Address::repeat_byte(0x99).as_slice().to_vec();
        auth.extend_from_slice(&[0u8; 65]);
        let signed = Eip8130Signed::new(tx, Bytes::from(auth), Bytes::new());
        assert_unsupported(TestValidator::validate_sender_auth(&signed));
    }

    #[test]
    fn accepts_eip8130_named_k1_sender_authenticator() {
        let tx = TxEip8130 { sender: Some(Address::repeat_byte(0xaa)), ..minimal_valid_eoa_tx() };
        let mut auth = Eip8130Constants::K1_AUTHENTICATOR.as_slice().to_vec();
        auth.extend_from_slice(&[0u8; 65]);
        let signed = Eip8130Signed::new(tx, Bytes::from(auth), Bytes::new());
        assert!(TestValidator::validate_sender_auth(&signed).is_ok());
    }

    #[test]
    fn rejects_eip8130_multiple_delegations() {
        let tx = TxEip8130 {
            account_changes: vec![
                AccountChange::Delegation(Delegation { target: Address::repeat_byte(0x11) }),
                AccountChange::Delegation(Delegation { target: Address::repeat_byte(0x22) }),
            ],
            ..minimal_valid_eoa_tx()
        };
        assert_unsupported(TestValidator::validate_account_changes(
            &sign_eoa_eip8130(tx),
            test_chain_id(),
        ));
    }

    /// L1 attribute deposit calldata that activates Isthmus and seeds a non-zero
    /// `operator_fee_scalar`/`operator_fee_constant`. Mirrors the fixture used by
    /// `parse_l1_info_isthmus` in `crates/execution/evm/src/l1.rs`.
    const ISTHMUS_L1_INFO_DATA_HEX: &str = concat!(
        "098999be00000558000c5fc500000000000000030000000067a9f765",
        "0000000000000029000000000000000000000000000000000000000000000000",
        "00000000006a6d090000000000000000000000000000000000000000000000000000000000000001",
        "72fcc8e8886636bdbe96ba0e4baab67ea7e7811633f52b52e8cf7a5123213b6f",
        "000000000000000000000000d3f2c5afb2d76f5579f326b0cd7da5f5a4126c35",
        "00004e2000000000000001f4",
    );

    /// Regression test for `HackerOne` #74725.
    ///
    /// Asserts that the txpool affordability check accounts for the post-Isthmus operator fee, so a
    /// sender funded only for `tx.cost + l1_data_fee` (but not the additional operator fee) is
    /// rejected at admission instead of being accepted and later failing during execution with
    /// `LackOfFundForMaxFee`.
    #[tokio::test]
    async fn rejects_tx_underfunded_for_operator_fee_post_isthmus() {
        let chain_config = ChainConfig::mainnet();
        let chain_spec = Arc::new(BaseChainSpec::mainnet());

        let signer = Account::Alice.signer();
        let sender = signer.address();
        let tx = TxEip1559 {
            chain_id: chain_config.chain_id,
            nonce: 0,
            gas_limit: 50_000,
            max_fee_per_gas: 1_000,
            max_priority_fee_per_gas: 0,
            to: TxKind::Call(Address::random()),
            value: U256::ZERO,
            access_list: Default::default(),
            input: bytes!("FACADE"),
        };
        let gas_limit = tx.gas_limit;
        let signature = signer.sign_hash_sync(&tx.signature_hash()).unwrap();
        let envelope = BaseTxEnvelope::Eip1559(tx.into_signed(signature));
        let recovered_tx = envelope.clone().try_into_recovered().unwrap();
        let encoded = recovered_tx.encoded_2718();

        let isthmus_data = decode(ISTHMUS_L1_INFO_DATA_HEX).expect("valid hex fixture");
        let mut l1_block_info = base_execution_evm::parse_l1_info(&isthmus_data).unwrap();
        let l1_only_cost = base_execution_evm::RethL1BlockInfo::l1_tx_data_fee(
            &mut l1_block_info,
            Arc::clone(&chain_spec),
            chain_config.isthmus_timestamp,
            &encoded,
            false,
        )
        .unwrap();
        let full_additional_cost = l1_block_info.tx_cost(
            &encoded,
            U256::from(gas_limit),
            BaseSpecId::from_timestamp(Arc::clone(&chain_spec), chain_config.isthmus_timestamp),
        );
        let base_tx_cost = U256::from(envelope.value()).saturating_add(U256::from(
            envelope.max_fee_per_gas().saturating_mul(envelope.gas_limit() as u128),
        ));
        let balance = base_tx_cost.saturating_add(l1_only_cost);

        assert!(
            full_additional_cost > l1_only_cost,
            "fixture must produce a non-zero operator fee post-Isthmus"
        );
        assert!(
            base_tx_cost.saturating_add(full_additional_cost) > balance,
            "balance must be insufficient once the operator fee is included"
        );

        let client = MockEthProvider::<BasePrimitives>::new()
            .with_chain_spec(Arc::clone(&chain_spec))
            .with_genesis_block();
        client.add_account(sender, ExtendedAccount::new(0, balance));
        let evm_config = BaseEvmConfig::base(Arc::clone(&chain_spec));
        let inner = EthTransactionValidatorBuilder::new(client, evm_config)
            .no_shanghai()
            .no_cancun()
            .build(InMemoryBlobStore::default());
        let validator =
            BaseTransactionValidator::with_block_info(inner, BaseL1BlockInfo::default());

        let header = alloy_consensus::Header {
            timestamp: chain_config.isthmus_timestamp,
            ..Default::default()
        };
        let l1_info_tx: BaseTransactionSigned = TxDeposit {
            source_hash: Default::default(),
            from: Address::ZERO,
            to: TxKind::Create,
            mint: 0,
            value: U256::ZERO,
            gas_limit: 0,
            is_system_transaction: false,
            input: isthmus_data.into(),
        }
        .into();
        validator.update_l1_block_info(&header, Some(&l1_info_tx));

        let pooled_tx: BasePooledTransaction =
            BasePooledTransaction::new(recovered_tx, envelope.encode_2718_len());
        let outcome = validator.validate_one(TransactionOrigin::External, pooled_tx).await;

        match outcome {
            TransactionValidationOutcome::Invalid(_, err) => {
                assert!(
                    matches!(
                        err,
                        InvalidPoolTransactionError::Consensus(
                            InvalidTransactionError::InsufficientFunds(_)
                        )
                    ),
                    "expected InsufficientFunds, got: {err:?}"
                );
            }
            other => panic!(
                "expected operator-fee-underfunded tx to be rejected at admission, got {other:?}"
            ),
        }
    }

    #[test]
    fn eip8130_payer_max_cost_includes_l1_and_operator_fees() {
        let chain_config = ChainConfig::mainnet();
        let chain_spec = everest_chain_spec();
        let signer = PrivateKeySigner::random();
        let sender = signer.address();
        let tx = TxEip8130 { gas_limit: 100_000, ..minimal_valid_eoa_tx() };
        let signature = signer.sign_hash_sync(&tx.sender_signature_hash()).unwrap();
        let signed =
            Eip8130Signed::new(tx, Bytes::from(signature.as_bytes().to_vec()), Bytes::new());

        let client = MockEthProvider::<BasePrimitives>::new()
            .with_chain_spec(Arc::clone(&chain_spec))
            .with_genesis_block();
        client
            .add_account(sender, ExtendedAccount::new(0, U256::from(1_000_000_000_000_000_000u64)));
        let evm_config = BaseEvmConfig::base(Arc::clone(&chain_spec));
        let inner = EthTransactionValidatorBuilder::new(client, evm_config)
            .no_shanghai()
            .no_cancun()
            .build(InMemoryBlobStore::default());
        let validator: TestValidator =
            BaseTransactionValidator::with_block_info(inner, BaseL1BlockInfo::default());

        let isthmus_data = decode(ISTHMUS_L1_INFO_DATA_HEX).expect("valid hex fixture");
        let header = alloy_consensus::Header {
            timestamp: chain_config.isthmus_timestamp,
            ..Default::default()
        };
        let l1_info_tx: BaseTransactionSigned = TxDeposit {
            source_hash: Default::default(),
            from: Address::ZERO,
            to: TxKind::Create,
            mint: 0,
            value: U256::ZERO,
            gas_limit: 0,
            is_system_transaction: false,
            input: isthmus_data.clone().into(),
        }
        .into();
        validator.update_l1_block_info(&header, Some(&l1_info_tx));

        let state = validator.validate_eip8130_full(&signed).expect("valid funded EIP-8130 tx");
        let encoded = validator.eip8130_encoded(&signed);
        let max_gas = FeeCheck::max_chargeable_gas(signed.tx().gas_limit, state.payer_auth);
        let gas_charge = FeeCheck::max_fee_charge(
            signed.tx().gas_limit,
            state.payer_auth,
            signed.tx().max_fee_per_gas,
        );
        let spec_id = BaseSpecId::from_timestamp(&chain_spec, chain_config.isthmus_timestamp);
        let mut l1_block_info = base_execution_evm::parse_l1_info(&isthmus_data).unwrap();
        let additional_fees = l1_block_info.tx_cost(&encoded, U256::from(max_gas), spec_id);

        assert!(!additional_fees.is_zero(), "fixture must charge L1/operator fees");
        assert_eq!(state.payer_max_cost, gas_charge.saturating_add(additional_fees));
        assert_eq!(state.manifest.payer_max_cost(), state.payer_max_cost);
    }

    /// A self-paying sender reserves its call value on top of gas, and is
    /// rejected when its balance cannot cover both.
    #[test]
    fn eip8130_self_pay_reserves_call_value() {
        const BALANCE: u64 = 1_000_000_000_000;
        let signer = PrivateKeySigner::random();
        let sender = signer.address();
        let recipient = Address::repeat_byte(0xee);
        let signed_with_value = |value: u64| {
            let tx = TxEip8130 {
                gas_limit: 100_000,
                calls: vec![vec![Call {
                    to: recipient,
                    value: U256::from(value),
                    data: Bytes::new(),
                }]],
                ..minimal_valid_eoa_tx()
            };
            let signature = signer.sign_hash_sync(&tx.sender_signature_hash()).unwrap();
            Eip8130Signed::new(tx, Bytes::from(signature.as_bytes().to_vec()), Bytes::new())
        };
        let validator =
            build_test_validator_with_account(sender, ExtendedAccount::new(0, U256::from(BALANCE)));

        let without_value = validator
            .validate_eip8130_full(&signed_with_value(0))
            .expect("gas alone is affordable");
        let with_value = validator
            .validate_eip8130_full(&signed_with_value(BALANCE / 2))
            .expect("gas plus half the balance is affordable");
        assert_eq!(
            with_value.payer_max_cost - without_value.payer_max_cost,
            U256::from(BALANCE / 2),
            "the call value is reserved on top of gas"
        );
        assert_eq!(with_value.manifest.payer_max_cost(), with_value.payer_max_cost);

        let err = validator
            .validate_eip8130_full(&signed_with_value(BALANCE))
            .expect_err("gas plus the whole balance is not affordable");
        assert!(
            matches!(
                err,
                InvalidPoolTransactionError::Consensus(InvalidTransactionError::InsufficientFunds(
                    _
                ))
            ),
            "expected InsufficientFunds, got {err:?}"
        );
    }

    #[test]
    fn nonce_free_manifest_uses_transaction_validity_window() {
        let chain_spec = everest_chain_spec();
        let signer = PrivateKeySigner::random();
        // Realistic seconds clock; `now * 1000` stays >= TIMESTAMP_MS_THRESHOLD so
        // the millisecond `valid_before` below is not re-scaled by normalization.
        let now = 1_700_000_000;
        // `valid_before` is in milliseconds; at the admission-window edge it is
        // `now * 1000 + NONCE_FREE_MAX_EXPIRY_WINDOW`. The on-chain bound is
        // exclusive, so the manifest boundary folds it onto the seconds axis as
        // `floor((valid_before - 1) / 1000)`.
        let valid_before = now * 1000 + Eip8130Constants::NONCE_FREE_MAX_EXPIRY_WINDOW;
        let tx = TxEip8130 {
            nonce_key: Eip8130Constants::NONCE_KEY_MAX,
            nonce_sequence: 0,
            valid_before,
            ..minimal_valid_eoa_tx()
        };
        let signature = signer.sign_hash_sync(&tx.sender_signature_hash()).unwrap();
        let signed =
            Eip8130Signed::new(tx, Bytes::from(signature.as_bytes().to_vec()), Bytes::new());

        let client = MockEthProvider::<BasePrimitives>::new()
            .with_chain_spec(Arc::clone(&chain_spec))
            .with_genesis_block();
        client.add_account(
            signer.address(),
            ExtendedAccount::new(0, U256::from(1_000_000_000_000_000_000u64)),
        );
        let evm_config = BaseEvmConfig::base(Arc::clone(&chain_spec));
        let inner = EthTransactionValidatorBuilder::new(client, evm_config)
            .no_shanghai()
            .no_cancun()
            .build(InMemoryBlobStore::default());
        let validator: TestValidator =
            BaseTransactionValidator::with_block_info(inner, BaseL1BlockInfo::default());
        let header = alloy_consensus::Header { timestamp: now, ..Default::default() };
        validator.update_l1_block_info::<_, TxEip1559>(&header, None);

        let state = validator.validate_eip8130_full(&signed).expect("valid nonce-free tx");
        assert_eq!(state.manifest.effective_expiry(), (valid_before - 1) / 1000);
    }

    /// Builds a K1 authenticator-prefixed auth blob (`K1(20) || r || s || v`,
    /// `v` in `{27, 28}`, low-s) over `hash` for the configured-actor wire form.
    fn k1_auth_blob(signer: &PrivateKeySigner, hash: B256) -> Bytes {
        let sig = signer.sign_hash_sync(&hash).unwrap();
        let mut out = Vec::with_capacity(20 + 65);
        out.extend_from_slice(Eip8130Constants::K1_AUTHENTICATOR.as_slice());
        out.extend_from_slice(&sig.r().to_be_bytes::<32>());
        out.extend_from_slice(&sig.s().to_be_bytes::<32>());
        out.push(27 + u8::from(sig.v()));
        Bytes::from(out)
    }

    fn delegation_indicator(target: Address) -> Bytes {
        let mut code = Vec::with_capacity(Eip8130Constants::DELEGATION_INDICATOR_SIZE);
        code.extend_from_slice(&Eip8130Constants::DELEGATION_INDICATOR_PREFIX);
        code.extend_from_slice(target.as_slice());
        Bytes::from(code)
    }

    fn delegation_validation_fixture(
        signer: &PrivateKeySigner,
        existing_code: Option<Bytes>,
    ) -> (TestValidator, Eip8130Signed, Address) {
        let sender = signer.address();
        let tx = TxEip8130 {
            chain_id: test_chain_id(),
            sender: None,
            nonce_key: U256::ZERO,
            nonce_sequence: 0,
            valid_after: 0,
            valid_before: 0,
            max_priority_fee_per_gas: 0,
            max_fee_per_gas: 100,
            gas_limit: 1_000_000,
            account_changes: vec![AccountChange::Delegation(Delegation {
                target: Address::repeat_byte(0x22),
            })],
            calls: Vec::new(),
            metadata: Bytes::new(),
            payer: None,
        };
        let sender_auth = k1_auth_blob(signer, tx.sender_signature_hash()).slice(20..);
        let signed = Eip8130Signed::new(tx, sender_auth, Bytes::new());

        let mut account = ExtendedAccount::new(0, U256::from(1_000_000_000_000_000_000u64));
        if let Some(code) = existing_code {
            account = account.with_bytecode(code);
        }
        let validator = build_test_validator_with_account(sender, account);
        (validator, signed, sender)
    }

    #[test]
    fn rejects_delegation_over_ordinary_code_via_overlay_install() {
        let signer = PrivateKeySigner::random();
        let (validator, signed, sender) =
            delegation_validation_fixture(&signer, Some(Bytes::from_static(&[0x60, 0x00])));
        assert_eq!(sender, signer.address());

        assert_eip8130_validation_reason(
            validator.validate_eip8130_full(&signed),
            "delegation sender has non-delegation code",
        );
    }

    #[test]
    fn admits_delegation_over_empty_code_via_overlay_install() {
        let signer = PrivateKeySigner::random();
        let (validator, signed, sender) = delegation_validation_fixture(&signer, None);
        assert_eq!(sender, signer.address());

        let state = validator
            .validate_eip8130_full(&signed)
            .expect("empty sender code must accept delegation");
        assert_eq!(state.sender, sender);
        assert_eq!(state.payer, sender);
        assert_eq!(state.sender_bytecode_hash, None);
        assert!(state.watch_set.iter().any(|key| *key == InvalidationKey::CodeHash(sender)));
    }

    #[test]
    fn admits_delegation_update_over_existing_indicator_via_overlay_install() {
        let signer = PrivateKeySigner::random();
        let existing_code = delegation_indicator(Address::repeat_byte(0x11));
        let expected_hash = alloy_primitives::keccak256(&existing_code);
        let (validator, signed, sender) =
            delegation_validation_fixture(&signer, Some(existing_code));
        assert_eq!(sender, signer.address());

        let state = validator
            .validate_eip8130_full(&signed)
            .expect("existing delegation indicator must accept a target update");
        assert_eq!(state.sender, sender);
        assert_eq!(state.payer, sender);
        assert_eq!(state.sender_bytecode_hash, Some(expected_hash));
        assert!(state.watch_set.iter().any(|key| *key == InvalidationKey::CodeHash(sender)));
    }
}
