use std::{
    any::Any,
    sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
    },
    time::Instant,
};

use alloy_consensus::{BlockHeader, Transaction, constants::KECCAK_EMPTY};
use alloy_eips::eip2718::Encodable2718;
use alloy_primitives::{Address, B256, LogData, U256, map::AddressSet};
use base_common_chains::{BaseFeature, Upgrades};
use base_common_consensus::{
    AccountChange, BaseTimeUpdateTx, TxBaseConstants, TxBaseSigned, TxBaseStructuralError,
    TxBaseStructure, TxBaseTimestampError,
};
use base_common_evm::{BaseSpecId, L1BlockInfo};
use base_common_genesis::DaFootprintGasScalarUpdate;
use base_common_precompiles::NonceManagerStorage;
use base_execution_base_tx::{
    DelegationError, FeeCheck, IntrinsicGas, NonceError, NonceMode, NonceValidator, ResolvedSender,
    TransactionAuthorizer, TxAuthError,
};
use base_precompile_storage::{
    BasePrecompileError, PrecompileStorageProvider, StorageCtx, validate_loaded_code_presence,
};
use parking_lot::RwLock;
use reth_chainspec::{ChainSpecProvider, EthChainSpec};
use reth_evm::ConfigureEvm;
use reth_primitives_traits::{
    Block, BlockBody, BlockTy, GotExpected, SealedBlock, constants::MAX_TX_GAS_LIMIT_OSAKA,
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

use crate::{
    BasePooledTx, InvalidationKey, LimitClass, UnixMillis, ValidatorMetrics, WatchManifest,
    WatchSet,
};

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
    /// The transaction failed Base-transaction-specific stateful validation.
    #[error("Base transaction validation failed: {reason}")]
    TxBaseValidation {
        /// Static validation label for the failure.
        reason: &'static str,
    },
    /// The transaction carries validity predicates but is an EIP-7702
    /// transaction or comes from an EIP-7702 delegated sender.
    #[error("validity predicates are not allowed on EIP-7702 transactions or delegated senders")]
    DelegatedValidityTransaction,
}

/// Resolved Base transaction sender, payer and state data required to build the pool outcome.
#[derive(Debug, Clone)]
struct TxBaseValidationState {
    sender: Address,
    payer: Address,
    classification_generation: u64,
    payer_balance: U256,
    /// The sender's balance, which reth's per-sender accounting checks the
    /// sender's `cost()` against. The payer's balance is enforced by admission
    /// and the guard's payer balance watch.
    sender_balance: U256,
    sender_nonce: u64,
    /// The sender's code hash when the sender is EIP-7702 delegated, the only
    /// sender code reth's delegation slot limit is meant for; `None` otherwise.
    sender_bytecode_hash: Option<B256>,
    watch_set: WatchSet,
    payer_allowlisted: bool,
    payer_max_cost: U256,
    /// Predicates used for build-time revalidation.
    manifest: WatchManifest,
}

// The pool must never admit a nonce-free transaction whose `valid_before` the
// consensus inclusion check would reject.
const _: () = assert!(
    TxBaseConstants::NONCE_FREE_MAX_EXPIRY_WINDOW <= NonceManagerStorage::NONCE_FREE_EXPIRY_WINDOW,
    "mempool nonce-free expiry window exceeds the consensus inclusion window",
);

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
    /// Current block timestamp in milliseconds.
    timestamp_ms: AtomicU64,
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
    /// Operator-allowlisted payers: count-limited at their own cap and
    /// balance-bounded through a payer book.
    allowlisted_payers: Arc<AddressSet>,
    /// Operator per-transaction gas cap (`--txpool.max-tx-gas-limit`). The inner
    /// validator holds the same value but does not expose it, and Base transaction
    /// admission does not run the inner validator.
    max_tx_gas_limit: Option<u64>,
    /// Allowlisted payers validated since their last balance change. Bounded by
    /// the operator allowlist, so it needs no eviction.
    allowlisted_validations: Arc<RwLock<AddressSet>>,
    /// Advanced whenever a canonical state change may invalidate a limit
    /// classification captured by an in-flight validation.
    limit_class_generation: Arc<AtomicU64>,
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

    /// Returns the current block timestamp in milliseconds, the clock Base transaction
    /// validity windows and the nonce-free replay ring are evaluated against.
    pub fn block_timestamp_ms(&self) -> u64 {
        self.block_info.timestamp_ms.load(Ordering::Relaxed)
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

    /// Sets the operator-allowlisted payers. An allowlisted payer is limited to
    /// [`crate::GuardLimits::allowlisted_payment_limit`] inflight payments and
    /// also balance-bounded through a payer book: its balance can still move,
    /// so the count caps the exposure while the book tracks it against
    /// canonical balance updates.
    #[must_use]
    pub fn with_allowlisted_payers(self, payers: impl IntoIterator<Item = Address>) -> Self {
        Self { allowlisted_payers: Arc::new(payers.into_iter().collect()), ..self }
    }

    /// Sets the operator per-transaction gas cap applied to Base transaction admission.
    /// Must match the value given to the inner validator, which applies it to
    /// every other transaction type.
    #[must_use]
    pub const fn with_max_tx_gas_limit(mut self, max_tx_gas_limit: Option<u64>) -> Self {
        self.max_tx_gas_limit = max_tx_gas_limit;
        self
    }

    /// Returns the classification generation used to close
    /// validation/invalidation races.
    pub fn limit_class_generation(&self) -> u64 {
        self.limit_class_generation.load(Ordering::Acquire)
    }

    /// Advances the classification generation when canonical state changes
    /// affect a classification an in-flight validation may have captured.
    ///
    /// A balance change seeds a booked (allowlisted) payer's `PayerBook` on
    /// first admission. `on_balance_changed` only corrects payers that already
    /// have a book; a booked payer with no book yet would otherwise seed it
    /// from the (now stale) validation snapshot. Advancing the generation makes
    /// an admission whose validation predates this diff re-validate against the
    /// fresh balance.
    ///
    /// Restricted to allowlisted payers validated since their last balance
    /// change: ordinary balance churn, the vast majority, is unrelated to any
    /// book and must not advance the generation and bounce unrelated
    /// admissions, which fail as stale.
    pub fn invalidate_limit_classes(&self, diffs: &[crate::AccountStateDiff]) {
        let mut validations = self.allowlisted_validations.write();
        let mut changed = false;
        for diff in diffs {
            if diff.balance.is_some() && validations.remove(&diff.address) {
                changed = true;
            }
        }
        if changed {
            self.limit_class_generation.fetch_add(1, Ordering::Release);
        }
    }

    /// Resets classification tracking after a state-diff feed gap.
    pub fn clear_limit_classes(&self) {
        self.allowlisted_validations.write().clear();
        self.limit_class_generation.fetch_add(1, Ordering::Release);
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
                let timestamp = block.header().timestamp();
                this.block_info.timestamp.store(timestamp, Ordering::Relaxed);
                this.block_info
                    .timestamp_ms
                    .store(timestamp.saturating_mul(1_000), Ordering::Relaxed);
            } else {
                this.update_l1_block_info(block.header(), block.body().transactions());
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
            allowlisted_payers: Arc::default(),
            max_tx_gas_limit: None,
            allowlisted_validations: Arc::default(),
            limit_class_generation: Arc::default(),
        }
    }

    /// Update the block clock and L1 block info from the given header and its
    /// transactions.
    ///
    /// The L1 block info is read from the first (system) transaction. From
    /// Denim, the millisecond timestamp is decoded from the `BaseTime` update at
    /// `tx[1]`; before Denim, or without one, it is the whole second.
    ///
    /// Note: the system transactions are optional, in case this is used in a dev setup
    pub fn update_l1_block_info<H, T>(&self, header: &H, transactions: &[T])
    where
        H: BlockHeader,
        T: Transaction,
    {
        let timestamp = header.timestamp();
        let timestamp_ms = self
            .chain_spec()
            .is_feature_active_at_timestamp(BaseFeature::SubsecondBlocks, timestamp)
            .then(|| BaseTimeUpdateTx::decode_timestamp_ms(transactions, timestamp))
            .flatten()
            .unwrap_or_else(|| timestamp.saturating_mul(1_000));
        self.block_info.timestamp.store(timestamp, Ordering::Relaxed);
        self.block_info.timestamp_ms.store(timestamp_ms, Ordering::Relaxed);

        if let Some(Ok(l1_block_info)) =
            transactions.first().map(base_execution_evm::extract_l1_info_from_tx)
        {
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
    /// - for Base transactions: rejects submissions before the Everest upgrade is
    ///   active, runs structural checks, applies the operator pool policies the inner validator
    ///   applies to every other type, then runs Base-transaction-specific stateful validation for
    ///   sender and payer authorization, nonce/replay state, intrinsic gas, delegation safety,
    ///   and payer funding instead of using the inner Eth validator
    /// - ensures that the account has enough balance to cover the L1 gas cost
    pub async fn validate_one_with_state(
        &self,
        origin: TransactionOrigin,
        transaction: Tx,
        state: &mut Option<Box<dyn AccountInfoReader + Send>>,
    ) -> TransactionValidationOutcome<Tx> {
        let kind = if transaction.as_base_tx().is_some() { "base_tx" } else { "standard" };
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

        if let Some(signed) = transaction.as_base_tx() {
            let delegates = signed
                .tx()
                .account_changes
                .iter()
                .any(|change| matches!(change, AccountChange::Delegation(_)));
            let validation = {
                self.validate_base_tx_structural(signed)
                    .and_then(|()| {
                        self.validate_base_tx_pool_policy(origin, transaction.sender(), signed)
                    })
                    .and_then(|()| self.validate_base_tx_full(signed, transaction.sender()))
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
                payer_allowlisted: state.payer_allowlisted,
                payer_balance: state.payer_balance,
                max_cost: state.payer_max_cost,
            });
            let outcome = TransactionValidationOutcome::Valid {
                balance: state.sender_balance,
                state_nonce: state.sender_nonce,
                transaction: ValidTransaction::new(transaction, None),
                propagate,
                bytecode_hash: state.sender_bytecode_hash,
                // Reth reserves authorities under its EIP-7702 delegation slot
                // limit. A delegation entry delegates the sender, like a 7702
                // authorization, and always uses the protocol nonce, so it lands
                // in reth's pool. A payer installs no delegation; the
                // payer-keyed guard bounds what it sponsors.
                authorities: delegates.then(|| vec![state.sender]),
            };
            return self.apply_base_checks(Self::reject_delegated_validity(outcome), false);
        }
        let outcome = self.inner.validate_one_with_state(origin, transaction, state);
        self.apply_base_checks(Self::reject_delegated_validity(outcome), true)
    }

    /// Rejects a transaction with validity predicates that delegates an account
    /// (an EIP-7702 authorization list or a Base transaction delegation entry, both
    /// reported as `authorities`) or whose sender is EIP-7702 delegated.
    ///
    /// Validity transactions live in their own pool, which does not enforce
    /// reth's in-flight limits for delegated accounts and pending
    /// authorizations. Keeping delegation out of validity transactions entirely
    /// means those limits never need to span both pools. A sender delegated
    /// after admission loses its validity transactions on the code change (see
    /// `BaseTransactionPool::apply_state_diff`).
    fn reject_delegated_validity(
        outcome: TransactionValidationOutcome<Tx>,
    ) -> TransactionValidationOutcome<Tx> {
        match outcome {
            TransactionValidationOutcome::Valid {
                transaction, bytecode_hash, authorities, ..
            } if !transaction.transaction().validity_conditions().is_empty()
                && (authorities.is_some()
                    || bytecode_hash.is_some_and(|hash| hash != KECCAK_EMPTY)) =>
            {
                TransactionValidationOutcome::Invalid(
                    transaction.into_transaction(),
                    InvalidPoolTransactionError::other(
                        BaseTxPoolError::DelegatedValidityTransaction,
                    ),
                )
            }
            outcome => outcome,
        }
    }

    /// Runs full Base transaction admission checks that require account/precompile state:
    /// sender and payer authorization, nonce/replay state, intrinsic gas,
    /// delegation safety, and payer balance. This deliberately bypasses the inner
    /// Eth validator for Base transactions because sponsored transactions charge a payer
    /// instead of the sender.
    ///
    /// The `validate_one_with_state` snapshot is only an `AccountInfoReader`; Base transactions
    /// need storage/code reads for nonce channels and delegation checks, so this path takes its own
    /// full state snapshot.
    ///
    /// `sender` is the pool transaction's sender, recovered from `signed` at
    /// decode (or, on a builder, by the forwarding mempool node, whose sender the
    /// pool trusts for every transaction type), so the pool never recovers it
    /// again.
    fn validate_base_tx_full(
        &self,
        signed: &TxBaseSigned,
        sender: Address,
    ) -> Result<TxBaseValidationState, InvalidPoolTransactionError> {
        let classification_generation = self.limit_class_generation();
        let local_chain_id = self.inner.chain_spec().chain().id();
        let now = self.block_timestamp();
        let now_ms = self.block_timestamp_ms();
        let state = self.client().latest().map_err(|error| Self::provider_unavailable(error))?;

        // Authorization is stateless, as in block execution: the payer is
        // recovered from its secp256k1 signature, and the EOA sender is the one
        // recovered at decode.
        let auth_start = Instant::now();
        let auth_result = ResolvedSender::assume_recovered(signed, sender)
            .and_then(|sender| TransactionAuthorizer::authorize(signed, sender));
        ValidatorMetrics::auth_seconds().record(auth_start.elapsed().as_secs_f64());
        let authorized = auth_result.map_err(Self::map_tx_auth_error)?;
        let (sender, payer) = (authorized.sender, authorized.payer);
        // As in execution, a delegation entry may replace only empty code or a
        // delegation indicator.
        if let Some(delegation) = &authorized.delegation {
            let mut storage = StateProviderPrecompileStorage::new(&*state, local_chain_id, now);
            StorageCtx::enter(&mut storage, |sctx| delegation.check(sctx))
                .map_err(Self::map_delegation_error)?;
        }
        // Record the validation as soon as the payer is known, so a balance
        // change that lands before admission invalidates it (see
        // `invalidate_limit_classes`).
        let payer_allowlisted = self.allowlisted_payers.contains(&payer);
        if payer_allowlisted {
            self.allowlisted_validations.write().insert(payer);
        }

        let sender_account = state
            .basic_account(&sender)
            .map_err(|error| Self::state_read_error(error, "sender account read failed"))?
            .unwrap_or_default();
        let protocol_nonce = sender_account.nonce;

        let mut storage = StateProviderPrecompileStorage::new(&*state, local_chain_id, now);
        // `NonceValidator::validate` compares the nonce-free replay ring's stored
        // `valid_before` (Unix milliseconds) against `now_ms`; the storage
        // adapter keeps `now` in seconds for `block.timestamp`.
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

        let sender_nonce = self.base_tx_sender_nonce(
            &*state,
            local_chain_id,
            now,
            signed,
            sender,
            protocol_nonce,
        )?;
        let encoded = self.base_tx_encoded(signed);
        // Intrinsic gas is a pure function of the transaction, so admission,
        // `eth_estimateGas` and execution all charge the same amount.
        let intrinsic = IntrinsicGas::compute(signed, sender);
        // EIP-7623 calldata floor: a `gas_limit` below the floor branch is
        // invalid. Sender-intrinsic gas is checked too, matching execution,
        // since the floor excludes the state-dependent costs.
        if signed.tx().gas_limit < intrinsic.sender_floor()
            || intrinsic.execution_gas_available(signed.tx().gas_limit).is_none()
        {
            return Err(InvalidTransactionError::GasTooLow.into());
        }
        // EIP-7825 (from Azul): `gas_limit` plus the payer authentication
        // metered on top of it is bounded by the per-transaction gas cap,
        // matching execution.
        if self
            .chain_spec()
            .is_feature_active_at_timestamp(BaseFeature::TransactionGasLimitCap, now)
            && FeeCheck::max_chargeable_gas(signed.tx().gas_limit, intrinsic.payer_auth_cost)
                > MAX_TX_GAS_LIMIT_OSAKA
        {
            return Err(InvalidTransactionError::GasLimitTooHigh.into());
        }

        let payer_account = state
            .basic_account(&payer)
            .map_err(|error| Self::state_read_error(error, "payer account read failed"))?
            .unwrap_or_default();
        FeeCheck::validate_balance(
            payer_account.balance,
            signed.tx().gas_limit,
            intrinsic.payer_auth_cost,
            signed.tx().max_fee_per_gas,
        )
        .map_err(|_| {
            InvalidPoolTransactionError::from(InvalidTransactionError::InsufficientFunds(
                GotExpected {
                    got: payer_account.balance,
                    expected: FeeCheck::max_fee_charge(
                        signed.tx().gas_limit,
                        intrinsic.payer_auth_cost,
                        signed.tx().max_fee_per_gas,
                    ),
                }
                .into(),
            ))
        })?;

        let nonce_free = signed.tx().nonce_key == TxBaseConstants::NONCE_KEY_MAX;
        // Every expiry predicate as the last block millisecond at which it holds,
        // so the guard and the builder precheck share one inclusive deadline.
        let effective_expiry =
            signed.tx().last_valid_ms().map_or(UnixMillis::MAX, UnixMillis::from_millis);
        let mut watch_set = WatchSet::new().watch(InvalidationKey::Balance(payer));
        // A delegation is admitted against the sender's current code.
        if authorized.delegation.is_some() {
            watch_set.push(InvalidationKey::CodeHash(sender));
        }
        if effective_expiry != UnixMillis::MAX {
            watch_set.push(InvalidationKey::Expiry(effective_expiry));
        }
        let nonce_key = signed.tx().nonce_key;
        if nonce_key.is_zero() {
            watch_set.push(InvalidationKey::ProtocolNonce(sender));
        } else if nonce_free {
            // Mining any transaction with this `replay_id` (this one, or a twin
            // with other fees or another signature) records it here, after which
            // this transaction can never be included. A twin mined after this
            // validation but before admission writes the slot with no watcher
            // yet, so the transaction stays pooled until its `valid_before`.
            watch_set.push(InvalidationKey::Slot {
                address: NonceManagerStorage::ADDRESS,
                slot: B256::from(NonceManagerStorage::expiring_nonce_seen_slot(
                    signed.tx().replay_id(sender),
                )),
            });
        } else if let Ok(slot) = NonceManagerStorage::nonce_slot(sender, nonce_key) {
            watch_set.push(InvalidationKey::Slot {
                address: NonceManagerStorage::ADDRESS,
                slot: B256::from(slot),
            });
        }

        let gas_charge = FeeCheck::max_fee_charge(
            signed.tx().gas_limit,
            intrinsic.payer_auth_cost,
            signed.tx().max_fee_per_gas,
        );
        let additional_fee = if self.requires_l1_data_gas_fee() {
            let mut info = self.block_info.l1_block_info.read().clone();
            let spec_id = BaseSpecId::from_timestamp(self.chain_spec(), now);
            info.tx_cost(
                &encoded,
                U256::from(FeeCheck::max_chargeable_gas(
                    signed.tx().gas_limit,
                    intrinsic.payer_auth_cost,
                )),
                spec_id,
            )
        } else {
            U256::ZERO
        };
        // Admission only requires the payer to cover the transaction's fees.
        // `call.value` is not reserved: a call whose value the sender cannot
        // cover reverts at execution, and the fees are still paid.
        let payer_max_cost = gas_charge.saturating_add(additional_fee);
        if payer_account.balance < payer_max_cost {
            return Err(InvalidTransactionError::InsufficientFunds(
                GotExpected { got: payer_account.balance, expected: payer_max_cost }.into(),
            )
            .into());
        }
        let manifest = WatchManifest::new(payer, payer_max_cost, effective_expiry);

        Ok(TxBaseValidationState {
            sender,
            payer,
            classification_generation,
            payer_balance: payer_account.balance,
            sender_balance: sender_account.balance,
            sender_nonce,
            sender_bytecode_hash: Self::delegated_code_hash(&*state, &sender_account)?,
            watch_set,
            payer_allowlisted,
            payer_max_cost,
            manifest,
        })
    }

    fn base_tx_sender_nonce(
        &self,
        state: &dyn StateProvider,
        local_chain_id: u64,
        now: u64,
        signed: &TxBaseSigned,
        sender: Address,
        protocol_nonce: u64,
    ) -> Result<u64, InvalidPoolTransactionError> {
        let nonce_key = signed.tx().nonce_key;
        if nonce_key == TxBaseConstants::NONCE_KEY_MAX || nonce_key.is_zero() {
            return Ok(protocol_nonce);
        }
        let mut storage = StateProviderPrecompileStorage::new(state, local_chain_id, now);
        StorageCtx::enter(&mut storage, |ctx| {
            NonceManagerStorage::new(ctx)
                .get_nonce(sender, nonce_key)
                .map_err(|error| Self::precompile_storage_error(error, "nonce manager read failed"))
        })
    }

    /// Returns `account`'s code hash when its code is an EIP-7702 delegation
    /// indicator, matching the only sender code a standard transaction can
    /// report to reth.
    fn delegated_code_hash(
        state: &dyn StateProvider,
        account: &reth_primitives_traits::Account,
    ) -> Result<Option<B256>, InvalidPoolTransactionError> {
        let Some(code_hash) = account.bytecode_hash.filter(|hash| *hash != KECCAK_EMPTY) else {
            return Ok(None);
        };
        let delegated = state
            .bytecode_by_hash(&code_hash)
            .map_err(|error| Self::state_read_error(error, "sender code read failed"))?
            .is_some_and(|code| code.is_eip7702());
        Ok(delegated.then_some(code_hash))
    }

    fn base_tx_encoded(&self, signed: &TxBaseSigned) -> Vec<u8> {
        let mut encoded = Vec::with_capacity(signed.encode_2718_len());
        signed.encode_2718(&mut encoded);
        encoded
    }

    fn map_tx_auth_error(error: TxAuthError) -> InvalidPoolTransactionError {
        tracing::debug!(error = ?error, "Base transaction authorization failed");
        let reason = match error {
            TxAuthError::UnsupportedAuthenticator => {
                return InvalidTransactionError::TxTypeNotSupported.into();
            }
            TxAuthError::SenderRecovery => "EOA sender recovery failed",
            TxAuthError::PayerRecovery => "open payer recovery failed",
            TxAuthError::ResolvedSenderMismatch => "sender resolved from a different transaction",
            TxAuthError::UnexpectedPayerAuth => "self-paid transaction carries a payer_auth",
            TxAuthError::SignatureRecovery => "secp256k1 signature recovery failed",
            TxAuthError::SignerMismatch => {
                "secp256k1 signature does not recover to the named account"
            }
            TxAuthError::MultipleDelegations => "at most one delegation is allowed",
        };
        Self::base_tx_error(reason)
    }

    fn map_delegation_error(error: DelegationError) -> InvalidPoolTransactionError {
        match error {
            DelegationError::Storage(error) => {
                Self::precompile_storage_error(error, "Base transaction state access failed")
            }
            DelegationError::NonDelegatableCode { .. } => {
                Self::base_tx_error("delegation sender has non-delegation code")
            }
        }
    }

    fn map_nonce_error(error: NonceError) -> InvalidPoolTransactionError {
        match error {
            NonceError::TooLow { channel, got } | NonceError::TooHigh { channel, got } => {
                InvalidTransactionError::NonceNotConsistent { tx: got, state: channel }.into()
            }
            NonceError::Replay => Self::base_tx_error("nonce-free replay detected"),
            NonceError::NonceFreeSequence { .. } => {
                Self::base_tx_error("nonce-free transaction has a non-zero nonce sequence")
            }
            NonceError::SequenceSaturated => Self::base_tx_error("nonce sequence is saturated"),
            NonceError::DelegationRequiresProtocolNonce => {
                Self::base_tx_error("delegation requires the protocol nonce")
            }
            NonceError::Storage(_) => Self::base_tx_error("nonce state read failed"),
        }
    }

    /// Maps an [`TxBaseTimestampError`] (from
    /// [`TxBaseSigned::validate_timestamp`]) to a named pool-rejection reason
    /// and logs the mismatch. This deliberately does *not* collapse into
    /// `TxTypeNotSupported`: the transaction type is supported, its validity
    /// window is simply outside this node's admission window relative to `now`
    /// (the head-block timestamp, in **milliseconds**). Emitting the reason plus
    /// `now`/window bounds here makes the otherwise-silent, node-local rejection
    /// greppable.
    fn map_timestamp_error(
        error: TxBaseTimestampError,
        signed: &TxBaseSigned,
        now: u64,
    ) -> InvalidPoolTransactionError {
        let reason = match error {
            TxBaseTimestampError::NonceFreeMalformed => {
                "nonce-free transaction must set a non-zero valid_before and a zero nonce sequence"
            }
            TxBaseTimestampError::NonceFreeExpired => {
                "nonce-free transaction validity window has elapsed"
            }
            TxBaseTimestampError::NonceFreeExpiryTooFar => {
                "nonce-free transaction validity window exceeds the admission window"
            }
            TxBaseTimestampError::NotYetValid => "transaction is not yet valid",
            TxBaseTimestampError::Expired => "transaction validity window has elapsed",
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
        if matches!(error, TxBaseTimestampError::NonceFreeExpiryTooFar) {
            tracing::debug!(
                reason,
                now,
                valid_after = tx.valid_after_ms(),
                valid_before = tx.valid_before_ms(),
                valid_after_raw = tx.valid_after,
                valid_before_raw = tx.valid_before,
                nonce_key = %tx.nonce_key,
                window = TxBaseConstants::NONCE_FREE_MAX_EXPIRY_WINDOW,
                "Base transaction timestamp validation failed",
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
                "Base transaction timestamp validation failed",
            );
        }
        Self::base_tx_error(reason)
    }

    fn base_tx_error(reason: &'static str) -> InvalidPoolTransactionError {
        InvalidPoolTransactionError::other(BaseTxPoolError::TxBaseValidation { reason })
    }

    fn provider_unavailable(error: impl core::fmt::Display) -> InvalidPoolTransactionError {
        tracing::debug!(error = %error, "Base transaction state provider unavailable");
        Self::base_tx_error("state provider unavailable")
    }

    fn state_read_error(
        error: impl core::fmt::Display,
        reason: &'static str,
    ) -> InvalidPoolTransactionError {
        tracing::debug!(error = %error, reason = reason, "Base transaction state read failed");
        Self::base_tx_error(reason)
    }

    fn precompile_storage_error(
        error: impl core::fmt::Display,
        reason: &'static str,
    ) -> InvalidPoolTransactionError {
        tracing::debug!(error = %error, reason = reason, "Base transaction precompile storage read failed");
        Self::base_tx_error(reason)
    }

    /// Runs the mempool admission checks that apply to Base transactions without account state
    /// lookups. Enforces the Everest fork gate and the structural invariants listed in EIP-8430 §
    /// Validation and § Nonce-Free Mode.
    fn validate_base_tx_structural(
        &self,
        signed: &TxBaseSigned,
    ) -> Result<(), InvalidPoolTransactionError> {
        let size = signed.encode_2718_len();
        let limit = self.inner.max_tx_input_bytes();
        if size > limit {
            return Err(InvalidPoolTransactionError::OversizedData { size, limit });
        }
        // Cheap shape checks run before the fork gate.
        TxBaseStructure::validate(signed).map_err(Self::map_structural_error)?;

        // Read the head-block clock up front so a concurrent `on_new_head_block`
        // cannot move it between the fork gate and the expiry check.
        let now = self.block_timestamp();
        let now_ms = self.block_timestamp_ms();
        // Fork gate: Base transactions are only
        // admissible to the pool once the Everest upgrade is active.
        if !self.chain_spec().is_feature_active_at_timestamp(BaseFeature::TxBase, now) {
            return Err(InvalidTransactionError::TxTypeNotSupported.into());
        }
        let local_chain_id = self.inner.chain_spec().chain().id();
        signed.validate_static(local_chain_id).map_err(InvalidPoolTransactionError::from)?;
        // The validity window is evaluated against the head block's millisecond
        // timestamp; the fork gate above uses seconds.
        signed
            .validate_timestamp(now_ms)
            .map_err(|error| Self::map_timestamp_error(error, signed, now_ms))?;
        // The pool never recovers the EOA sender: full validation checks the
        // signature's encoding and takes the sender recovered at decode.
        Ok(())
    }

    /// Maps a structural rule failure to the pool's rejection: a named reason for
    /// the call phase cap, and an unsupported transaction type for every
    /// malformed shape (the pool's long-standing response to those).
    fn map_structural_error(error: TxBaseStructuralError) -> InvalidPoolTransactionError {
        match error {
            TxBaseStructuralError::TooManyCallPhases => {
                Self::base_tx_error("call phase count exceeds maximum")
            }
            TxBaseStructuralError::MalformedSenderAuth
            | TxBaseStructuralError::MalformedPayerAuth
            | TxBaseStructuralError::MalformedAccountChange => {
                InvalidTransactionError::TxTypeNotSupported.into()
            }
        }
    }

    /// Applies the operator pool policies that reth's `validate_stateless` enforces for every
    /// other transaction type, with the same rejections: the block gas limit,
    /// `--txpool.max-tx-gas-limit`, the RPC fee cap for local submissions, and
    /// `--txpool.minimum-priority-fee`.
    ///
    /// Payer authentication is metered on top of `gas_limit`, so both gas limits bound
    /// `gas_limit + payer_auth`, the amount execution reserves against the block, and the fee
    /// cap bounds the payer's maximum gas charge.
    fn validate_base_tx_pool_policy(
        &self,
        origin: TransactionOrigin,
        sender: Address,
        signed: &TxBaseSigned,
    ) -> Result<(), InvalidPoolTransactionError> {
        let tx = signed.tx();
        let payer_auth = IntrinsicGas::payer_auth_cost_of(signed);
        let reserved_gas = FeeCheck::max_chargeable_gas(tx.gas_limit, payer_auth);

        let block_gas_limit = self.inner.block_gas_limit();
        if reserved_gas > block_gas_limit {
            return Err(InvalidPoolTransactionError::ExceedsGasLimit(
                reserved_gas,
                block_gas_limit,
            ));
        }
        if let Some(max_tx_gas_limit) = self.max_tx_gas_limit
            && reserved_gas > max_tx_gas_limit
        {
            return Err(InvalidPoolTransactionError::MaxTxGasLimitExceeded(
                reserved_gas,
                max_tx_gas_limit,
            ));
        }
        if let Some(tx_fee_cap_wei) = self.inner.tx_fee_cap().filter(|cap| *cap != 0)
            && self.inner.local_transactions_config().is_local(origin, &sender)
        {
            let max_tx_fee = FeeCheck::max_fee_charge(tx.gas_limit, payer_auth, tx.max_fee_per_gas);
            if max_tx_fee > U256::from(tx_fee_cap_wei) {
                return Err(InvalidPoolTransactionError::ExceedsFeeCap {
                    max_tx_fee_wei: max_tx_fee.saturating_to(),
                    tx_fee_cap_wei,
                });
            }
        }
        if let Some(minimum_priority_fee) = *self.inner.minimum_priority_fee()
            && tx.max_priority_fee_per_gas < minimum_priority_fee
        {
            return Err(InvalidPoolTransactionError::PriorityFeeBelowMinimum {
                minimum_priority_fee,
            });
        }
        Ok(())
    }

    /// Performs the necessary Base-specific checks on top of the regular eth outcome: the DA
    /// footprint limit and, when `check_cost` is set, that `balance` covers the transaction's
    /// cost plus its L1 data and operator fees.
    ///
    /// Base transaction admission already requires the payer to cover gas plus those fees (see
    /// `validate_base_tx_full`), so it passes `check_cost = false`: its `cost()` is the
    /// sender's obligation, including a declared coinbase tip that admission deliberately does
    /// not require.
    fn apply_base_checks(
        &self,
        outcome: TransactionValidationOutcome<Tx>,
        check_cost: bool,
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
            if self
                .chain_spec()
                .is_feature_active_at_timestamp(BaseFeature::DaFootprint, self.block_timestamp())
            {
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

            if !check_cost {
                return TransactionValidationOutcome::Valid {
                    balance,
                    state_nonce,
                    transaction: valid_tx,
                    propagate,
                    bytecode_hash,
                    authorities,
                };
            }

            let encoded = valid_tx.transaction().encoded_2718();

            // Must mirror the execution-side cost in `BaseHandler` (L1 data fee + operator fee
            // post-Isthmus); otherwise operator-fee-underfunded txs get admitted but never execute.
            let spec_id = BaseSpecId::from_timestamp(self.chain_spec(), self.block_timestamp());
            let cost_addition = l1_block_info.tx_cost(
                &encoded,
                U256::from(valid_tx.transaction().gas_limit()),
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

    /// Validates the batch in order against one state provider, as reth's
    /// `EthTransactionValidator` does, instead of opening one per transaction.
    async fn validate_transactions(
        &self,
        transactions: impl IntoIterator<Item = (TransactionOrigin, Self::Transaction), IntoIter: Send>
        + Send,
    ) -> Vec<TransactionValidationOutcome<Self::Transaction>> {
        let mut state = None;
        let mut outcomes = Vec::new();
        for (origin, transaction) in transactions {
            outcomes.push(self.validate_one_with_state(origin, transaction, &mut state).await);
        }
        outcomes
    }

    fn on_new_head_block(&self, new_tip_block: &SealedBlock<Self::Block>) {
        self.inner.on_new_head_block(new_tip_block);
        self.update_l1_block_info(new_tip_block.header(), new_tip_block.body().transactions());
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
        TxBase, TxBaseConstants, TxBaseSigned, TxDeposit,
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

    /// Builds a [`BaseTransactionValidator`] against an Everest-activated test chain spec with no
    /// accounts seeded. Base transaction admission is fork-gated on Everest, so the structural-gate
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

    /// Returns the chain id the [`build_test_validator`] is configured against.
    fn test_chain_id() -> u64 {
        ChainConfig::mainnet().chain_id
    }

    fn balance_diff(address: Address, balance: u64) -> crate::AccountStateDiff {
        crate::AccountStateDiff {
            address,
            balance: Some(U256::from(balance)),
            nonce: None,
            code_changed: false,
            changed_slots: Vec::new(),
        }
    }

    /// An allowlisted payer seeds a balance book on first admission, so a
    /// balance change after its validation advances the generation: an
    /// admission validated against the old balance re-validates instead of
    /// seeding a stale book. An allowlisted payer with no validation since its
    /// last balance change must not advance it, and neither must any other
    /// payer, so balance churn does not bounce unrelated admissions.
    #[test]
    fn allowlisted_payer_balance_diff_advances_generation_only_after_validation() {
        let allowlisted = Address::repeat_byte(7);
        let validator = build_test_validator().with_allowlisted_payers([allowlisted]);
        let before = validator.limit_class_generation();
        validator.invalidate_limit_classes(&[balance_diff(allowlisted, 1)]);
        assert_eq!(validator.limit_class_generation(), before, "idle payer");

        validator.allowlisted_validations.write().insert(allowlisted);
        validator.invalidate_limit_classes(&[balance_diff(Address::repeat_byte(8), 2)]);
        assert_eq!(validator.limit_class_generation(), before, "another payer");

        validator.invalidate_limit_classes(&[balance_diff(allowlisted, 2)]);
        let after = validator.limit_class_generation();
        assert!(after > before, "a validated payer's balance change advances the generation");

        validator.invalidate_limit_classes(&[balance_diff(allowlisted, 3)]);
        assert_eq!(
            validator.limit_class_generation(),
            after,
            "the record is consumed; churn without a new validation does not advance it"
        );
    }

    /// Validation classifies an allowlisted payer so admission can count and
    /// book it.
    #[test]
    fn validation_classifies_allowlisted_payers() {
        let signer = PrivateKeySigner::random();
        let sender = signer.address();
        let tx = TxBase { gas_limit: 100_000, ..minimal_valid_eoa_tx() };
        let signature = signer.sign_hash_sync(&tx.sender_signature_hash()).unwrap();
        let signed =
            TxBaseSigned::new(tx, Bytes::from(signature.as_bytes().to_vec()), Bytes::new());
        let funded = ExtendedAccount::new(0, U256::from(1_000_000_000_000u64));

        let ordinary = build_test_validator_with_account(sender, funded.clone())
            .validate_base_tx_full(&signed, pool_sender(&signed))
            .unwrap();
        assert!(!ordinary.payer_allowlisted);
        let validator =
            build_test_validator_with_account(sender, funded).with_allowlisted_payers([sender]);
        let allowlisted = validator.validate_base_tx_full(&signed, pool_sender(&signed)).unwrap();
        assert!(allowlisted.payer_allowlisted, "the self-paying sender is its own payer");
        assert!(
            validator.allowlisted_validations.write().remove(&sender),
            "validation records the allowlisted payer for balance-diff invalidation"
        );
    }

    /// Signs `tx` as an EOA-path Base transaction and returns the resulting
    /// [`TxBaseSigned`] with a valid 65-byte secp256k1 `sender_auth`.
    fn sign_eoa_base_tx(tx: TxBase) -> TxBaseSigned {
        let signer = PrivateKeySigner::random();
        let signature = signer.sign_hash_sync(&tx.sender_signature_hash()).unwrap();
        let sig_bytes: Bytes = signature.as_bytes().to_vec().into();
        TxBaseSigned::new(tx, sig_bytes, Bytes::new())
    }

    /// The sender a pool transaction carries for `signed`: recovered from its
    /// signature, as decode does.
    fn pool_sender(signed: &TxBaseSigned) -> Address {
        signed.recover_sender().expect("test transaction signature recovers")
    }

    /// Returns a minimal, structurally valid EOA-path [`TxBase`] bound to the
    /// test chain. `sender` is left as `None` so the EOA recovery path is exercised.
    fn minimal_valid_eoa_tx() -> TxBase {
        TxBase {
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

    /// Maps a shared structural check to the pool's rejection, as admission does.
    fn structural(
        result: Result<(), TxBaseStructuralError>,
    ) -> Result<(), InvalidPoolTransactionError> {
        result.map_err(TestValidator::map_structural_error)
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
    /// named [`BaseTxPoolError::TxBaseValidation`] reason.
    #[track_caller]
    fn assert_structural_reason(
        result: Result<(), InvalidPoolTransactionError>,
        expected: &'static str,
    ) {
        match result {
            Err(InvalidPoolTransactionError::Other(error)) => {
                match error.as_any().downcast_ref::<BaseTxPoolError>() {
                    Some(BaseTxPoolError::TxBaseValidation { reason }) => {
                        assert_eq!(*reason, expected);
                    }
                    other => panic!("expected TxBaseValidation, got {other:?}"),
                }
            }
            other => panic!("expected TxBaseValidation, got {other:?}"),
        }
    }

    #[track_caller]
    fn assert_base_tx_validation_reason(
        result: Result<TxBaseValidationState, InvalidPoolTransactionError>,
        expected: &'static str,
    ) {
        match result {
            Err(InvalidPoolTransactionError::Other(error)) => {
                match error.as_any().downcast_ref::<BaseTxPoolError>() {
                    Some(BaseTxPoolError::TxBaseValidation { reason }) => {
                        assert_eq!(*reason, expected);
                    }
                    other => panic!("expected TxBaseValidation, got {other:?}"),
                }
            }
            other => panic!("expected TxBaseValidation, got {other:?}"),
        }
    }

    #[test]
    fn accepts_base_tx_with_minimum_valid_eoa_shape() {
        let validator = build_test_validator();
        let signed = sign_eoa_base_tx(minimal_valid_eoa_tx());
        assert!(validator.validate_base_tx_structural(&signed).is_ok());
    }

    #[test]
    fn accepts_base_tx_at_encoded_size_limit() {
        let signed = sign_eoa_base_tx(minimal_valid_eoa_tx());
        let validator = build_test_validator_with_max_tx_input_bytes(signed.encode_2718_len());

        assert!(validator.validate_base_tx_structural(&signed).is_ok());
    }

    #[test]
    fn rejects_base_tx_over_encoded_size_limit() {
        let signed = sign_eoa_base_tx(minimal_valid_eoa_tx());
        let size = signed.encode_2718_len();
        let limit = size - 1;
        let validator = build_test_validator_with_max_tx_input_bytes(limit);

        assert!(matches!(
            validator.validate_base_tx_structural(&signed),
            Err(InvalidPoolTransactionError::OversizedData {
                size: rejected_size,
                limit: rejected_limit,
            }) if rejected_size == size && rejected_limit == limit
        ));
    }

    #[test]
    fn rejects_constructed_base_tx_over_call_phase_limit() {
        let validator = build_test_validator();
        let tx = TxBase {
            calls: vec![Vec::new(); TxBaseConstants::MAX_CALL_PHASES_PER_TX + 1],
            ..minimal_valid_eoa_tx()
        };
        let signed = sign_eoa_base_tx(tx);

        assert_structural_reason(
            validator.validate_base_tx_structural(&signed),
            "call phase count exceeds maximum",
        );
    }

    /// The call phase cap is a cheap shape check, so it rejects before the fork
    /// gate.
    #[test]
    fn call_phase_cap_rejects_before_fork_gate() {
        let pre_everest = BaseChainSpecBuilder::base_mainnet().cobalt_activated().build();
        let tx = TxBase {
            calls: vec![Vec::new(); TxBaseConstants::MAX_CALL_PHASES_PER_TX + 1],
            ..minimal_valid_eoa_tx()
        };
        let signed = TxBaseSigned::new(tx, Bytes::from(vec![0xff; 65]), Bytes::new());

        for validator in
            [build_test_validator(), build_test_validator_with_spec(Arc::new(pre_everest))]
        {
            assert_structural_reason(
                validator.validate_base_tx_structural(&signed),
                "call phase count exceeds maximum",
            );
        }
    }

    /// An unrecoverable EOA signature passes the stateless checks and is rejected
    /// by full validation's encoding check, whatever sender the pool carries.
    #[test]
    fn unrecoverable_eoa_signature_is_rejected_by_full_validation() {
        let validator = build_test_validator();
        let unrecoverable =
            TxBaseSigned::new(minimal_valid_eoa_tx(), Bytes::from(vec![0xff; 65]), Bytes::new());
        assert!(validator.validate_base_tx_structural(&unrecoverable).is_ok());
        let error = validator.validate_base_tx_full(&unrecoverable, Address::ZERO).unwrap_err();
        assert!(error.to_string().contains("EOA sender recovery failed"), "{error}");
    }

    #[test]
    fn rejects_base_tx_before_everest_activation() {
        // Cobalt alone does not open the Base transaction gate.
        let chain_spec = BaseChainSpecBuilder::base_mainnet().cobalt_activated().build();
        let validator = build_test_validator_with_spec(Arc::new(chain_spec));
        let signed = sign_eoa_base_tx(minimal_valid_eoa_tx());
        assert_unsupported(validator.validate_base_tx_structural(&signed));
    }

    #[test]
    fn structural_base_tx_validation_is_origin_independent() {
        let validator = build_test_validator();
        let signed = sign_eoa_base_tx(minimal_valid_eoa_tx());
        assert!(validator.validate_base_tx_structural(&signed).is_ok());
    }

    #[test]
    fn rejects_base_tx_with_wrong_chain_id() {
        let validator = build_test_validator();
        let tx = TxBase { chain_id: test_chain_id() + 1, ..minimal_valid_eoa_tx() };
        let signed = sign_eoa_base_tx(tx);
        assert_chain_id_mismatch(validator.validate_base_tx_structural(&signed));
    }

    #[test]
    fn rejects_base_tx_with_tip_above_fee_cap() {
        let validator = build_test_validator();
        let tx = TxBase {
            max_fee_per_gas: 100,
            max_priority_fee_per_gas: 200,
            ..minimal_valid_eoa_tx()
        };
        let signed = sign_eoa_base_tx(tx);
        assert_tip_above_fee_cap(validator.validate_base_tx_structural(&signed));
    }

    #[test]
    fn rejects_base_tx_with_zero_gas_limit() {
        let validator = build_test_validator();
        let tx = TxBase { gas_limit: 0, ..minimal_valid_eoa_tx() };
        let signed = sign_eoa_base_tx(tx);
        assert!(matches!(
            validator.validate_base_tx_structural(&signed),
            Err(InvalidPoolTransactionError::Consensus(InvalidTransactionError::GasTooLow))
        ));
    }

    #[test]
    fn rejects_base_tx_with_zero_fee_cap() {
        let validator = build_test_validator();
        let tx = TxBase { max_fee_per_gas: 0, ..minimal_valid_eoa_tx() };
        let signed = sign_eoa_base_tx(tx);
        assert!(matches!(
            validator.validate_base_tx_structural(&signed),
            Err(InvalidPoolTransactionError::Consensus(InvalidTransactionError::FeeCapTooLow))
        ));
    }

    #[test]
    fn rejects_base_tx_nonce_free_without_expiry() {
        let validator = build_test_validator();
        let tx = TxBase {
            nonce_key: TxBaseConstants::NONCE_KEY_MAX,
            nonce_sequence: 0,
            valid_before: 0,
            ..minimal_valid_eoa_tx()
        };
        let signed = sign_eoa_base_tx(tx);
        assert_structural_reason(
            validator.validate_base_tx_structural(&signed),
            "nonce-free transaction must set a non-zero valid_before and a zero nonce sequence",
        );
    }

    #[test]
    fn rejects_base_tx_nonce_free_with_nonzero_sequence() {
        let validator = build_test_validator();
        let tx = TxBase {
            nonce_key: TxBaseConstants::NONCE_KEY_MAX,
            nonce_sequence: 1,
            valid_before: 5,
            ..minimal_valid_eoa_tx()
        };
        let signed = sign_eoa_base_tx(tx);
        assert_structural_reason(
            validator.validate_base_tx_structural(&signed),
            "nonce-free transaction must set a non-zero valid_before and a zero nonce sequence",
        );
    }

    #[test]
    fn rejects_base_tx_nonce_free_already_expired() {
        // Advance the validator's tracked block timestamp to a realistic clock
        // (now_ms = 1_700_000_100_000) so `valid_before` (a millisecond value one
        // second in the past) is strictly elapsed. Both values are >=
        // TIMESTAMP_MS_THRESHOLD, so normalization is a no-op and this exercises
        // the raw expiry comparison.
        let validator = build_test_validator();
        let header = alloy_consensus::Header { timestamp: 1_700_000_100, ..Default::default() };
        validator.update_l1_block_info::<_, TxEip1559>(&header, &[]);
        let tx = TxBase {
            nonce_key: TxBaseConstants::NONCE_KEY_MAX,
            nonce_sequence: 0,
            valid_before: 1_700_000_099_000,
            ..minimal_valid_eoa_tx()
        };
        let signed = sign_eoa_base_tx(tx);
        assert_structural_reason(
            validator.validate_base_tx_structural(&signed),
            "nonce-free transaction validity window has elapsed",
        );
    }

    #[test]
    fn base_tx_window_uses_the_head_block_millisecond_clock() {
        // The head block is 400 ms past its whole second, so a `valid_before`
        // 100 ms past that second has already elapsed.
        let timestamp = 1_700_000_000;
        let header = alloy_consensus::Header { timestamp, ..Default::default() };
        let block_transactions: [BaseTransactionSigned; 2] = [
            TxDeposit::default().into(),
            BaseTimeUpdateTx::new(400).unwrap().into_deposit_tx(1).into_inner().into(),
        ];
        let tx = TxBase {
            nonce_key: TxBaseConstants::NONCE_KEY_MAX,
            nonce_sequence: 0,
            valid_before: timestamp * 1_000 + 100,
            ..minimal_valid_eoa_tx()
        };
        let signed = sign_eoa_base_tx(tx);

        let validator = build_test_validator();
        validator.update_l1_block_info(&header, &block_transactions);
        assert_eq!(validator.block_timestamp_ms(), timestamp * 1_000 + 400);
        assert_structural_reason(
            validator.validate_base_tx_structural(&signed),
            "nonce-free transaction validity window has elapsed",
        );

        // Before Denim there is no `BaseTime` update, so the clock stays on the
        // whole second.
        let pre_denim = BaseChainSpecBuilder::base_mainnet().cobalt_activated().build();
        let validator = build_test_validator_with_spec(Arc::new(pre_denim));
        validator.update_l1_block_info(&header, &block_transactions);
        assert_eq!(validator.block_timestamp_ms(), timestamp * 1_000);
    }

    #[test]
    fn rejects_base_tx_nonce_free_not_yet_valid() {
        // Realistic millisecond clock (both bounds >= TIMESTAMP_MS_THRESHOLD, so
        // normalization is a no-op). A future `valid_after` opens the window
        // later, so the nonce-free branch must reject with `NotYetValid` before
        // the expiry checks (which `validate_timestamp` evaluates afterward).
        let validator = build_test_validator();
        let header = alloy_consensus::Header { timestamp: 1_700_000_000, ..Default::default() };
        validator.update_l1_block_info::<_, TxEip1559>(&header, &[]);
        let now_ms = 1_700_000_000_000;
        let tx = TxBase {
            nonce_key: TxBaseConstants::NONCE_KEY_MAX,
            nonce_sequence: 0,
            valid_after: now_ms + 50_000,
            valid_before: now_ms + 60_000,
            ..minimal_valid_eoa_tx()
        };
        let signed = sign_eoa_base_tx(tx);
        assert_structural_reason(
            validator.validate_base_tx_structural(&signed),
            "transaction is not yet valid",
        );
    }

    #[test]
    fn rejects_base_tx_nonce_bearing_not_yet_valid() {
        // Sequenced (nonce-bearing) transaction on a realistic millisecond clock
        // with a future `valid_after` (>= TIMESTAMP_MS_THRESHOLD, so normalization
        // is a no-op): the else-branch of `validate_timestamp` must reject with
        // `NotYetValid`.
        let validator = build_test_validator();
        let header = alloy_consensus::Header { timestamp: 1_700_000_000, ..Default::default() };
        validator.update_l1_block_info::<_, TxEip1559>(&header, &[]);
        let now_ms = 1_700_000_000_000;
        let tx = TxBase { valid_after: now_ms + 50_000, ..minimal_valid_eoa_tx() };
        let signed = sign_eoa_base_tx(tx);
        assert_structural_reason(
            validator.validate_base_tx_structural(&signed),
            "transaction is not yet valid",
        );
    }

    #[test]
    fn rejects_base_tx_nonce_free_expiry_too_far_in_future() {
        // Seed a realistic millisecond clock and place `valid_before` exactly one
        // millisecond past the admission-window edge (`now_ms +
        // NONCE_FREE_MAX_EXPIRY_WINDOW`). Both bounds are >= TIMESTAMP_MS_THRESHOLD
        // so normalization is a no-op; this exercises the true edge+1 rejection,
        // the mirror of `accepts_base_tx_nonce_free_at_expiry_window_edge`.
        let validator = build_test_validator();
        let header = alloy_consensus::Header { timestamp: 1_700_000_000, ..Default::default() };
        validator.update_l1_block_info::<_, TxEip1559>(&header, &[]);
        let now_ms = 1_700_000_000_000;
        let tx = TxBase {
            nonce_key: TxBaseConstants::NONCE_KEY_MAX,
            nonce_sequence: 0,
            valid_before: now_ms + TxBaseConstants::NONCE_FREE_MAX_EXPIRY_WINDOW + 1,
            ..minimal_valid_eoa_tx()
        };
        let signed = sign_eoa_base_tx(tx);
        assert_structural_reason(
            validator.validate_base_tx_structural(&signed),
            "nonce-free transaction validity window exceeds the admission window",
        );
    }

    #[test]
    fn accepts_base_tx_nonce_free_at_expiry_window_edge() {
        // Seed a realistic millisecond clock and place `valid_before` exactly at
        // the admission-window edge (`now_ms + NONCE_FREE_MAX_EXPIRY_WINDOW`).
        // Both bounds are >= TIMESTAMP_MS_THRESHOLD so normalization is a no-op;
        // this checks the inclusive edge in true milliseconds.
        let validator = build_test_validator();
        let header = alloy_consensus::Header { timestamp: 1_700_000_000, ..Default::default() };
        validator.update_l1_block_info::<_, TxEip1559>(&header, &[]);
        let now_ms = 1_700_000_000_000;
        let tx = TxBase {
            nonce_key: TxBaseConstants::NONCE_KEY_MAX,
            nonce_sequence: 0,
            valid_before: now_ms + TxBaseConstants::NONCE_FREE_MAX_EXPIRY_WINDOW,
            ..minimal_valid_eoa_tx()
        };
        let signed = sign_eoa_base_tx(tx);
        assert!(validator.validate_base_tx_structural(&signed).is_ok());
    }

    #[test]
    fn rejects_base_tx_with_invalid_sender_auth_length_eoa_path() {
        // EOA path requires exactly 65 bytes; anything else is rejected.
        let tx = minimal_valid_eoa_tx();
        let signed = TxBaseSigned::new(tx, Bytes::from_static(&[0u8; 32]), Bytes::new());
        assert_unsupported(structural(TxBaseStructure::validate_sender_auth(&signed)));
    }

    #[test]
    fn rejects_base_tx_with_empty_sender_auth() {
        let tx = minimal_valid_eoa_tx();
        let signed = TxBaseSigned::new(tx, Bytes::new(), Bytes::new());
        assert_unsupported(structural(TxBaseStructure::validate_sender_auth(&signed)));
    }

    // A named sender must not select the reserved `address(0)` authenticator.
    #[test]
    fn rejects_base_tx_named_sender_with_reserved_authenticator() {
        let tx = TxBase { sender: Some(Address::repeat_byte(0xaa)), ..minimal_valid_eoa_tx() };
        let auth = Bytes::from(Address::ZERO.to_vec());
        let signed = TxBaseSigned::new(tx, auth, Bytes::new());
        assert_unsupported(structural(TxBaseStructure::validate_sender_auth(&signed)));
    }

    #[test]
    fn rejects_base_tx_named_sender_with_short_auth() {
        let tx = TxBase { sender: Some(Address::repeat_byte(0xaa)), ..minimal_valid_eoa_tx() };
        let signed = TxBaseSigned::new(tx, Bytes::from_static(&[0u8; 5]), Bytes::new());
        assert_unsupported(structural(TxBaseStructure::validate_sender_auth(&signed)));
    }

    #[test]
    fn rejects_base_tx_payer_present_without_auth() {
        let tx = TxBase { payer: Some(Address::repeat_byte(0x11)), ..minimal_valid_eoa_tx() };
        let signed = TxBaseSigned::new(tx, Bytes::from_static(&[0u8; 65]), Bytes::new());
        assert_unsupported(structural(TxBaseStructure::validate_payer_auth(&signed)));
    }

    #[test]
    fn rejects_base_tx_payer_absent_with_auth() {
        let tx = minimal_valid_eoa_tx();
        let signed =
            TxBaseSigned::new(tx, Bytes::from_static(&[0u8; 65]), Bytes::from_static(&[0u8; 20]));
        assert_unsupported(structural(TxBaseStructure::validate_payer_auth(&signed)));
    }

    #[test]
    fn rejects_base_tx_payer_authenticator_reserved() {
        let tx = TxBase { payer: Some(Address::repeat_byte(0x11)), ..minimal_valid_eoa_tx() };
        let signed = TxBaseSigned::new(
            tx,
            Bytes::from_static(&[0u8; 65]),
            Bytes::from(Address::ZERO.to_vec()),
        );
        assert_unsupported(structural(TxBaseStructure::validate_payer_auth(&signed)));
    }

    /// A named sender or named payer selecting an authenticator other than the
    /// native secp256k1 one is an unsupported transaction type.
    #[test]
    fn non_k1_authenticator_is_unsupported() {
        let mut other_auth = Address::repeat_byte(0x05).to_vec();
        other_auth.extend_from_slice(&[0u8; 64]);
        let sender = TxBase { sender: Some(Address::repeat_byte(0xaa)), ..minimal_valid_eoa_tx() };
        let sender = TxBaseSigned::new(sender, Bytes::from(other_auth.clone()), Bytes::new());
        let payer = TxBase { payer: Some(Address::repeat_byte(0xbb)), ..minimal_valid_eoa_tx() };
        let payer =
            TxBaseSigned::new(payer, Bytes::from_static(&[0u8; 65]), Bytes::from(other_auth));

        for signed in [sender, payer] {
            assert_unsupported(build_test_validator().validate_base_tx_structural(&signed));
        }
    }

    #[test]
    fn accepts_base_tx_with_one_delegation() {
        let tx = TxBase {
            account_changes: vec![AccountChange::Delegation(Delegation {
                target: Address::repeat_byte(0x11),
            })],
            ..minimal_valid_eoa_tx()
        };
        assert!(
            structural(TxBaseStructure::validate_account_changes(&sign_eoa_base_tx(tx))).is_ok()
        );
    }

    #[test]
    fn rejects_base_tx_multiple_delegations() {
        let tx = TxBase {
            account_changes: vec![
                AccountChange::Delegation(Delegation { target: Address::repeat_byte(0x11) }),
                AccountChange::Delegation(Delegation { target: Address::repeat_byte(0x22) }),
            ],
            ..minimal_valid_eoa_tx()
        };
        assert_unsupported(structural(TxBaseStructure::validate_account_changes(
            &sign_eoa_base_tx(tx),
        )));
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
        validator.update_l1_block_info(&header, &[l1_info_tx]);

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
    fn base_tx_payer_max_cost_includes_l1_and_operator_fees() {
        let chain_config = ChainConfig::mainnet();
        let chain_spec = everest_chain_spec();
        let signer = PrivateKeySigner::random();
        let sender = signer.address();
        let tx = minimal_valid_eoa_tx();
        let signature = signer.sign_hash_sync(&tx.sender_signature_hash()).unwrap();
        let signed =
            TxBaseSigned::new(tx, Bytes::from(signature.as_bytes().to_vec()), Bytes::new());

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
        validator.update_l1_block_info(&header, &[l1_info_tx]);

        let state = validator
            .validate_base_tx_full(&signed, pool_sender(&signed))
            .expect("valid funded Base transaction tx");
        let encoded = validator.base_tx_encoded(&signed);
        // Self-pay: no payer authentication gas.
        let max_gas = FeeCheck::max_chargeable_gas(signed.tx().gas_limit, 0);
        let gas_charge =
            FeeCheck::max_fee_charge(signed.tx().gas_limit, 0, signed.tx().max_fee_per_gas);
        let spec_id = BaseSpecId::from_timestamp(&chain_spec, chain_config.isthmus_timestamp);
        let mut l1_block_info = base_execution_evm::parse_l1_info(&isthmus_data).unwrap();
        let additional_fees = l1_block_info.tx_cost(&encoded, U256::from(max_gas), spec_id);

        assert!(!additional_fees.is_zero(), "fixture must charge L1/operator fees");
        assert_eq!(state.payer_max_cost, gas_charge.saturating_add(additional_fees));
        assert_eq!(state.manifest.payer_max_cost(), state.payer_max_cost);
    }

    /// Admission reserves only the transaction's fees: call value is not
    /// reserved, so a call worth more than the sender's balance is admitted as
    /// long as the payer can cover the fees.
    #[test]
    fn base_tx_admission_ignores_call_value() {
        const BALANCE: u64 = 1_000_000_000_000;
        let signer = PrivateKeySigner::random();
        let sender = signer.address();
        let recipient = Address::repeat_byte(0xee);
        let signed_with_value = |value: U256| {
            let tx = TxBase {
                gas_limit: 100_000,
                calls: vec![vec![Call { to: recipient, value, data: Bytes::new() }]],
                ..minimal_valid_eoa_tx()
            };
            let signature = signer.sign_hash_sync(&tx.sender_signature_hash()).unwrap();
            TxBaseSigned::new(tx, Bytes::from(signature.as_bytes().to_vec()), Bytes::new())
        };
        let validator =
            build_test_validator_with_account(sender, ExtendedAccount::new(0, U256::from(BALANCE)));

        let without_value = validator
            .validate_base_tx_full(&signed_with_value(U256::ZERO), sender)
            .expect("fees are affordable");
        let with_value = validator
            .validate_base_tx_full(&signed_with_value(U256::from(BALANCE) * U256::from(10)), sender)
            .expect("call value beyond the balance does not block admission");
        assert_eq!(with_value.payer_max_cost, without_value.payer_max_cost);
        assert_eq!(with_value.manifest.payer_max_cost(), with_value.payer_max_cost);
    }

    /// EIP-7825 bounds `gas_limit` plus payer authentication at admission, as
    /// at execution.
    #[test]
    fn base_tx_gas_above_the_per_transaction_cap_is_rejected() {
        let signer = PrivateKeySigner::random();
        let sender = signer.address();
        let signed_with_gas = |gas_limit: u64| {
            let tx = TxBase { gas_limit, ..minimal_valid_eoa_tx() };
            let signature = signer.sign_hash_sync(&tx.sender_signature_hash()).unwrap();
            TxBaseSigned::new(tx, Bytes::from(signature.as_bytes().to_vec()), Bytes::new())
        };
        let validator = build_test_validator_with_account(
            sender,
            ExtendedAccount::new(0, U256::from(u64::MAX) * U256::from(u64::MAX)),
        );

        validator
            .validate_base_tx_full(&signed_with_gas(MAX_TX_GAS_LIMIT_OSAKA), sender)
            .expect("gas at the cap is admitted");
        let err = validator
            .validate_base_tx_full(&signed_with_gas(MAX_TX_GAS_LIMIT_OSAKA + 1), sender)
            .expect_err("gas above the cap is rejected");
        assert!(
            matches!(
                err,
                InvalidPoolTransactionError::Consensus(InvalidTransactionError::GasLimitTooHigh)
            ),
            "expected GasLimitTooHigh, got {err:?}"
        );
    }

    #[test]
    fn expiry_deadline_is_the_last_valid_block_millisecond() {
        let chain_spec = everest_chain_spec();
        let signer = PrivateKeySigner::random();
        // Realistic seconds clock; `now * 1000` stays >= TIMESTAMP_MS_THRESHOLD so
        // the millisecond `valid_before` bounds below are not re-scaled by
        // normalization.
        let now = 1_700_000_000;
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
        validator.update_l1_block_info::<_, TxEip1559>(&header, &[]);

        // A sub-second nonce-bearing bound is inclusive. A nonce-free bound at the
        // admission-window edge is exclusive, so the deadline is one millisecond
        // earlier.
        let nonce_free_valid_before = now * 1000 + TxBaseConstants::NONCE_FREE_MAX_EXPIRY_WINDOW;
        for (nonce_key, valid_before, deadline) in [
            (U256::ZERO, now * 1000 + 1_300, now * 1000 + 1_300),
            (TxBaseConstants::NONCE_KEY_MAX, nonce_free_valid_before, nonce_free_valid_before - 1),
        ] {
            let tx =
                TxBase { nonce_key, nonce_sequence: 0, valid_before, ..minimal_valid_eoa_tx() };
            let signature = signer.sign_hash_sync(&tx.sender_signature_hash()).unwrap();
            let signed =
                TxBaseSigned::new(tx, Bytes::from(signature.as_bytes().to_vec()), Bytes::new());

            let state =
                validator.validate_base_tx_full(&signed, pool_sender(&signed)).expect("valid tx");
            let deadline = UnixMillis::from_millis(deadline);
            assert_eq!(state.manifest.effective_expiry(), deadline);
            assert!(state.watch_set.contains(&InvalidationKey::Expiry(deadline)));
        }
    }

    /// Builds a K1 authenticator-prefixed auth blob (`K1(20) || r || s || v`,
    /// `v` in `{27, 28}`, low-s) over `hash` for the named-account wire form.
    fn k1_auth_blob(signer: &PrivateKeySigner, hash: B256) -> Bytes {
        let sig = signer.sign_hash_sync(&hash).unwrap();
        let mut out = Vec::with_capacity(20 + 65);
        out.extend_from_slice(TxBaseConstants::K1_AUTHENTICATOR.as_slice());
        out.extend_from_slice(&sig.r().to_be_bytes::<32>());
        out.extend_from_slice(&sig.s().to_be_bytes::<32>());
        out.push(27 + u8::from(sig.v()));
        Bytes::from(out)
    }

    fn delegation_indicator(target: Address) -> Bytes {
        let mut code = Vec::with_capacity(TxBaseConstants::DELEGATION_INDICATOR_SIZE);
        code.extend_from_slice(&TxBaseConstants::DELEGATION_INDICATOR_PREFIX);
        code.extend_from_slice(target.as_slice());
        Bytes::from(code)
    }

    fn delegation_validation_fixture(
        signer: &PrivateKeySigner,
        existing_code: Option<Bytes>,
    ) -> (TestValidator, TxBaseSigned, Address) {
        let sender = signer.address();
        let tx = TxBase {
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
        let signed = TxBaseSigned::new(tx, sender_auth, Bytes::new());

        let mut account = ExtendedAccount::new(0, U256::from(1_000_000_000_000_000_000u64));
        if let Some(code) = existing_code {
            account = account.with_bytecode(code);
        }
        let validator = build_test_validator_with_account(sender, account);
        (validator, signed, sender)
    }

    #[test]
    fn rejects_delegation_over_ordinary_code() {
        let signer = PrivateKeySigner::random();
        let (validator, signed, sender) =
            delegation_validation_fixture(&signer, Some(Bytes::from_static(&[0x60, 0x00])));
        assert_eq!(sender, signer.address());

        assert_base_tx_validation_reason(
            validator.validate_base_tx_full(&signed, pool_sender(&signed)),
            "delegation sender has non-delegation code",
        );
    }

    #[test]
    fn admits_delegation_over_empty_code() {
        let signer = PrivateKeySigner::random();
        let (validator, signed, sender) = delegation_validation_fixture(&signer, None);
        assert_eq!(sender, signer.address());

        let state = validator
            .validate_base_tx_full(&signed, pool_sender(&signed))
            .expect("empty sender code must accept delegation");
        assert_eq!(state.sender, sender);
        assert_eq!(state.payer, sender);
        assert_eq!(state.sender_bytecode_hash, None);
        assert!(state.watch_set.iter().any(|key| *key == InvalidationKey::CodeHash(sender)));
    }

    /// Admission applies the same rule as inclusion: a delegation must use the
    /// protocol nonce, so it is rejected on a 2D nonce lane (here at the lane's
    /// current sequence) and nonce-free (here within its validity window).
    #[test]
    fn rejects_delegation_off_protocol_nonce() {
        let now_ms = 1_700_000_000_000;
        for (nonce_key, valid_before) in
            [(U256::from(7), 0), (TxBaseConstants::NONCE_KEY_MAX, now_ms + 10_000)]
        {
            let signer = PrivateKeySigner::random();
            let (validator, signed, _) = delegation_validation_fixture(&signer, None);
            let header =
                alloy_consensus::Header { timestamp: now_ms / 1_000, ..Default::default() };
            validator.update_l1_block_info::<_, TxEip1559>(&header, &[]);
            let tx = TxBase { nonce_key, valid_before, ..signed.tx().clone() };
            let sender_auth = k1_auth_blob(&signer, tx.sender_signature_hash()).slice(20..);
            let signed = TxBaseSigned::new(tx, sender_auth, Bytes::new());

            assert_base_tx_validation_reason(
                validator.validate_base_tx_full(&signed, pool_sender(&signed)),
                "delegation requires the protocol nonce",
            );
        }
    }

    #[test]
    fn admits_delegation_update_over_existing_indicator() {
        let signer = PrivateKeySigner::random();
        let existing_code = delegation_indicator(Address::repeat_byte(0x11));
        let expected_hash = alloy_primitives::keccak256(&existing_code);
        let (validator, signed, sender) =
            delegation_validation_fixture(&signer, Some(existing_code));
        assert_eq!(sender, signer.address());

        let state = validator
            .validate_base_tx_full(&signed, pool_sender(&signed))
            .expect("existing delegation indicator must accept a target update");
        assert_eq!(state.sender, sender);
        assert_eq!(state.payer, sender);
        assert_eq!(state.sender_bytecode_hash, Some(expected_hash));
        assert!(state.watch_set.iter().any(|key| *key == InvalidationKey::CodeHash(sender)));
    }

    /// Reth applies its EIP-7702 delegation slot limit to any sender reported
    /// with code, so a sender whose code is not a delegation indicator must
    /// report none, as a standard transaction's sender would.
    #[test]
    fn sender_with_non_delegation_code_reports_no_code_hash() {
        let signer = PrivateKeySigner::random();
        let sender = signer.address();
        let tx = TxBase {
            chain_id: test_chain_id(),
            nonce_key: U256::ZERO,
            max_fee_per_gas: 100,
            gas_limit: 1_000_000,
            ..Default::default()
        };
        let sender_auth = k1_auth_blob(&signer, tx.sender_signature_hash()).slice(20..);
        let signed = TxBaseSigned::new(tx, sender_auth, Bytes::new());
        let account = ExtendedAccount::new(0, U256::from(1_000_000_000_000_000_000u64))
            .with_bytecode(Bytes::from_static(&[0x60, 0x00]));
        let validator = build_test_validator_with_account(sender, account);

        let state = validator
            .validate_base_tx_full(&signed, pool_sender(&signed))
            .expect("a sender with code may send a Base transaction");
        assert_eq!(state.sender_bytecode_hash, None);
    }
}
