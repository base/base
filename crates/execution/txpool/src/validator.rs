use std::{
    any::Any,
    collections::{BTreeMap, BTreeSet},
    sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
    },
};

use alloy_consensus::{BlockHeader, Transaction, constants::KECCAK_EMPTY};
use alloy_eips::eip2718::Encodable2718;
use alloy_primitives::{Address, B256, LogData, U256};
use base_common_chains::Upgrades;
use base_common_consensus::{
    AccountChange, ActorChange, ActorChangeType, Eip8130Constants, Eip8130Contracts, Eip8130Signed,
    InitialActor,
};
use base_common_evm::{BaseSpecId, L1BlockInfo};
use base_common_genesis::DaFootprintGasScalarUpdate;
use base_common_precompiles::NonceManagerStorage;
use base_execution_eip8130::{
    AccountConfigurationStorage, ApplyError, AuthorizeError, FeeCheck, IntrinsicGas,
    IntrinsicGasInput, NonceError, NonceMode, NonceValidator, TransactionAuthorizer, TxAuthError,
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

use crate::BasePooledTx;

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
#[derive(Debug, Clone, Copy)]
struct Eip8130ValidationState {
    sender: Address,
    payer: Address,
    payer_balance_after_auth: U256,
    sender_nonce: u64,
    sender_bytecode_hash: Option<B256>,
    /// Payer-authentication gas metered on top of `gas_limit`. The execution
    /// path charges the operator fee on `gas_limit + payer_auth`, so admission
    /// must do the same to avoid admitting operator-fee-underfunded sponsored
    /// transactions. Zero for self-pay transactions.
    payer_auth: u64,
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

    fn checkpoint_commit(&mut self) {}

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
}

impl<'a> OverlayPrecompileStorage<'a> {
    const fn new(inner: StateProviderPrecompileStorage<'a>) -> Self {
        Self { inner, storage: BTreeMap::new(), transient: BTreeMap::new() }
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
        self.inner.with_account_code(address, f)
    }

    fn sload(&mut self, address: Address, key: U256) -> Result<U256, BasePrecompileError> {
        if let Some(value) = self.storage.get(&(address, key)) {
            return Ok(*value);
        }
        self.inner.sload(address, key)
    }

    fn tload(&mut self, address: Address, key: U256) -> Result<U256, BasePrecompileError> {
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

    fn checkpoint_commit(&mut self) {}

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

    /// Whether execution will auto-delegate the code-less sender to
    /// `DEFAULT_ACCOUNT` (charging a `DELEGATION_DEPOSIT_COST`) given the
    /// transaction's `account_changes`.
    ///
    /// Auto-delegation is suppressed only when the transaction leaves the sender
    /// with non-empty code: a `Create` or a *non-zero* `Delegation` both install
    /// code, so `auto_delegate_codeless_sender` is a no-op at execution. A
    /// zero-target `Delegation` (a delegation clear) instead leaves the sender
    /// code-less, so execution re-fires auto-delegation and charges a second
    /// deposit (one for the clear entry itself, one for the auto-delegation); it
    /// must NOT suppress budgeting here.
    pub fn sender_auto_delegated(account_changes: &[AccountChange]) -> bool {
        !account_changes.iter().any(|change| match change {
            AccountChange::Create(_) => true,
            AccountChange::Delegation(delegation) => !delegation.target.is_zero(),
            AccountChange::ConfigChange(_) => false,
        })
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
    /// - for eip8130 (account abstraction): rejects submissions before the Cobalt upgrade is
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
        let local_chain_id = self.inner.chain_spec().chain().id();
        let now = self.block_timestamp();
        let state = self.client().latest().map_err(|error| Self::provider_unavailable(error))?;

        // Authorize *and apply* the account changes against a writable overlay so
        // the sender/payer and every config change are validated against the same
        // evolving state the builder sees at inclusion — both run
        // `TransactionAuthorizer::authorize_and_apply`. The overlay's buffered
        // writes are discarded with it; admission never mutates canonical state.
        let mut storage = OverlayPrecompileStorage::new(StateProviderPrecompileStorage::new(
            &*state,
            local_chain_id,
            now,
        ));
        let (sender, payer, is_create) = StorageCtx::enter(&mut storage, |ctx| {
            let applied = {
                let mut account_config = AccountConfigurationStorage::new(ctx);
                TransactionAuthorizer::authorize_and_apply(
                    signed,
                    &mut account_config,
                    local_chain_id,
                    now,
                )?
            };
            if let Some(delegation) = applied.applied.delegation {
                delegation.install(ctx).map_err(TxAuthError::from)?;
            }

            let sender = applied.actors.sender.account;
            let payer = applied.actors.payer.map_or(sender, |actor| actor.account);
            // Thread the authoritative create flag through rather than
            // re-scanning account_changes below.
            let is_create = applied.applied.created.is_some();
            Ok::<_, TxAuthError>((sender, payer, is_create))
        })
        .map_err(Self::map_tx_auth_error)?;

        let sender_account = state
            .basic_account(&sender)
            .map_err(|error| Self::state_read_error(error, "sender account read failed"))?
            .unwrap_or_default();
        let protocol_nonce = sender_account.nonce;
        if is_create {
            Self::validate_eip8130_create_freshness(&*state, sender, &sender_account)?;
        }

        let mut storage = StateProviderPrecompileStorage::new(&*state, local_chain_id, now);
        StorageCtx::enter(&mut storage, |ctx| {
            let nonce_storage = NonceManagerStorage::new(ctx);
            NonceValidator::validate(
                signed.tx(),
                sender,
                protocol_nonce,
                &nonce_storage,
                NonceMode::Pool,
                now,
            )
            .map(|_| ())
            .map_err(Self::map_nonce_error)
        })?;

        let (nonce_key_first_use, sender_nonce) =
            self.eip8130_nonce_state(&*state, local_chain_id, now, signed, sender, protocol_nonce)?;
        // Conservatively assume auto-delegation fires unless the transaction itself
        // leaves the sender with non-empty code (see `sender_auto_delegated`). This
        // intentionally ignores the sender's current on-chain code state: a sender
        // that is already delegated (has code) at admission time may lose its
        // delegation before inclusion (e.g. via a native EIP-7702 revocation), so
        // always budgeting `DELEGATION_DEPOSIT_COST` in `gas_limit` prevents a hard
        // intrinsic-gas error at block production time. The overestimate is safe:
        // if execution finds the sender already has code, `auto_delegate_codeless_sender`
        // is a no-op and the reserved gas flows into execution gas instead.
        let sender_auto_delegated = Self::sender_auto_delegated(&signed.tx().account_changes);
        let intrinsic = IntrinsicGas::compute(
            signed,
            self.eip8130_encoded(signed).as_ref(),
            &IntrinsicGasInput::new(nonce_key_first_use, sender_auto_delegated),
        )
        .map_err(|_| Self::eip8130_error("intrinsic gas computation failed"))?;
        if intrinsic.execution_gas_available(signed.tx().gas_limit).is_none() {
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

        Ok(Eip8130ValidationState {
            sender,
            payer,
            payer_balance_after_auth: payer_account.balance.saturating_sub(payer_auth_charge),
            sender_nonce,
            sender_bytecode_hash: sender_account.bytecode_hash,
            payer_auth: intrinsic.payer_auth,
        })
    }

    fn validate_eip8130_create_freshness(
        state: &dyn StateProvider,
        sender: Address,
        account: &reth_primitives_traits::Account,
    ) -> Result<(), InvalidPoolTransactionError> {
        if account.nonce != 0 {
            return Err(Self::eip8130_error("create sender nonce is non-zero"));
        }
        if Self::account_has_code(state, sender)
            .map_err(|error| Self::state_read_error(error, "sender code read failed"))?
        {
            return Err(Self::eip8130_error("create sender already has code"));
        }
        Ok(())
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

    fn account_has_code(
        state: &dyn StateProvider,
        address: Address,
    ) -> Result<bool, reth_storage_api::errors::ProviderError> {
        Ok(state
            .basic_account(&address)?
            .and_then(|account| account.bytecode_hash)
            .is_some_and(|hash| hash != KECCAK_EMPTY))
    }

    fn eip8130_encoded(&self, signed: &Eip8130Signed) -> Vec<u8> {
        let mut encoded = Vec::with_capacity(signed.encode_2718_len());
        signed.encode_2718(&mut encoded);
        encoded
    }

    fn map_tx_auth_error(error: TxAuthError) -> InvalidPoolTransactionError {
        tracing::debug!(error = ?error, "EIP-8130 actor authorization failed");
        let reason = match error {
            TxAuthError::Authorize(AuthorizeError::Authenticate(_)) => {
                "actor authentication failed"
            }
            TxAuthError::Authorize(AuthorizeError::Storage(_)) => {
                "account configuration read failed"
            }
            TxAuthError::Authorize(AuthorizeError::ZeroActor) => "actor id is zero",
            TxAuthError::Authorize(AuthorizeError::NotBound { .. }) => "actor is not bound",
            TxAuthError::Authorize(AuthorizeError::DefaultEoaRevoked { .. }) => {
                "default EOA actor is revoked"
            }
            TxAuthError::Authorize(AuthorizeError::Expired { .. }) => "actor credential expired",
            TxAuthError::Authorize(AuthorizeError::NestedSignatureScope { .. }) => {
                "delegate nested actor lacks SIGNATURE scope"
            }
            TxAuthError::SenderRecovery => "EOA sender recovery failed",
            TxAuthError::Scope { .. } => "actor scope insufficient",
            TxAuthError::AccountLocked => "account is locked",
            TxAuthError::DelegationUnauthorized => "delegation requires native-k1 admin self actor",
            TxAuthError::ConfigChainId { .. } => "config change targets a foreign chain",
            TxAuthError::ConfigSequence { .. } => "config change sequence mismatch",
            TxAuthError::Apply(apply) => Self::map_apply_error(apply),
        };
        Self::eip8130_error(reason)
    }

    /// Maps an [`ApplyError`] (surfaced via [`TxAuthError::Apply`] when an
    /// account change fails to apply against the admission overlay) to a named
    /// pool-rejection reason, so the create/config/delegation apply failures
    /// keep a specific user-visible reason rather than collapsing into one
    /// generic string. The structured error is still logged in
    /// [`Self::map_tx_auth_error`].
    fn map_apply_error(error: ApplyError) -> &'static str {
        match error {
            ApplyError::Storage(_) => "EIP-8130 state access failed",
            ApplyError::MalformedAuthorizeData => "actor change authorize data is malformed",
            ApplyError::InvalidAuthenticator => "actor authenticator is not canonical",
            ApplyError::MalformedPolicyData => "actor policy data is malformed",
            ApplyError::NotAnActor { .. } => "revoked actor is not authorized",
            ApplyError::NoInitialActors => "create entry has no initial actors",
            ApplyError::UnsortedInitialActors => "create initial actors are not strictly ascending",
            ApplyError::BytecodeTooLarge => "create bytecode exceeds the size limit",
            ApplyError::AlreadyCreated { .. } => "create account already exists",
            ApplyError::CreateAddressMismatch { .. } => "create address does not match the sender",
            ApplyError::InvalidCreatePosition => "create entry must be the only one, at index 0",
            ApplyError::MultipleDelegations => "at most one delegation is allowed",
            ApplyError::CreateAndDelegation => "create and delegation may not coexist",
            ApplyError::NonDelegatableCode { .. } => "delegation sender has non-delegation code",
            ApplyError::SequenceOverflow => "config change sequence overflow",
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
    /// state lookups. Enforces the Cobalt fork gate and the structural
    /// invariants listed in EIP-8130 § Validation and § Nonce-Free Mode.
    fn validate_eip8130_structural(
        &self,
        signed: &Eip8130Signed,
    ) -> Result<(), InvalidPoolTransactionError> {
        // Single read of the head-block timestamp so the fork gate and the
        // expiry check see the same value even when `on_new_head_block` updates
        // the atomic concurrently.
        let now = self.block_timestamp();
        // Fork gate: EIP-8130 (account abstraction) transactions are only
        // admissible to the pool once the Cobalt upgrade is active.
        if !self.chain_spec().is_cobalt_active_at_timestamp(now) {
            return Err(InvalidTransactionError::TxTypeNotSupported.into());
        }
        let local_chain_id = self.inner.chain_spec().chain().id();
        signed.validate_static(local_chain_id).map_err(InvalidPoolTransactionError::from)?;
        signed.validate_timestamp(now).map_err(InvalidPoolTransactionError::from)?;
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
            // Configured-actor path: leading 20 bytes are the authenticator address.
            if auth.len() < 20 {
                return Err(InvalidTransactionError::TxTypeNotSupported.into());
            }
            let authenticator = Address::from_slice(&auth[..20]);
            if !Self::authenticator_allowed_for_tx_path(&authenticator)
                || !Self::authenticator_payload_well_formed(&authenticator, &auth[20..])
            {
                return Err(InvalidTransactionError::TxTypeNotSupported.into());
            }
        }
        Ok(())
    }

    /// Ensures `payer_auth` is present iff a `payer` is set, and that its
    /// authenticator prefix sits in the live policy range (above the reserved
    /// floor, below the revoked sentinel).
    fn validate_payer_auth(signed: &Eip8130Signed) -> Result<(), InvalidPoolTransactionError> {
        let payer_present = signed.tx().payer.is_some();
        let auth = signed.payer_auth();
        // XOR: presence must match.
        if payer_present == auth.is_empty() {
            return Err(InvalidTransactionError::TxTypeNotSupported.into());
        }
        if payer_present {
            if auth.len() < 20 {
                return Err(InvalidTransactionError::TxTypeNotSupported.into());
            }
            let authenticator = Address::from_slice(&auth[..20]);
            if !Self::authenticator_allowed_for_tx_path(&authenticator)
                || !Self::authenticator_payload_well_formed(&authenticator, &auth[20..])
            {
                return Err(InvalidTransactionError::TxTypeNotSupported.into());
            }
        }
        Ok(())
    }

    /// Returns `true` when `authenticator` falls outside the live mempool policy
    /// range. Mirrors the check in [`Self::validate_initial_actors`] and
    /// [`Self::validate_actor_changes`] so all auth surfaces (`sender_auth`,
    /// `payer_auth`, `cfg.auth`, and per-actor authenticators) reject the reserved
    /// `< K1_AUTHENTICATOR` window identically. `address(0)` (the only address in
    /// that window) is the empty / "no actor configured" sentinel and is never a
    /// valid authenticator selector.
    fn authenticator_out_of_range(authenticator: &Address) -> bool {
        *authenticator < Eip8130Constants::K1_AUTHENTICATOR
    }

    /// Returns `true` when an authenticator selector may be used directly on the
    /// EIP-8130 transaction validation path.
    fn authenticator_allowed_for_tx_path(authenticator: &Address) -> bool {
        *authenticator == Eip8130Constants::K1_AUTHENTICATOR
            || Eip8130Contracts::is_canonical_authenticator(authenticator)
    }

    /// Performs cheap selector-specific wire checks that do not require running
    /// an authenticator. Native k1 must carry exactly `r || s || v`; delegated
    /// auth must be depth-1 and name a canonical nested authenticator.
    fn authenticator_payload_well_formed(authenticator: &Address, data: &[u8]) -> bool {
        if *authenticator == Eip8130Constants::K1_AUTHENTICATOR {
            return data.len() == 65;
        }
        if *authenticator == Eip8130Contracts::DELEGATE_AUTHENTICATOR {
            if data.len() < 40 {
                return false;
            }
            let nested = Address::from_slice(&data[20..40]);
            return nested != Eip8130Contracts::DELEGATE_AUTHENTICATOR
                && Self::authenticator_allowed_for_tx_path(&nested);
        }
        true
    }

    /// Enforces the interim total-account-changes admission cap
    /// ([`Eip8130Constants::MAX_ACCOUNT_CHANGES_PER_TX`]) and then the per-entry
    /// structural invariants via [`Self::validate_account_change_entries`].
    ///
    /// The total cap is an interim pool-only throttle that currently sits below
    /// the per-type [`Eip8130Constants::MAX_CONFIG_CHANGES_PER_TX`] cap, so the
    /// per-type cap is exercised directly against
    /// [`Self::validate_account_change_entries`] in tests rather than through
    /// this gate.
    fn validate_account_changes(
        signed: &Eip8130Signed,
        local_chain_id: u64,
    ) -> Result<(), InvalidPoolTransactionError> {
        // Conservative admission cap on the number of account changes a single
        // transaction may carry while the interleaved authorize-and-apply flow
        // beds in. Keeps the per-transaction admission work (and the overlay it
        // applies against) small and bounded.
        if signed.tx().account_changes.len() > Eip8130Constants::MAX_ACCOUNT_CHANGES_PER_TX {
            return Err(InvalidTransactionError::TxTypeNotSupported.into());
        }
        Self::validate_account_change_entries(signed, local_chain_id)
    }

    /// Walks `account_changes` and enforces the per-entry structural invariants:
    /// at most one `Create` (and only as the first entry), at most one
    /// `Delegation`, `ConfigChange` count capped at
    /// [`Eip8130Constants::MAX_CONFIG_CHANGES_PER_TX`], chain-binding on
    /// config changes, and per-entry well-formedness. Authenticator-address bounds
    /// and actor-id uniqueness are enforced on both `Create.initial_actors`
    /// and `ConfigChange.actor_changes` via [`Self::validate_initial_actors`]
    /// and [`Self::validate_actor_changes`] respectively.
    ///
    /// This is the structural walk independent of the interim total cap applied
    /// by [`Self::validate_account_changes`], so the per-type caps it enforces
    /// remain meaningful (and testable) if that interim cap is later raised.
    fn validate_account_change_entries(
        signed: &Eip8130Signed,
        local_chain_id: u64,
    ) -> Result<(), InvalidPoolTransactionError> {
        let mut create_count = 0usize;
        let mut delegation_count = 0usize;
        let mut config_count = 0usize;
        for (idx, change) in signed.tx().account_changes.iter().enumerate() {
            match change {
                AccountChange::Create(create) => {
                    create_count += 1;
                    if create_count > 1 || idx != 0 {
                        return Err(InvalidTransactionError::TxTypeNotSupported.into());
                    }
                    if create.code.is_empty()
                        || create.code.len() > Eip8130Constants::MAX_CODE_SIZE
                        || create.initial_actors.is_empty()
                    {
                        return Err(InvalidTransactionError::TxTypeNotSupported.into());
                    }
                    Self::validate_initial_actors(&create.initial_actors)?;
                }
                AccountChange::ConfigChange(cfg) => {
                    config_count += 1;
                    if config_count > Eip8130Constants::MAX_CONFIG_CHANGES_PER_TX {
                        return Err(InvalidTransactionError::TxTypeNotSupported.into());
                    }
                    if cfg.chain_id != 0 && cfg.chain_id != local_chain_id {
                        return Err(InvalidTransactionError::ChainIdMismatch.into());
                    }
                    if cfg.auth.len() < 20 {
                        return Err(InvalidTransactionError::TxTypeNotSupported.into());
                    }
                    let cfg_authenticator = Address::from_slice(&cfg.auth[..20]);
                    if !Self::authenticator_allowed_for_tx_path(&cfg_authenticator)
                        || !Self::authenticator_payload_well_formed(
                            &cfg_authenticator,
                            &cfg.auth[20..],
                        )
                    {
                        return Err(InvalidTransactionError::TxTypeNotSupported.into());
                    }
                    Self::validate_actor_changes(&cfg.actor_changes)?;
                }
                AccountChange::Delegation(_) => {
                    delegation_count += 1;
                    if delegation_count > 1 {
                        return Err(InvalidTransactionError::TxTypeNotSupported.into());
                    }
                    if create_count > 0 {
                        return Err(InvalidTransactionError::TxTypeNotSupported.into());
                    }
                }
            }
        }
        Ok(())
    }

    /// Validates `Create.initial_actors`: the slice length is bounded by
    /// [`Eip8130Constants::MAX_ACTORS_PER_ENTRY`] (anti-DoS cap on memory + work
    /// spent on duplicate detection), every `authenticator` is at or above the
    /// `K1_AUTHENTICATOR` floor (i.e. not the `address(0)` empty sentinel), no
    /// two entries share the same `actor_id`, and each entry's `policy_data` is
    /// structurally consistent with its `scope`: empty unless `SCOPE_POLICY` is
    /// set, otherwise exactly `manager (20) || commitment (32)` (52 bytes). The
    /// same consistency is enforced downstream in `authorize_actor`/`slice_policy`;
    /// checking it here rejects malformed creates before the expensive overlay
    /// path runs.
    fn validate_initial_actors(actors: &[InitialActor]) -> Result<(), InvalidPoolTransactionError> {
        if actors.len() > Eip8130Constants::MAX_ACTORS_PER_ENTRY {
            return Err(InvalidTransactionError::TxTypeNotSupported.into());
        }
        let mut previous = None;
        for actor in actors {
            if Self::authenticator_out_of_range(&actor.authenticator) {
                return Err(InvalidTransactionError::TxTypeNotSupported.into());
            }
            if previous.is_some_and(|previous| actor.actor_id <= previous) {
                return Err(InvalidTransactionError::TxTypeNotSupported.into());
            }
            let policy = actor.scope & Eip8130Constants::SCOPE_POLICY != 0;
            let expected_policy_len = if policy { Eip8130Constants::POLICY_DATA_LEN } else { 0 };
            if actor.policy_data.len() != expected_policy_len {
                return Err(InvalidTransactionError::TxTypeNotSupported.into());
            }
            previous = Some(actor.actor_id);
        }
        Ok(())
    }

    /// Validates `ConfigChange.actor_changes`: the slice is bounded by
    /// [`Eip8130Constants::MAX_ACTOR_CHANGES_PER_CONFIG`], plus the
    /// reserved-window authenticator bound for the *new* actor of each
    /// `Authorize`. The authenticator lives in the ABI-encoded `data`
    /// (`abi.encode(ActorConfig, bytes)`), where `ActorConfig.authenticator` is
    /// the right-aligned address in the first 32-byte word, so it is read from
    /// `data[12..32]` without a full decode (the leading 12 padding bytes must be
    /// zero, matching ABI encoding); the remaining structure is validated where
    /// the change is applied. A `Revoke` carries empty `data` and names no
    /// authenticator, so only the cap applies.
    ///
    /// Per EIP-8130 a config change MAY authorize a non-canonical authenticator
    /// (for in-EVM use such as recovery keys); only the reserved window
    /// (`< K1_AUTHENTICATOR`, i.e. the `address(0)` empty sentinel) is rejected
    /// here, matching the bound applied to the other auth surfaces. A `Revoke`
    /// names no authenticator and MUST carry empty `data`; a non-empty `data` is
    /// malformed and rejected at the gate.
    fn validate_actor_changes(changes: &[ActorChange]) -> Result<(), InvalidPoolTransactionError> {
        if changes.len() > Eip8130Constants::MAX_ACTOR_CHANGES_PER_CONFIG {
            return Err(InvalidTransactionError::TxTypeNotSupported.into());
        }
        let mut seen = BTreeSet::new();
        for change in changes {
            match change.change_type {
                ActorChangeType::Authorize => {
                    // `data` = `abi.encode(ActorConfig, bytes)`; the new actor's
                    // authenticator is the right-aligned address in the first word.
                    if change.data.len() < 32 {
                        return Err(InvalidTransactionError::TxTypeNotSupported.into());
                    }
                    // The first word is an ABI-encoded `address`: the leading 12
                    // bytes are zero padding. Reject dirty upper bits so the gate
                    // and a strict ABI decoder downstream agree on validity.
                    if change.data[..12].iter().any(|&b| b != 0) {
                        return Err(InvalidTransactionError::TxTypeNotSupported.into());
                    }
                    let authenticator = Address::from_slice(&change.data[12..32]);
                    if Self::authenticator_out_of_range(&authenticator) {
                        return Err(InvalidTransactionError::TxTypeNotSupported.into());
                    }
                }
                ActorChangeType::Revoke => {
                    if !change.data.is_empty() {
                        return Err(InvalidTransactionError::TxTypeNotSupported.into());
                    }
                }
            }
            if !seen.insert(change.actor_id) {
                return Err(InvalidTransactionError::TxTypeNotSupported.into());
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
        AccountChange, ActorChange, ActorChangeType, BasePrimitives, BaseTransactionSigned,
        BaseTxEnvelope, ConfigChange, CreateEntry, Delegation, Eip8130Constants, Eip8130Signed,
        InitialActor, TxDeposit, TxEip8130,
    };
    use base_execution_chainspec::{BaseChainSpec, BaseChainSpecBuilder};
    use base_execution_eip8130::{AccountChangeApplier, ConfigChangeAuthorizer};
    use base_execution_evm::BaseEvmConfig;
    use base_test_utils::Account;
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

    /// Builds a [`BaseTransactionValidator`] against a Cobalt-activated mainnet chain spec with
    /// no accounts seeded. EIP-8130 admission is fork-gated on Cobalt, so the structural-gate
    /// tests run with Cobalt active (at genesis) to exercise the checks past the fork gate.
    fn build_test_validator() -> TestValidator {
        let chain_spec = Arc::new(BaseChainSpecBuilder::base_mainnet().cobalt_activated().build());
        build_test_validator_with_spec(chain_spec)
    }

    /// Builds a Cobalt-activated validator with one canonical account seeded.
    fn build_test_validator_with_account(
        address: Address,
        account: ExtendedAccount,
    ) -> TestValidator {
        let chain_spec = Arc::new(BaseChainSpecBuilder::base_mainnet().cobalt_activated().build());
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
            expiry: 0,
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
    fn rejects_eip8130_before_cobalt_activation() {
        // Mainnet leaves Cobalt unscheduled, so the fork gate rejects an otherwise
        // structurally valid EIP-8130 transaction regardless of its contents.
        let validator = build_test_validator_with_spec(Arc::new(BaseChainSpec::mainnet()));
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
            expiry: 0,
            ..minimal_valid_eoa_tx()
        };
        let signed = sign_eoa_eip8130(tx);
        assert_unsupported(validator.validate_eip8130_structural(&signed));
    }

    #[test]
    fn rejects_eip8130_nonce_free_with_nonzero_sequence() {
        let validator = build_test_validator();
        let tx = TxEip8130 {
            nonce_key: Eip8130Constants::NONCE_KEY_MAX,
            nonce_sequence: 1,
            expiry: 5,
            ..minimal_valid_eoa_tx()
        };
        let signed = sign_eoa_eip8130(tx);
        assert_unsupported(validator.validate_eip8130_structural(&signed));
    }

    #[test]
    fn rejects_eip8130_nonce_free_already_expired() {
        // Advance the validator's tracked block timestamp to 100 so that expiry=50
        // is strictly in the past; the default fixture sits at timestamp 0 where
        // there is no way to express "already expired".
        let validator = build_test_validator();
        let header = alloy_consensus::Header { timestamp: 100, ..Default::default() };
        validator.update_l1_block_info::<_, TxEip1559>(&header, None);
        let tx = TxEip8130 {
            nonce_key: Eip8130Constants::NONCE_KEY_MAX,
            nonce_sequence: 0,
            expiry: 50,
            ..minimal_valid_eoa_tx()
        };
        let signed = sign_eoa_eip8130(tx);
        assert_unsupported(validator.validate_eip8130_structural(&signed));
    }

    #[test]
    fn rejects_eip8130_nonce_free_expiry_too_far_in_future() {
        let validator = build_test_validator();
        // block_timestamp returns 0 by default; cap is NONCE_FREE_MAX_EXPIRY_WINDOW (10).
        let tx = TxEip8130 {
            nonce_key: Eip8130Constants::NONCE_KEY_MAX,
            nonce_sequence: 0,
            expiry: Eip8130Constants::NONCE_FREE_MAX_EXPIRY_WINDOW + 1,
            ..minimal_valid_eoa_tx()
        };
        let signed = sign_eoa_eip8130(tx);
        assert_unsupported(validator.validate_eip8130_structural(&signed));
    }

    #[test]
    fn accepts_eip8130_nonce_free_at_expiry_window_edge() {
        let validator = build_test_validator();
        let tx = TxEip8130 {
            nonce_key: Eip8130Constants::NONCE_KEY_MAX,
            nonce_sequence: 0,
            expiry: Eip8130Constants::NONCE_FREE_MAX_EXPIRY_WINDOW,
            ..minimal_valid_eoa_tx()
        };
        let signed = sign_eoa_eip8130(tx);
        assert!(validator.validate_eip8130_structural(&signed).is_ok());
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

    /// Returns an authenticator address comfortably above the `K1_AUTHENTICATOR`
    /// floor.
    fn ok_authenticator() -> Address {
        Address::repeat_byte(0x42)
    }

    fn make_initial_actor(actor_id_byte: u8) -> InitialActor {
        InitialActor::owner(B256::repeat_byte(actor_id_byte), ok_authenticator())
    }

    /// Builds an `Authorize` actor-change whose ABI-encoded `data` carries
    /// `authenticator` in the first word (`ActorConfig.authenticator`), matching
    /// the layout the validator reads from `data[12..32]`.
    fn make_authorize_change(actor_id: B256, authenticator: Address) -> ActorChange {
        let mut data = vec![0u8; 160];
        data[12..32].copy_from_slice(authenticator.as_slice());
        ActorChange { change_type: ActorChangeType::Authorize, actor_id, data: Bytes::from(data) }
    }

    /// Builds a `Revoke` actor-change. Per EIP-8130 a revoke names no
    /// authenticator and carries empty `data`.
    fn make_revoke_change(actor_id: B256) -> ActorChange {
        ActorChange { change_type: ActorChangeType::Revoke, actor_id, data: Bytes::new() }
    }

    fn make_valid_create_entry() -> CreateEntry {
        CreateEntry {
            user_salt: B256::ZERO,
            code: Bytes::from_static(&[0x60, 0x00]),
            initial_actors: vec![make_initial_actor(0x01)],
        }
    }

    #[test]
    fn rejects_eip8130_create_not_at_index_zero() {
        let tx = TxEip8130 {
            account_changes: vec![
                AccountChange::Delegation(Delegation { target: Address::repeat_byte(0x33) }),
                AccountChange::Create(make_valid_create_entry()),
            ],
            ..minimal_valid_eoa_tx()
        };
        assert_unsupported(TestValidator::validate_account_changes(
            &sign_eoa_eip8130(tx),
            test_chain_id(),
        ));
    }

    #[test]
    fn rejects_eip8130_multiple_create_entries() {
        let tx = TxEip8130 {
            account_changes: vec![
                AccountChange::Create(make_valid_create_entry()),
                AccountChange::Create(make_valid_create_entry()),
            ],
            ..minimal_valid_eoa_tx()
        };
        assert_unsupported(TestValidator::validate_account_changes(
            &sign_eoa_eip8130(tx),
            test_chain_id(),
        ));
    }

    #[test]
    fn rejects_eip8130_create_with_empty_code() {
        let mut entry = make_valid_create_entry();
        entry.code = Bytes::new();
        let tx = TxEip8130 {
            account_changes: vec![AccountChange::Create(entry)],
            ..minimal_valid_eoa_tx()
        };
        assert_unsupported(TestValidator::validate_account_changes(
            &sign_eoa_eip8130(tx),
            test_chain_id(),
        ));
    }

    #[test]
    fn rejects_eip8130_create_with_no_initial_actors() {
        let mut entry = make_valid_create_entry();
        entry.initial_actors.clear();
        let tx = TxEip8130 {
            account_changes: vec![AccountChange::Create(entry)],
            ..minimal_valid_eoa_tx()
        };
        assert_unsupported(TestValidator::validate_account_changes(
            &sign_eoa_eip8130(tx),
            test_chain_id(),
        ));
    }

    #[test]
    fn rejects_eip8130_create_with_duplicate_actor_ids() {
        let mut entry = make_valid_create_entry();
        entry.initial_actors.push(make_initial_actor(0x01));
        let tx = TxEip8130 {
            account_changes: vec![AccountChange::Create(entry)],
            ..minimal_valid_eoa_tx()
        };
        assert_unsupported(TestValidator::validate_account_changes(
            &sign_eoa_eip8130(tx),
            test_chain_id(),
        ));
    }

    #[test]
    fn rejects_eip8130_create_with_actor_authenticator_below_floor() {
        let mut entry = make_valid_create_entry();
        entry.initial_actors[0].authenticator = Address::ZERO;
        let tx = TxEip8130 {
            account_changes: vec![AccountChange::Create(entry)],
            ..minimal_valid_eoa_tx()
        };
        assert_unsupported(TestValidator::validate_account_changes(
            &sign_eoa_eip8130(tx),
            test_chain_id(),
        ));
    }

    #[test]
    fn rejects_eip8130_create_with_policy_data_on_ungated_actor() {
        let mut entry = make_valid_create_entry();
        entry.initial_actors[0].scope = 0;
        entry.initial_actors[0].policy_data = vec![0u8; Eip8130Constants::POLICY_DATA_LEN].into();
        let tx = TxEip8130 {
            account_changes: vec![AccountChange::Create(entry)],
            ..minimal_valid_eoa_tx()
        };
        assert_unsupported(TestValidator::validate_account_changes(
            &sign_eoa_eip8130(tx),
            test_chain_id(),
        ));
    }

    #[test]
    fn rejects_eip8130_create_with_wrong_length_policy_data() {
        let mut entry = make_valid_create_entry();
        entry.initial_actors[0].scope = Eip8130Constants::SCOPE_POLICY;
        entry.initial_actors[0].policy_data =
            vec![0u8; Eip8130Constants::POLICY_DATA_LEN - 1].into();
        let tx = TxEip8130 {
            account_changes: vec![AccountChange::Create(entry)],
            ..minimal_valid_eoa_tx()
        };
        assert_unsupported(TestValidator::validate_account_changes(
            &sign_eoa_eip8130(tx),
            test_chain_id(),
        ));
    }

    #[test]
    fn accepts_eip8130_create_with_well_formed_policy_data() {
        let mut entry = make_valid_create_entry();
        entry.initial_actors[0].scope = Eip8130Constants::SCOPE_POLICY;
        entry.initial_actors[0].policy_data = vec![0u8; Eip8130Constants::POLICY_DATA_LEN].into();
        let tx = TxEip8130 {
            account_changes: vec![AccountChange::Create(entry)],
            ..minimal_valid_eoa_tx()
        };
        assert!(
            TestValidator::validate_account_changes(&sign_eoa_eip8130(tx), test_chain_id(),)
                .is_ok()
        );
    }

    #[test]
    fn rejects_eip8130_create_with_too_many_initial_actors() {
        let mut entry = make_valid_create_entry();
        entry.initial_actors.clear();
        for i in 0..(Eip8130Constants::MAX_ACTORS_PER_ENTRY + 1) {
            entry.initial_actors.push(make_initial_actor(i as u8));
        }
        let tx = TxEip8130 {
            account_changes: vec![AccountChange::Create(entry)],
            ..minimal_valid_eoa_tx()
        };
        assert_unsupported(TestValidator::validate_account_changes(
            &sign_eoa_eip8130(tx),
            test_chain_id(),
        ));
    }

    #[test]
    fn accepts_eip8130_create_with_exactly_max_initial_actors() {
        let mut entry = make_valid_create_entry();
        entry.initial_actors.clear();
        for i in 0..Eip8130Constants::MAX_ACTORS_PER_ENTRY {
            entry.initial_actors.push(make_initial_actor(i as u8));
        }
        let tx = TxEip8130 {
            account_changes: vec![AccountChange::Create(entry)],
            ..minimal_valid_eoa_tx()
        };
        assert!(
            TestValidator::validate_account_changes(&sign_eoa_eip8130(tx), test_chain_id()).is_ok()
        );
    }

    fn make_valid_config_change() -> ConfigChange {
        let mut auth = Eip8130Constants::K1_AUTHENTICATOR.to_vec();
        auth.extend_from_slice(&[0u8; 65]);
        ConfigChange {
            chain_id: 0,
            sequence: 0,
            actor_changes: Vec::new(),
            auth: Bytes::from(auth),
        }
    }

    #[test]
    fn rejects_eip8130_config_change_with_foreign_chain_id() {
        let cfg = ConfigChange { chain_id: test_chain_id() + 1, ..make_valid_config_change() };
        let tx = TxEip8130 {
            account_changes: vec![AccountChange::ConfigChange(cfg)],
            ..minimal_valid_eoa_tx()
        };
        let result =
            TestValidator::validate_account_changes(&sign_eoa_eip8130(tx), test_chain_id());
        match result {
            Err(InvalidPoolTransactionError::Consensus(
                InvalidTransactionError::ChainIdMismatch,
            )) => {}
            other => panic!("expected ChainIdMismatch, got {other:?}"),
        }
    }

    #[test]
    fn rejects_eip8130_config_change_with_short_auth() {
        let cfg =
            ConfigChange { auth: Bytes::from_static(&[0u8; 5]), ..make_valid_config_change() };
        let tx = TxEip8130 {
            account_changes: vec![AccountChange::ConfigChange(cfg)],
            ..minimal_valid_eoa_tx()
        };
        assert_unsupported(TestValidator::validate_account_changes(
            &sign_eoa_eip8130(tx),
            test_chain_id(),
        ));
    }

    #[test]
    fn rejects_eip8130_config_change_with_duplicate_actor_ids() {
        let dup_id = B256::repeat_byte(0x07);
        let cfg = ConfigChange {
            actor_changes: vec![
                make_authorize_change(dup_id, ok_authenticator()),
                make_authorize_change(dup_id, ok_authenticator()),
            ],
            ..make_valid_config_change()
        };
        let tx = TxEip8130 {
            account_changes: vec![AccountChange::ConfigChange(cfg)],
            ..minimal_valid_eoa_tx()
        };
        assert_unsupported(TestValidator::validate_account_changes(
            &sign_eoa_eip8130(tx),
            test_chain_id(),
        ));
    }

    #[test]
    fn accepts_eip8130_config_change_with_exactly_max_actor_changes() {
        let actor_changes = (0..Eip8130Constants::MAX_ACTOR_CHANGES_PER_CONFIG)
            .map(|i| make_authorize_change(B256::repeat_byte(i as u8), ok_authenticator()))
            .collect();
        let cfg = ConfigChange { actor_changes, ..make_valid_config_change() };
        let tx = TxEip8130 {
            account_changes: vec![AccountChange::ConfigChange(cfg)],
            ..minimal_valid_eoa_tx()
        };
        assert!(
            TestValidator::validate_account_changes(&sign_eoa_eip8130(tx), test_chain_id()).is_ok()
        );
    }

    #[test]
    fn rejects_eip8130_config_change_with_too_many_actor_changes() {
        let actor_changes = (0..(Eip8130Constants::MAX_ACTOR_CHANGES_PER_CONFIG + 1))
            .map(|i| make_authorize_change(B256::repeat_byte(i as u8), ok_authenticator()))
            .collect();
        let cfg = ConfigChange { actor_changes, ..make_valid_config_change() };
        let tx = TxEip8130 {
            account_changes: vec![AccountChange::ConfigChange(cfg)],
            ..minimal_valid_eoa_tx()
        };
        assert_unsupported(TestValidator::validate_account_changes(
            &sign_eoa_eip8130(tx),
            test_chain_id(),
        ));
    }

    // A `Revoke` carries empty `data` and names no authenticator, so it must
    // pass `validate_actor_changes` (no authenticator bound is applied).
    #[test]
    fn accepts_eip8130_config_change_with_valid_revoke() {
        let cfg = ConfigChange {
            actor_changes: vec![make_revoke_change(B256::repeat_byte(0x01))],
            ..make_valid_config_change()
        };
        let tx = TxEip8130 {
            account_changes: vec![AccountChange::ConfigChange(cfg)],
            ..minimal_valid_eoa_tx()
        };
        assert!(
            TestValidator::validate_account_changes(&sign_eoa_eip8130(tx), test_chain_id()).is_ok()
        );
    }

    // A `Revoke` with non-empty `data` is malformed and rejected at the gate.
    #[test]
    fn rejects_eip8130_config_change_with_nonempty_revoke_data() {
        let cfg = ConfigChange {
            actor_changes: vec![ActorChange {
                change_type: ActorChangeType::Revoke,
                actor_id: B256::repeat_byte(0x01),
                data: Bytes::from_static(&[0xaa]),
            }],
            ..make_valid_config_change()
        };
        let tx = TxEip8130 {
            account_changes: vec![AccountChange::ConfigChange(cfg)],
            ..minimal_valid_eoa_tx()
        };
        assert_unsupported(TestValidator::validate_account_changes(
            &sign_eoa_eip8130(tx),
            test_chain_id(),
        ));
    }

    // The first `data` word is an ABI-encoded `address`; non-zero padding in the
    // leading 12 bytes is malformed and rejected at the gate.
    #[test]
    fn rejects_eip8130_config_change_with_dirty_authenticator_padding() {
        let mut change = make_authorize_change(B256::repeat_byte(0x01), ok_authenticator());
        let mut data = change.data.to_vec();
        data[0] = 0x01;
        change.data = Bytes::from(data);
        let cfg = ConfigChange { actor_changes: vec![change], ..make_valid_config_change() };
        let tx = TxEip8130 {
            account_changes: vec![AccountChange::ConfigChange(cfg)],
            ..minimal_valid_eoa_tx()
        };
        assert_unsupported(TestValidator::validate_account_changes(
            &sign_eoa_eip8130(tx),
            test_chain_id(),
        ));
    }

    // Duplicate `actor_id` detection spans mixed `Authorize`/`Revoke` entries.
    #[test]
    fn rejects_eip8130_config_change_with_duplicate_actor_ids_mixed() {
        let dup_id = B256::repeat_byte(0x07);
        let cfg = ConfigChange {
            actor_changes: vec![
                make_authorize_change(dup_id, ok_authenticator()),
                make_revoke_change(dup_id),
            ],
            ..make_valid_config_change()
        };
        let tx = TxEip8130 {
            account_changes: vec![AccountChange::ConfigChange(cfg)],
            ..minimal_valid_eoa_tx()
        };
        assert_unsupported(TestValidator::validate_account_changes(
            &sign_eoa_eip8130(tx),
            test_chain_id(),
        ));
    }

    #[test]
    fn rejects_eip8130_too_many_config_changes() {
        // The interim total-account-changes cap currently sits below
        // `MAX_CONFIG_CHANGES_PER_TX`, so exercise the per-type config cap
        // directly against the structural entry walk (bypassing the total gate)
        // to keep that invariant covered independently.
        let count = Eip8130Constants::MAX_CONFIG_CHANGES_PER_TX + 1;
        let account_changes =
            (0..count).map(|_| AccountChange::ConfigChange(make_valid_config_change())).collect();
        let tx = TxEip8130 { account_changes, ..minimal_valid_eoa_tx() };
        assert_unsupported(TestValidator::validate_account_change_entries(
            &sign_eoa_eip8130(tx),
            test_chain_id(),
        ));
    }

    #[test]
    fn accepts_eip8130_exactly_max_config_changes_in_structural_walk() {
        // Exactly `MAX_CONFIG_CHANGES_PER_TX` config changes pass the per-type
        // cap in the structural walk (the interim total cap is applied
        // separately by `validate_account_changes`).
        let count = Eip8130Constants::MAX_CONFIG_CHANGES_PER_TX;
        let account_changes =
            (0..count).map(|_| AccountChange::ConfigChange(make_valid_config_change())).collect();
        let tx = TxEip8130 { account_changes, ..minimal_valid_eoa_tx() };
        assert!(
            TestValidator::validate_account_change_entries(&sign_eoa_eip8130(tx), test_chain_id())
                .is_ok()
        );
    }

    #[test]
    fn accepts_eip8130_with_exactly_max_account_changes() {
        let count = Eip8130Constants::MAX_ACCOUNT_CHANGES_PER_TX;
        let account_changes =
            (0..count).map(|_| AccountChange::ConfigChange(make_valid_config_change())).collect();
        let tx = TxEip8130 { account_changes, ..minimal_valid_eoa_tx() };
        assert!(
            TestValidator::validate_account_changes(&sign_eoa_eip8130(tx), test_chain_id(),)
                .is_ok()
        );
    }

    #[test]
    fn rejects_eip8130_too_many_account_changes() {
        let count = Eip8130Constants::MAX_ACCOUNT_CHANGES_PER_TX + 1;
        let account_changes =
            (0..count).map(|_| AccountChange::ConfigChange(make_valid_config_change())).collect();
        let tx = TxEip8130 { account_changes, ..minimal_valid_eoa_tx() };
        assert_unsupported(TestValidator::validate_account_changes(
            &sign_eoa_eip8130(tx),
            test_chain_id(),
        ));
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

    #[test]
    fn rejects_eip8130_create_and_delegation_coexistence() {
        // A transaction must not contain both a Create and a Delegation entry.
        // These are mutually exclusive: create establishes a fresh account
        // (code installed by the protocol) while delegation modifies an
        // existing account's code pointer.
        let account_changes = vec![
            AccountChange::Create(make_valid_create_entry()),
            AccountChange::Delegation(Delegation { target: Address::repeat_byte(0x55) }),
        ];
        let tx = TxEip8130 { account_changes, ..minimal_valid_eoa_tx() };
        assert_unsupported(TestValidator::validate_account_changes(
            &sign_eoa_eip8130(tx),
            test_chain_id(),
        ));
    }

    #[test]
    fn rejects_eip8130_create_config_and_delegation_coexistence() {
        // Same invariant with a config change interleaved between the create and
        // the delegation — the delegation is still rejected.
        let account_changes = vec![
            AccountChange::Create(make_valid_create_entry()),
            AccountChange::ConfigChange(make_valid_config_change()),
            AccountChange::Delegation(Delegation { target: Address::repeat_byte(0x55) }),
        ];
        let tx = TxEip8130 { account_changes, ..minimal_valid_eoa_tx() };
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
            expiry: 0,
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

    /// Pool-side coverage for the [`OverlayPrecompileStorage`] admission path:
    /// a counterfactual `Create` followed by a `ConfigChange` in the same
    /// transaction must be admitted, which can only happen if the overlay
    /// buffers the create's writes so the config change authorizes against the
    /// freshly-created account's evolving state (the create installs an
    /// unrestricted owner; the config change then advances the multichain
    /// channel from sequence 0). If the overlay did not persist the create's
    /// storage transitions, the config change would fail with `NotBound`.
    #[test]
    fn admits_eip8130_create_then_config_change_via_overlay() {
        let signer = PrivateKeySigner::random();
        let signer_addr = signer.address();
        let actor_id = {
            let mut id = [0u8; 32];
            id[..20].copy_from_slice(signer_addr.as_slice());
            B256::from_slice(&id)
        };
        let initial_actors =
            vec![InitialActor::owner(actor_id, Eip8130Constants::K1_AUTHENTICATOR)];
        let create = CreateEntry {
            user_salt: B256::ZERO,
            // Non-empty code: the structural gate rejects create.code.is_empty(),
            // so empty code would never reach validate_eip8130_full in production.
            // Using minimal valid bytecode (PUSH1 0x00) also affects the CREATE2
            // address derivation, exercising a more realistic admitted scenario.
            code: Bytes::from_static(&[0x60, 0x00]),
            initial_actors: initial_actors.clone(),
        };
        let derived = AccountChangeApplier::compute_address(
            create.user_salt,
            create.code.as_ref(),
            &initial_actors,
        )
        .expect("address derivation");

        // Multichain (chain_id == 0) config change at the channel's first
        // sequence, signed by the create's initial owner and bound to the
        // counterfactual address.
        let mut config = ConfigChange {
            chain_id: 0,
            sequence: 0,
            actor_changes: Vec::new(),
            auth: Bytes::new(),
        };
        let config_digest = ConfigChangeAuthorizer::signed_actor_changes_digest(derived, &config);
        config.auth = k1_auth_blob(&signer, config_digest);

        let tx = TxEip8130 {
            chain_id: test_chain_id(),
            sender: Some(derived),
            nonce_key: U256::ZERO,
            nonce_sequence: 0,
            expiry: 0,
            max_priority_fee_per_gas: 0,
            max_fee_per_gas: 100,
            gas_limit: 1_000_000,
            account_changes: vec![
                AccountChange::Create(create),
                AccountChange::ConfigChange(config),
            ],
            calls: Vec::new(),
            metadata: Bytes::new(),
            payer: None,
        };
        let sender_auth = k1_auth_blob(&signer, tx.sender_signature_hash());
        let signed = Eip8130Signed::new(tx, sender_auth, Bytes::new());

        // Fund the counterfactual address so the self-paid fee check passes; it
        // is still "fresh" (nonce 0, no code) for the create freshness gate.
        let validator = build_test_validator_with_account(
            derived,
            ExtendedAccount::new(0, U256::from(1_000_000_000_000_000_000u64)),
        );

        let state = validator
            .validate_eip8130_full(&signed)
            .expect("create + config change must be admitted via the overlay");
        assert_eq!(state.sender, derived);
        assert_eq!(state.payer, derived, "self-paid create");
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
    }

    #[test]
    fn sender_auto_delegated_budgets_clear_delegation() {
        // No account changes: a code-less sender is auto-delegated at execution,
        // so the mempool must budget the deposit.
        assert!(TestValidator::sender_auto_delegated(&[]));

        // A zero-target `Delegation` (a clear) leaves the sender code-less at
        // execution, so auto-delegation re-fires and charges a *second* deposit.
        // The mempool must still budget it — matching xenoliss's reported case.
        assert!(TestValidator::sender_auto_delegated(&[AccountChange::Delegation(Delegation {
            target: Address::ZERO,
        })]));

        // A non-zero `Delegation` leaves the sender with code, so execution's
        // `auto_delegate_codeless_sender` is a no-op — no auto-delegation deposit.
        assert!(!TestValidator::sender_auto_delegated(&[AccountChange::Delegation(Delegation {
            target: Address::repeat_byte(0x11),
        })]));

        // A `Create` installs code, so auto-delegation does not fire.
        assert!(!TestValidator::sender_auto_delegated(&[AccountChange::Create(
            make_valid_create_entry(),
        )]));
    }
}
