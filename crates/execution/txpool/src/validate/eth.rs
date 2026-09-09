//! Ethereum transaction validator.

use std::{
    fmt,
    sync::{
        Arc,
        atomic::{AtomicBool, AtomicU64, AtomicUsize},
    },
};

use alloy_eips::{BlockId, eip1559::ETHEREUM_BLOCK_GAS_LIMIT_30M, eip7840::BlobParams};
use alloy_hardforks::EthereumHardforks;
use alloy_primitives::{Address, U256};
use alloy_rlp::Encodable;
use base_common_chain_config::{BaseChainSpec, ChainSpecProvider};
use base_common_types_chain::{
    BaseBlock, BlockHeader, Transaction, Typed2718,
    constants::{
        EIP1559_TX_TYPE_ID, EIP2930_TX_TYPE_ID, EIP4844_TX_TYPE_ID, EIP7702_TX_TYPE_ID,
        KECCAK_EMPTY, LEGACY_TX_TYPE_ID,
    },
};
use base_evm_context::Cfg;
use base_execution_evm_blocks::BaseEvmConfig;
use reth_primitives_traits::{
    Account, GotExpected, SealedBlock, transaction::error::InvalidTransactionError,
};
use reth_storage_api::{
    AccountInfoReader, BlockReaderIdExt, BytecodeReader, StateProviderBox, StateProviderFactory,
    errors::ProviderError,
};
use reth_tasks::Runtime;

use super::constants::DEFAULT_MAX_TX_INPUT_BYTES;
use crate::{
    LocalTransactionConfig, TransactionValidationOutcome, TransactionValidationTaskExecutor,
    TransactionValidator,
    error::{
        Eip4844PoolTransactionError, Eip7702PoolTransactionError, InvalidPoolTransactionError,
    },
    traits::TransactionOrigin,
    validate::ValidTransaction,
};

/// Additional stateless validation function signature.
///
/// Receives the transaction origin and a reference to the transaction. Returns `Ok(())` if the
/// transaction passes or `Err` to reject it.
pub type StatelessValidationFn<T> =
    Arc<dyn Fn(TransactionOrigin, &T) -> Result<(), InvalidPoolTransactionError> + Send + Sync>;

/// Additional stateful validation function signature.
///
/// Receives the transaction origin, a reference to the transaction, and an account state reader.
/// Returns `Ok(())` if the transaction passes or `Err` to reject it.
pub type StatefulValidationFn<T> = Arc<
    dyn Fn(TransactionOrigin, &T, &dyn AccountInfoReader) -> Result<(), InvalidPoolTransactionError>
        + Send
        + Sync,
>;

/// A [`TransactionValidator`] implementation that validates ethereum transaction.
///
/// It supports all known ethereum transaction types:
/// - Legacy
/// - EIP-2718
/// - EIP-1559
/// - EIP-4844
/// - EIP-7702
///
/// And enforces additional constraints such as:
/// - Maximum transaction size
/// - Maximum gas limit
///
/// And adheres to the configured [`LocalTransactionConfig`].
pub struct EthTransactionValidator<Client> {
    /// This type fetches account info from the db
    client: Client,
    /// The chain ID transactions must use.
    chain_id: u64,
    /// tracks activated forks relevant for transaction validation
    fork_tracker: ForkTracker,
    /// Fork indicator whether we are using EIP-2718 type transactions.
    eip2718: bool,
    /// Fork indicator whether we are using EIP-1559 type transactions.
    eip1559: bool,
    /// Fork indicator whether we are using EIP-4844 blob transactions.
    eip4844: bool,
    /// Fork indicator whether we are using EIP-7702 type transactions.
    eip7702: bool,
    /// The current max gas limit
    block_gas_limit: AtomicU64,
    /// The current tx fee cap limit in wei locally submitted into the pool.
    tx_fee_cap: Option<u128>,
    /// Minimum priority fee to enforce for acceptance into the pool.
    minimum_priority_fee: Option<u128>,
    /// How to handle [`TransactionOrigin::Local`](TransactionOrigin) transactions.
    local_transactions_config: LocalTransactionConfig,
    /// Maximum size in bytes a single transaction can have in order to be accepted into the pool.
    max_tx_input_bytes: usize,
    /// Maximum gas limit for individual transactions
    max_tx_gas_limit: Option<u64>,
    /// Disable balance checks during transaction validation
    disable_balance_check: bool,
    /// EVM configuration for fetching execution limits
    evm_config: BaseEvmConfig,
    /// Bitmap of custom transaction types that are allowed.
    other_tx_types: U256,
    /// Optional additional stateless validation check applied at the end of
    /// [`validate_stateless`](Self::validate_stateless).
    additional_stateless_validation: Option<StatelessValidationFn<crate::BasePooledTransaction>>,
    /// Optional additional stateful validation check applied at the end of
    /// [`validate_stateful`](Self::validate_stateful).
    additional_stateful_validation: Option<StatefulValidationFn<crate::BasePooledTransaction>>,
}

impl<Client> fmt::Debug for EthTransactionValidator<Client> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("EthTransactionValidator")
            .field("fork_tracker", &self.fork_tracker)
            .field("eip2718", &self.eip2718)
            .field("eip1559", &self.eip1559)
            .field("eip4844", &self.eip4844)
            .field("eip7702", &self.eip7702)
            .field("block_gas_limit", &self.block_gas_limit)
            .field("tx_fee_cap", &self.tx_fee_cap)
            .field("minimum_priority_fee", &self.minimum_priority_fee)
            .field("max_tx_input_bytes", &self.max_tx_input_bytes)
            .field("max_tx_gas_limit", &self.max_tx_gas_limit)
            .field("disable_balance_check", &self.disable_balance_check)
            .field(
                "additional_stateless_validation",
                &self.additional_stateless_validation.as_ref().map(|_| "..."),
            )
            .field(
                "additional_stateful_validation",
                &self.additional_stateful_validation.as_ref().map(|_| "..."),
            )
            .finish()
    }
}

impl<Client> EthTransactionValidator<Client> {
    /// Returns the configured chain spec
    pub fn chain_spec(&self) -> Arc<BaseChainSpec>
    where
        Client: ChainSpecProvider,
    {
        self.client().chain_spec()
    }

    /// Returns the configured chain id
    pub const fn chain_id(&self) -> u64 {
        self.chain_id
    }

    /// Returns the configured client
    pub const fn client(&self) -> &Client {
        &self.client
    }

    /// Returns the tracks activated forks relevant for transaction validation
    pub const fn fork_tracker(&self) -> &ForkTracker {
        &self.fork_tracker
    }

    /// Returns the EVM config used for transaction validation.
    pub const fn evm_config(&self) -> &BaseEvmConfig {
        &self.evm_config
    }

    /// Returns if there are EIP-2718 type transactions
    pub const fn eip2718(&self) -> bool {
        self.eip2718
    }

    /// Returns if there are EIP-1559 type transactions
    pub const fn eip1559(&self) -> bool {
        self.eip1559
    }

    /// Returns if there are EIP-4844 blob transactions
    pub const fn eip4844(&self) -> bool {
        self.eip4844
    }

    /// Returns if there are EIP-7702 type transactions
    pub const fn eip7702(&self) -> bool {
        self.eip7702
    }

    /// Returns the current tx fee cap limit in wei locally submitted into the pool
    pub const fn tx_fee_cap(&self) -> &Option<u128> {
        &self.tx_fee_cap
    }

    /// Returns the minimum priority fee to enforce for acceptance into the pool
    pub const fn minimum_priority_fee(&self) -> &Option<u128> {
        &self.minimum_priority_fee
    }

    /// Returns the config to handle [`TransactionOrigin::Local`](TransactionOrigin) transactions..
    pub const fn local_transactions_config(&self) -> &LocalTransactionConfig {
        &self.local_transactions_config
    }

    /// Returns the maximum size in bytes a single transaction can have in order to be accepted into
    /// the pool.
    pub const fn max_tx_input_bytes(&self) -> usize {
        self.max_tx_input_bytes
    }

    /// Returns whether balance checks are disabled for this validator.
    pub const fn disable_balance_check(&self) -> bool {
        self.disable_balance_check
    }

    /// Sets an additional stateless validation check that is applied at the end of
    /// [`validate_stateless`](Self::validate_stateless).
    ///
    /// The check receives the transaction origin and a reference to the transaction, and
    /// should return `Ok(())` if the transaction is valid or
    /// `Err(InvalidPoolTransactionError)` to reject it.
    ///
    /// # Example
    ///
    /// ```ignore
    /// use reth_transaction_pool::{error::InvalidPoolTransactionError, TransactionOrigin};
    ///
    /// let mut validator = builder.build(blob_store);
    /// // Reject external transactions with input data exceeding 1KB
    /// validator.set_additional_stateless_validation(|origin, tx| {
    ///     if origin.is_external() && tx.input().len() > 1024 {
    ///         return Err(InvalidPoolTransactionError::OversizedData {
    ///             size: tx.input().len(),
    ///             limit: 1024,
    ///         });
    ///     }
    ///     Ok(())
    /// });
    /// ```
    pub fn set_additional_stateless_validation<F>(&mut self, f: F)
    where
        F: Fn(
                TransactionOrigin,
                &crate::BasePooledTransaction,
            ) -> Result<(), InvalidPoolTransactionError>
            + Send
            + Sync
            + 'static,
    {
        self.additional_stateless_validation = Some(Arc::new(f));
    }

    /// Sets the additional stateless validation check from an already shared
    /// [`StatelessValidationFn`].
    ///
    /// This is useful when the same hook is shared across multiple validators, avoiding an extra
    /// allocation compared to
    /// [`set_additional_stateless_validation`](Self::set_additional_stateless_validation).
    pub fn set_additional_stateless_validation_fn(
        &mut self,
        f: StatelessValidationFn<crate::BasePooledTransaction>,
    ) {
        self.additional_stateless_validation = Some(f);
    }

    /// Sets or clears the additional stateless validation check from an optional
    /// [`StatelessValidationFn`].
    ///
    /// Passing `None` removes any previously configured check.
    pub fn set_additional_stateless_validation_fn_opt(
        &mut self,
        f: Option<StatelessValidationFn<crate::BasePooledTransaction>>,
    ) {
        self.additional_stateless_validation = f;
    }

    /// Sets an additional stateful validation check that is applied at the end of
    /// [`validate_stateful`](Self::validate_stateful).
    ///
    /// The check receives the transaction origin, a reference to the transaction, and the
    /// account state reader, and should return `Ok(())` if the transaction is valid or
    /// `Err(InvalidPoolTransactionError)` to reject it.
    ///
    /// # Example
    ///
    /// ```ignore
    /// use reth_transaction_pool::{error::InvalidPoolTransactionError, TransactionOrigin};
    ///
    /// let mut validator = builder.build(blob_store);
    /// // Reject transactions from accounts with zero balance
    /// validator.set_additional_stateful_validation(|origin, tx, state| {
    ///     let account = state.basic_account(tx.sender_ref())?.unwrap_or_default();
    ///     if account.balance.is_zero() {
    ///         return Err(InvalidPoolTransactionError::Other(Box::new(
    ///             std::io::Error::new(std::io::ErrorKind::Other, "zero balance"),
    ///         )));
    ///     }
    ///     Ok(())
    /// });
    /// ```
    pub fn set_additional_stateful_validation<F>(&mut self, f: F)
    where
        F: Fn(
                TransactionOrigin,
                &crate::BasePooledTransaction,
                &dyn AccountInfoReader,
            ) -> Result<(), InvalidPoolTransactionError>
            + Send
            + Sync
            + 'static,
    {
        self.additional_stateful_validation = Some(Arc::new(f));
    }

    /// Sets the additional stateful validation check from an already shared
    /// [`StatefulValidationFn`].
    ///
    /// This is useful when the same hook is shared across multiple validators, avoiding an extra
    /// allocation compared to
    /// [`set_additional_stateful_validation`](Self::set_additional_stateful_validation).
    pub fn set_additional_stateful_validation_fn(
        &mut self,
        f: StatefulValidationFn<crate::BasePooledTransaction>,
    ) {
        self.additional_stateful_validation = Some(f);
    }

    /// Sets or clears the additional stateful validation check from an optional
    /// [`StatefulValidationFn`].
    ///
    /// Passing `None` removes any previously configured check.
    pub fn set_additional_stateful_validation_fn_opt(
        &mut self,
        f: Option<StatefulValidationFn<crate::BasePooledTransaction>>,
    ) {
        self.additional_stateful_validation = f;
    }
}

impl<Client> EthTransactionValidator<Client>
where
    Client: ChainSpecProvider + StateProviderFactory,
{
    /// Returns the current max gas limit
    pub fn block_gas_limit(&self) -> u64 {
        self.max_gas_limit()
    }

    /// Validates a single transaction.
    ///
    /// See also [`TransactionValidator::validate_transaction`]
    pub fn validate_one(
        &self,
        origin: TransactionOrigin,
        transaction: crate::BasePooledTransaction,
    ) -> TransactionValidationOutcome {
        let mut state: Option<StateProviderBox> = None;
        self.validate_one_with_provider(origin, transaction, &mut state, || self.client.latest())
    }

    /// Validates a single transaction with the provided state provider.
    ///
    /// This allows reusing the same provider across multiple transaction validations,
    /// which can improve performance when validating many transactions.
    ///
    /// If `state` is `None`, a new state provider will be created.
    pub fn validate_one_with_state(
        &self,
        origin: TransactionOrigin,
        transaction: crate::BasePooledTransaction,
        state: &mut Option<Box<dyn AccountInfoReader + Send>>,
    ) -> TransactionValidationOutcome {
        self.validate_one_with_provider(origin, transaction, state, || {
            self.client.latest().map(|state| Box::new(state) as Box<dyn AccountInfoReader + Send>)
        })
    }

    /// Validates a single transaction using an optional cached state provider.
    /// If no provider is passed, a new one will be created. This allows reusing
    /// the same provider across multiple txs.
    fn validate_one_with_provider<P, F>(
        &self,
        origin: TransactionOrigin,
        transaction: crate::BasePooledTransaction,
        maybe_state: &mut Option<P>,
        state_provider: F,
    ) -> TransactionValidationOutcome
    where
        P: AccountInfoReader,
        F: FnOnce() -> Result<P, ProviderError>,
    {
        match self.validate_stateless(origin, &transaction) {
            Ok(()) => {
                // stateless checks passed, pass transaction down stateful validation pipeline
                // If we don't have a state provider yet, fetch the latest state
                if maybe_state.is_none() {
                    match state_provider() {
                        Ok(new_state) => {
                            *maybe_state = Some(new_state);
                        }
                        Err(err) => {
                            return TransactionValidationOutcome::Error(
                                *transaction.hash(),
                                Box::new(err),
                            );
                        }
                    }
                }

                let state = maybe_state.as_ref().expect("provider is set");

                self.validate_stateful(origin, transaction, state)
            }
            Err(err) => TransactionValidationOutcome::Invalid(transaction, err),
        }
    }

    /// Validates a single transaction against the given state provider, performing both
    /// [stateless](Self::validate_stateless) and [stateful](Self::validate_stateful) checks.
    pub fn validate_one_with_state_provider(
        &self,
        origin: TransactionOrigin,
        transaction: crate::BasePooledTransaction,
        state: impl AccountInfoReader,
    ) -> TransactionValidationOutcome {
        if let Err(err) = self.validate_stateless(origin, &transaction) {
            return TransactionValidationOutcome::Invalid(transaction, err);
        }
        self.validate_stateful(origin, transaction, state)
    }

    /// Validates a single transaction without requiring any state access (stateless checks only).
    ///
    /// Checks tx type support, nonce bounds, size limits, gas limits, fee constraints, chain ID,
    /// intrinsic gas, and blob tx pre-checks.
    pub fn validate_stateless(
        &self,
        origin: TransactionOrigin,
        transaction: &crate::BasePooledTransaction,
    ) -> Result<(), InvalidPoolTransactionError> {
        // Checks for tx_type
        match transaction.ty() {
            // Accept only legacy transactions until EIP-2718/2930 activates
            EIP2930_TX_TYPE_ID if !self.eip2718 => {
                return Err(InvalidTransactionError::Eip2930Disabled.into());
            }
            // Reject dynamic fee transactions until EIP-1559 activates.
            EIP1559_TX_TYPE_ID if !self.eip1559 => {
                return Err(InvalidTransactionError::Eip1559Disabled.into());
            }
            // Reject blob transactions.
            EIP4844_TX_TYPE_ID if !self.eip4844 => {
                return Err(InvalidTransactionError::Eip4844Disabled.into());
            }
            // Reject EIP-7702 transactions.
            EIP7702_TX_TYPE_ID if !self.eip7702 => {
                return Err(InvalidTransactionError::Eip7702Disabled.into());
            }
            // Accept known transaction types when their respective fork is active
            LEGACY_TX_TYPE_ID | EIP2930_TX_TYPE_ID | EIP1559_TX_TYPE_ID | EIP4844_TX_TYPE_ID
            | EIP7702_TX_TYPE_ID => {}

            ty if !self.other_tx_types.bit(ty as usize) => {
                return Err(InvalidTransactionError::TxTypeNotSupported.into());
            }

            _ => {}
        };

        // Reject transactions with a nonce equal to U64::max according to EIP-2681
        let tx_nonce = transaction.nonce();
        if tx_nonce == u64::MAX {
            return Err(InvalidPoolTransactionError::Eip2681);
        }

        // Reject transactions over defined size to prevent DOS attacks
        if transaction.is_eip4844() {
            // Since blob transactions are pulled instead of pushed, and only the consensus data is
            // kept in memory while the sidecar is cached on disk, there is no critical limit that
            // should be enforced. Still, enforcing some cap on the dynamic transaction data. blob
            // txs also must be executable right away when they enter the pool.
            let tx_size = transaction.input().len().saturating_add(
                transaction
                    .access_list()
                    .map(|access_list| access_list.length())
                    .unwrap_or_default(),
            );
            if tx_size > self.max_tx_input_bytes {
                return Err(InvalidPoolTransactionError::OversizedData {
                    size: tx_size,
                    limit: self.max_tx_input_bytes,
                });
            }
        } else {
            // ensure the size of the non-blob transaction
            let tx_size = transaction.encoded_length();
            if tx_size > self.max_tx_input_bytes {
                return Err(InvalidPoolTransactionError::OversizedData {
                    size: tx_size,
                    limit: self.max_tx_input_bytes,
                });
            }
        }

        // Check whether the init code size has been exceeded.
        if self.fork_tracker.is_shanghai_activated() {
            let max_initcode_size =
                self.fork_tracker.max_initcode_size.load(std::sync::atomic::Ordering::Relaxed);
            transaction.ensure_max_init_code_size(max_initcode_size)?;
        }

        // Checks for gas limit
        let transaction_gas_limit = transaction.gas_limit();
        let block_gas_limit = self.max_gas_limit();
        if transaction_gas_limit > block_gas_limit {
            return Err(InvalidPoolTransactionError::ExceedsGasLimit(
                transaction_gas_limit,
                block_gas_limit,
            ));
        }

        // Check individual transaction gas limit if configured
        if let Some(max_tx_gas_limit) = self.max_tx_gas_limit
            && transaction_gas_limit > max_tx_gas_limit
        {
            return Err(InvalidPoolTransactionError::MaxTxGasLimitExceeded(
                transaction_gas_limit,
                max_tx_gas_limit,
            ));
        }

        // Ensure max_priority_fee_per_gas (if EIP1559) is less than max_fee_per_gas if any.
        if transaction.max_priority_fee_per_gas() > Some(transaction.max_fee_per_gas()) {
            return Err(InvalidTransactionError::TipAboveFeeCap.into());
        }

        // determine whether the transaction should be treated as local
        let is_local = self.local_transactions_config.is_local(origin, transaction.sender_ref());

        // Ensure max possible transaction fee doesn't exceed configured transaction fee cap.
        // Only for transactions locally submitted for acceptance into the pool.
        if is_local {
            match self.tx_fee_cap {
                Some(0) | None => {} // Skip if cap is 0 or None
                Some(tx_fee_cap_wei) => {
                    let max_tx_fee_wei = transaction.cost().saturating_sub(transaction.value());
                    if max_tx_fee_wei > tx_fee_cap_wei {
                        return Err(InvalidPoolTransactionError::ExceedsFeeCap {
                            max_tx_fee_wei: max_tx_fee_wei.saturating_to(),
                            tx_fee_cap_wei,
                        });
                    }
                }
            }
        }

        // Drop non-local transactions with a fee lower than the configured fee for acceptance into
        // the pool.
        if !is_local
            && transaction.is_dynamic_fee()
            && transaction.max_priority_fee_per_gas() < self.minimum_priority_fee
        {
            return Err(InvalidPoolTransactionError::PriorityFeeBelowMinimum {
                minimum_priority_fee: self
                    .minimum_priority_fee
                    .expect("minimum priority fee is expected inside if statement"),
            });
        }

        // Checks for chainid
        if let Some(chain_id) = transaction.chain_id()
            && chain_id != self.chain_id()
        {
            return Err(InvalidTransactionError::ChainIdMismatch.into());
        }

        if transaction.is_eip7702() {
            // Prague fork is required for 7702 txs
            if !self.fork_tracker.is_prague_activated() {
                return Err(InvalidTransactionError::TxTypeNotSupported.into());
            }

            if transaction.authorization_list().is_none_or(|l| l.is_empty()) {
                return Err(Eip7702PoolTransactionError::MissingEip7702AuthorizationList.into());
            }
        }

        ensure_intrinsic_gas(transaction, &self.fork_tracker)?;

        // light blob tx pre-checks
        if transaction.is_eip4844() {
            // Cancun fork is required for blob txs
            if !self.fork_tracker.is_cancun_activated() {
                return Err(InvalidTransactionError::TxTypeNotSupported.into());
            }

            let blob_count = transaction.blob_count().unwrap_or(0);
            if blob_count == 0 {
                // no blobs
                return Err(InvalidPoolTransactionError::Eip4844(
                    Eip4844PoolTransactionError::NoEip4844Blobs,
                ));
            }

            let max_blob_count = self.fork_tracker.max_blob_count();
            if blob_count > max_blob_count {
                return Err(InvalidPoolTransactionError::Eip4844(
                    Eip4844PoolTransactionError::TooManyEip4844Blobs {
                        have: blob_count,
                        permitted: max_blob_count,
                    },
                ));
            }
        }

        // Transaction gas limit validation (EIP-7825 for Osaka+)
        let tx_gas_limit_cap =
            self.fork_tracker.tx_gas_limit_cap.load(std::sync::atomic::Ordering::Relaxed);
        if tx_gas_limit_cap > 0 && transaction.gas_limit() > tx_gas_limit_cap {
            return Err(InvalidTransactionError::GasLimitTooHigh.into());
        }

        // Run additional stateless validation if configured
        if let Some(check) = &self.additional_stateless_validation {
            check(origin, transaction)?;
        }

        Ok(())
    }

    /// Validates a single transaction against the given state (stateful checks only).
    ///
    /// Checks sender account balance, nonce, bytecode, and validates blob sidecars. The
    /// transaction must have already passed [`validate_stateless`](Self::validate_stateless).
    pub fn validate_stateful<P>(
        &self,
        origin: TransactionOrigin,
        transaction: crate::BasePooledTransaction,
        state: P,
    ) -> TransactionValidationOutcome
    where
        P: AccountInfoReader,
    {
        // Use provider to get account info
        let account = match state.basic_account(transaction.sender_ref()) {
            Ok(account) => account.unwrap_or_default(),
            Err(err) => {
                return TransactionValidationOutcome::Error(*transaction.hash(), Box::new(err));
            }
        };

        // check for bytecode
        match self.validate_sender_bytecode(&transaction, &account, &state) {
            Err(outcome) => return outcome,
            Ok(Err(err)) => return TransactionValidationOutcome::Invalid(transaction, err),
            _ => {}
        };

        // Checks for nonce
        if transaction.requires_nonce_check()
            && let Err(err) = self.validate_sender_nonce(&transaction, &account)
        {
            return TransactionValidationOutcome::Invalid(transaction, err);
        }

        // checks for max cost not exceedng account_balance
        if let Err(err) = self.validate_sender_balance(&transaction, &account) {
            return TransactionValidationOutcome::Invalid(transaction, err);
        }

        // Run additional stateful validation if configured
        if let Some(check) = &self.additional_stateful_validation
            && let Err(err) = check(origin, &transaction, &state)
        {
            return TransactionValidationOutcome::Invalid(transaction, err);
        }

        let authorities = self.recover_authorities(&transaction);
        // Return the valid transaction
        TransactionValidationOutcome::Valid {
            balance: account.balance,
            state_nonce: account.nonce,
            bytecode_hash: account.bytecode_hash,
            transaction: ValidTransaction::new(transaction, None),
            // by this point assume all external transactions should be propagated
            propagate: match origin {
                TransactionOrigin::External => true,
                TransactionOrigin::Local => {
                    self.local_transactions_config.propagate_local_transactions
                }
                TransactionOrigin::Private => false,
            },
            authorities,
        }
    }

    /// Validates that the sender’s account has valid or no bytecode.
    pub fn validate_sender_bytecode(
        &self,
        transaction: &crate::BasePooledTransaction,
        sender: &Account,
        state: impl BytecodeReader,
    ) -> Result<Result<(), InvalidPoolTransactionError>, TransactionValidationOutcome> {
        // Unless Prague is active, the signer account shouldn't have bytecode.
        //
        // If Prague is active, only EIP-7702 bytecode is allowed for the sender.
        //
        // Any other case means that the account is not an EOA, and should not be able to send
        // transactions.
        if let Some(code_hash) = &sender.bytecode_hash
            && *code_hash != KECCAK_EMPTY
        {
            let is_eip7702 = if self.fork_tracker.is_prague_activated() {
                match state.bytecode_by_hash(code_hash) {
                    Ok(bytecode) => bytecode.unwrap_or_default().is_eip7702(),
                    Err(err) => {
                        return Err(TransactionValidationOutcome::Error(
                            *transaction.hash(),
                            Box::new(err),
                        ));
                    }
                }
            } else {
                false
            };

            if !is_eip7702 {
                return Ok(Err(InvalidTransactionError::SignerAccountHasBytecode.into()));
            }
        }
        Ok(Ok(()))
    }

    /// Checks if the transaction nonce is valid.
    pub fn validate_sender_nonce(
        &self,
        transaction: &crate::BasePooledTransaction,
        sender: &Account,
    ) -> Result<(), InvalidPoolTransactionError> {
        let tx_nonce = transaction.nonce();

        if tx_nonce < sender.nonce {
            return Err(InvalidTransactionError::NonceNotConsistent {
                tx: tx_nonce,
                state: sender.nonce,
            }
            .into());
        }
        Ok(())
    }

    /// Ensures the sender has sufficient account balance.
    pub fn validate_sender_balance(
        &self,
        transaction: &crate::BasePooledTransaction,
        sender: &Account,
    ) -> Result<(), InvalidPoolTransactionError> {
        let cost = transaction.cost();

        if !self.disable_balance_check && cost > &sender.balance {
            let expected = *cost;
            return Err(InvalidTransactionError::InsufficientFunds(
                GotExpected { got: sender.balance, expected }.into(),
            )
            .into());
        }
        Ok(())
    }

    /// Returns the recovered authorities for the given transaction
    fn recover_authorities(
        &self,
        transaction: &crate::BasePooledTransaction,
    ) -> std::option::Option<Vec<Address>> {
        transaction
            .authorization_list()
            .map(|auths| auths.iter().flat_map(|auth| auth.recover_authority()).collect::<Vec<_>>())
    }

    /// Validates all given transactions.
    fn validate_batch(
        &self,
        transactions: impl IntoIterator<Item = (TransactionOrigin, crate::BasePooledTransaction)>,
    ) -> Vec<TransactionValidationOutcome> {
        let mut provider: Option<StateProviderBox> = None;
        transactions
            .into_iter()
            .map(|(origin, tx)| {
                self.validate_one_with_provider(origin, tx, &mut provider, || self.client.latest())
            })
            .collect()
    }

    /// Validates all given transactions with origin.
    fn validate_batch_with_origin(
        &self,
        origin: TransactionOrigin,
        transactions: impl IntoIterator<Item = crate::BasePooledTransaction> + Send,
    ) -> Vec<TransactionValidationOutcome> {
        let mut provider: Option<StateProviderBox> = None;
        transactions
            .into_iter()
            .map(|tx| {
                self.validate_one_with_provider(origin, tx, &mut provider, || self.client.latest())
            })
            .collect()
    }

    fn on_new_head_block(&self, new_tip_block: &base_common_types_chain::Header) {
        // update all forks
        if self.chain_spec().is_shanghai_active_at_timestamp(new_tip_block.timestamp()) {
            self.fork_tracker.shanghai.store(true, std::sync::atomic::Ordering::Relaxed);
        }

        if self.chain_spec().is_cancun_active_at_timestamp(new_tip_block.timestamp()) {
            self.fork_tracker.cancun.store(true, std::sync::atomic::Ordering::Relaxed);
        }

        if self.chain_spec().is_prague_active_at_timestamp(new_tip_block.timestamp()) {
            self.fork_tracker.prague.store(true, std::sync::atomic::Ordering::Relaxed);
        }

        if self.chain_spec().is_osaka_active_at_timestamp(new_tip_block.timestamp()) {
            self.fork_tracker.osaka.store(true, std::sync::atomic::Ordering::Relaxed);
        }

        if self.chain_spec().is_amsterdam_active_at_timestamp(new_tip_block.timestamp()) {
            self.fork_tracker.amsterdam.store(true, std::sync::atomic::Ordering::Relaxed);
        }

        self.fork_tracker
            .tip_timestamp
            .store(new_tip_block.timestamp(), std::sync::atomic::Ordering::Relaxed);

        if let Some(blob_params) =
            self.chain_spec().blob_params_at_timestamp(new_tip_block.timestamp())
        {
            self.fork_tracker
                .max_blob_count
                .store(blob_params.max_blobs_per_tx, std::sync::atomic::Ordering::Relaxed);
        }

        self.block_gas_limit.store(new_tip_block.gas_limit(), std::sync::atomic::Ordering::Relaxed);

        // Get EVM limits from evm_config.evm_env()
        let evm_env = self
            .evm_config
            .evm_env(new_tip_block)
            .expect("evm_env should not fail for executed block");

        self.fork_tracker
            .max_initcode_size
            .store(evm_env.cfg_env.max_initcode_size(), std::sync::atomic::Ordering::Relaxed);
        // EIP-8037: When state gas is enabled, `tx.gas` can exceed the per-tx gas limit cap
        // because the cap only applies to regular gas (state gas uses a reservoir).
        // Store 0 to disable the txpool-level check.
        let tx_gas_limit_cap = if evm_env.cfg_env.is_amsterdam_eip8037_enabled() {
            0
        } else {
            evm_env.cfg_env.tx_gas_limit_cap()
        };
        self.fork_tracker
            .tx_gas_limit_cap
            .store(tx_gas_limit_cap, std::sync::atomic::Ordering::Relaxed);
    }

    fn max_gas_limit(&self) -> u64 {
        self.block_gas_limit.load(std::sync::atomic::Ordering::Relaxed)
    }
}

impl<Client> TransactionValidator for EthTransactionValidator<Client>
where
    Client: ChainSpecProvider + StateProviderFactory,
{
    type Block = BaseBlock;

    async fn validate_transaction(
        &self,
        origin: TransactionOrigin,
        transaction: crate::BasePooledTransaction,
    ) -> TransactionValidationOutcome {
        self.validate_one(origin, transaction)
    }

    async fn validate_transactions(
        &self,
        transactions: impl IntoIterator<
            Item = (TransactionOrigin, crate::BasePooledTransaction),
            IntoIter: Send,
        > + Send,
    ) -> Vec<TransactionValidationOutcome> {
        self.validate_batch(transactions)
    }

    async fn validate_transactions_with_origin(
        &self,
        origin: TransactionOrigin,
        transactions: impl IntoIterator<Item = crate::BasePooledTransaction, IntoIter: Send> + Send,
    ) -> Vec<TransactionValidationOutcome> {
        self.validate_batch_with_origin(origin, transactions)
    }

    fn on_new_head_block(&self, new_tip_block: &SealedBlock) {
        Self::on_new_head_block(self, new_tip_block.header())
    }
}

/// A builder for [`EthTransactionValidator`] and [`TransactionValidationTaskExecutor`]
#[derive(Debug)]
pub struct EthTransactionValidatorBuilder<Client> {
    client: Client,
    /// The chain ID transactions must use.
    chain_id: u64,
    /// The EVM configuration to use for validation.
    evm_config: BaseEvmConfig,
    /// Fork indicator whether we are in the Shanghai stage.
    shanghai: bool,
    /// Fork indicator whether we are in the Cancun hardfork.
    cancun: bool,
    /// Fork indicator whether we are in the Prague hardfork.
    prague: bool,
    /// Fork indicator whether we are in the Osaka hardfork.
    osaka: bool,
    /// Fork indicator whether we are in the Amsterdam hardfork.
    amsterdam: bool,
    /// Timestamp of the tip block.
    tip_timestamp: u64,
    /// Max blob count at the block's timestamp.
    max_blob_count: u64,
    /// Whether using EIP-2718 type transactions is allowed
    eip2718: bool,
    /// Whether using EIP-1559 type transactions is allowed
    eip1559: bool,
    /// Whether using EIP-4844 type transactions is allowed
    eip4844: bool,
    /// Whether using EIP-7702 type transactions is allowed
    eip7702: bool,
    /// The current max gas limit
    block_gas_limit: AtomicU64,
    /// The current tx fee cap limit in wei locally submitted into the pool.
    tx_fee_cap: Option<u128>,
    /// Minimum priority fee to enforce for acceptance into the pool.
    minimum_priority_fee: Option<u128>,
    /// Determines how many additional tasks to spawn
    ///
    /// Default is 1
    additional_tasks: usize,

    /// How to handle [`TransactionOrigin::Local`](TransactionOrigin) transactions.
    local_transactions_config: LocalTransactionConfig,
    /// Max size in bytes of a single transaction allowed
    max_tx_input_bytes: usize,
    /// Maximum gas limit for individual transactions
    max_tx_gas_limit: Option<u64>,
    /// Disable balance checks during transaction validation
    disable_balance_check: bool,
    /// Bitmap of custom transaction types that are allowed.
    other_tx_types: U256,
    /// Cached max initcode size from EVM config
    max_initcode_size: usize,
    /// Cached transaction gas limit cap from EVM config (0 = no cap)
    tx_gas_limit_cap: u64,
}

impl<Client> EthTransactionValidatorBuilder<Client> {
    /// Creates a new builder for the given client and EVM config
    ///
    /// By default this assumes the network is on the `Prague` hardfork and the following
    /// transactions are allowed:
    ///  - Legacy
    ///  - EIP-2718
    ///  - EIP-1559
    ///  - EIP-4844
    ///  - EIP-7702
    pub fn new(client: Client, evm_config: BaseEvmConfig) -> Self
    where
        Client: ChainSpecProvider + BlockReaderIdExt,
    {
        let chain_spec = client.chain_spec();
        let tip = client
            .header_by_id(BlockId::latest())
            .expect("failed to fetch latest header")
            .expect("latest header is not found");
        let evm_env =
            evm_config.evm_env(&tip).expect("evm_env should not fail for existing blocks");

        Self {
            block_gas_limit: ETHEREUM_BLOCK_GAS_LIMIT_30M.into(),
            client,
            chain_id: chain_spec.chain().id(),
            evm_config,
            minimum_priority_fee: None,
            additional_tasks: 1,

            local_transactions_config: Default::default(),
            max_tx_input_bytes: DEFAULT_MAX_TX_INPUT_BYTES,
            tx_fee_cap: Some(1e18 as u128),
            max_tx_gas_limit: None,
            // by default all transaction types are allowed
            eip2718: true,
            eip1559: true,
            eip4844: true,
            eip7702: true,

            shanghai: chain_spec.is_shanghai_active_at_timestamp(tip.timestamp()),
            cancun: chain_spec.is_cancun_active_at_timestamp(tip.timestamp()),
            prague: chain_spec.is_prague_active_at_timestamp(tip.timestamp()),
            osaka: chain_spec.is_osaka_active_at_timestamp(tip.timestamp()),
            amsterdam: chain_spec.is_amsterdam_active_at_timestamp(tip.timestamp()),

            tip_timestamp: tip.timestamp(),

            max_blob_count: chain_spec
                .blob_params_at_timestamp(tip.timestamp())
                .unwrap_or_else(BlobParams::prague)
                .max_blobs_per_tx,

            // balance checks are enabled by default
            disable_balance_check: false,

            // no custom transaction types by default
            other_tx_types: U256::ZERO,

            // EIP-8037: When state gas is enabled, tx.gas can exceed the per-tx cap
            tx_gas_limit_cap: if evm_env.cfg_env.is_amsterdam_eip8037_enabled() {
                0
            } else {
                evm_env.cfg_env.tx_gas_limit_cap()
            },
            max_initcode_size: evm_env.cfg_env.max_initcode_size(),
            // EIP-7594 sidecars are accepted by default (standard Ethereum behavior)
        }
    }

    /// Disables the Cancun fork.
    pub const fn no_cancun(self) -> Self {
        self.set_cancun(false)
    }

    /// Whether to allow exemptions for local transaction exemptions.
    pub fn with_local_transactions_config(
        mut self,
        local_transactions_config: LocalTransactionConfig,
    ) -> Self {
        self.local_transactions_config = local_transactions_config;
        self
    }

    /// Set the Cancun fork.
    pub const fn set_cancun(mut self, cancun: bool) -> Self {
        self.cancun = cancun;
        self
    }

    /// Disables the Shanghai fork.
    pub const fn no_shanghai(self) -> Self {
        self.set_shanghai(false)
    }

    /// Set the Shanghai fork.
    pub const fn set_shanghai(mut self, shanghai: bool) -> Self {
        self.shanghai = shanghai;
        self
    }

    /// Disables the Prague fork.
    pub const fn no_prague(self) -> Self {
        self.set_prague(false)
    }

    /// Set the Prague fork.
    pub const fn set_prague(mut self, prague: bool) -> Self {
        self.prague = prague;
        self
    }

    /// Disables the Osaka fork.
    pub const fn no_osaka(self) -> Self {
        self.set_osaka(false)
    }

    /// Set the Osaka fork.
    pub const fn set_osaka(mut self, osaka: bool) -> Self {
        self.osaka = osaka;
        self
    }

    /// Disables the Amsterdam fork.
    pub const fn no_amsterdam(self) -> Self {
        self.set_amsterdam(false)
    }

    /// Set the Amsterdam fork.
    pub const fn set_amsterdam(mut self, amsterdam: bool) -> Self {
        self.amsterdam = amsterdam;
        self
    }

    /// Disables the support for EIP-2718 transactions.
    pub const fn no_eip2718(self) -> Self {
        self.set_eip2718(false)
    }

    /// Set the support for EIP-2718 transactions.
    pub const fn set_eip2718(mut self, eip2718: bool) -> Self {
        self.eip2718 = eip2718;
        self
    }

    /// Disables the support for EIP-1559 transactions.
    pub const fn no_eip1559(self) -> Self {
        self.set_eip1559(false)
    }

    /// Set the support for EIP-1559 transactions.
    pub const fn set_eip1559(mut self, eip1559: bool) -> Self {
        self.eip1559 = eip1559;
        self
    }

    /// Disables the support for EIP-4844 transactions.
    pub const fn no_eip4844(self) -> Self {
        self.set_eip4844(false)
    }

    /// Set the support for EIP-4844 transactions.
    pub const fn set_eip4844(mut self, eip4844: bool) -> Self {
        self.eip4844 = eip4844;
        self
    }

    /// Disables the support for EIP-7702 transactions.
    pub const fn no_eip7702(self) -> Self {
        self.set_eip7702(false)
    }

    /// Set the support for EIP-7702 transactions.
    pub const fn set_eip7702(mut self, eip7702: bool) -> Self {
        self.eip7702 = eip7702;
        self
    }

    /// Sets a minimum priority fee that's enforced for acceptance into the pool.
    pub const fn with_minimum_priority_fee(mut self, minimum_priority_fee: Option<u128>) -> Self {
        self.minimum_priority_fee = minimum_priority_fee;
        self
    }

    /// Sets the number of additional tasks to spawn.
    pub const fn with_additional_tasks(mut self, additional_tasks: usize) -> Self {
        self.additional_tasks = additional_tasks;
        self
    }

    /// Sets a max size in bytes of a single transaction allowed into the pool
    pub const fn with_max_tx_input_bytes(mut self, max_tx_input_bytes: usize) -> Self {
        self.max_tx_input_bytes = max_tx_input_bytes;
        self
    }

    /// Sets the block gas limit
    ///
    /// Transactions with a gas limit greater than this will be rejected.
    pub fn set_block_gas_limit(self, block_gas_limit: u64) -> Self {
        self.block_gas_limit.store(block_gas_limit, std::sync::atomic::Ordering::Relaxed);
        self
    }

    /// Sets the block gas limit
    ///
    /// Transactions with a gas limit greater than this will be rejected.
    pub const fn set_tx_fee_cap(mut self, tx_fee_cap: u128) -> Self {
        self.tx_fee_cap = Some(tx_fee_cap);
        self
    }

    /// Sets the maximum gas limit for individual transactions
    pub const fn with_max_tx_gas_limit(mut self, max_tx_gas_limit: Option<u64>) -> Self {
        self.max_tx_gas_limit = max_tx_gas_limit;
        self
    }

    /// Disables balance checks during transaction validation
    pub const fn disable_balance_check(mut self) -> Self {
        self.disable_balance_check = true;
        self
    }

    /// Adds a custom transaction type to the validator.
    pub const fn with_custom_tx_type(mut self, tx_type: u8) -> Self {
        self.other_tx_types.set_bit(tx_type as usize, true);
        self
    }

    /// Builds a the [`EthTransactionValidator`] without spawning validator tasks.
    pub fn build(self) -> EthTransactionValidator<Client> {
        let Self {
            client,
            chain_id,
            evm_config,
            shanghai,
            cancun,
            prague,
            osaka,
            amsterdam,
            tip_timestamp,
            eip2718,
            eip1559,
            eip4844,
            eip7702,
            block_gas_limit,
            tx_fee_cap,
            minimum_priority_fee,

            local_transactions_config,
            max_tx_input_bytes,
            max_tx_gas_limit,
            disable_balance_check,
            max_blob_count,
            additional_tasks: _,
            other_tx_types,
            max_initcode_size,
            tx_gas_limit_cap,
        } = self;

        let fork_tracker = ForkTracker {
            shanghai: AtomicBool::new(shanghai),
            cancun: AtomicBool::new(cancun),
            prague: AtomicBool::new(prague),
            osaka: AtomicBool::new(osaka),
            amsterdam: AtomicBool::new(amsterdam),
            tip_timestamp: AtomicU64::new(tip_timestamp),
            max_blob_count: AtomicU64::new(max_blob_count),
            max_initcode_size: AtomicUsize::new(max_initcode_size),
            tx_gas_limit_cap: AtomicU64::new(tx_gas_limit_cap),
        };

        EthTransactionValidator {
            client,
            chain_id,
            eip2718,
            eip1559,
            fork_tracker,
            eip4844,
            eip7702,
            block_gas_limit,
            tx_fee_cap,
            minimum_priority_fee,

            local_transactions_config,
            max_tx_input_bytes,
            max_tx_gas_limit,
            disable_balance_check,
            evm_config,

            other_tx_types,

            additional_stateless_validation: None,
            additional_stateful_validation: None,
        }
    }

    /// Builds a [`EthTransactionValidator`] and spawns validation tasks via the
    /// [`TransactionValidationTaskExecutor`]
    ///
    /// The validator will spawn `additional_tasks` additional tasks for validation.
    ///
    /// By default this will spawn 1 additional task.
    pub fn build_with_tasks(
        self,
        tasks: Runtime,
    ) -> TransactionValidationTaskExecutor<EthTransactionValidator<Client>> {
        let additional_tasks = self.additional_tasks;
        let validator = self.build();
        TransactionValidationTaskExecutor::spawn(validator, &tasks, additional_tasks)
    }
}

/// Keeps track of whether certain forks are activated
#[derive(Debug)]
pub struct ForkTracker {
    /// Tracks if shanghai is activated at the block's timestamp.
    pub shanghai: AtomicBool,
    /// Tracks if cancun is activated at the block's timestamp.
    pub cancun: AtomicBool,
    /// Tracks if prague is activated at the block's timestamp.
    pub prague: AtomicBool,
    /// Tracks if osaka is activated at the block's timestamp.
    pub osaka: AtomicBool,
    /// Tracks if amsterdam is activated at the block's timestamp.
    pub amsterdam: AtomicBool,
    /// Tracks max blob count per transaction at the block's timestamp.
    pub max_blob_count: AtomicU64,
    /// Tracks the timestamp of the tip block.
    pub tip_timestamp: AtomicU64,
    /// Cached max initcode size from EVM config
    pub max_initcode_size: AtomicUsize,
    /// Cached transaction gas limit cap from EVM config (0 = no cap)
    pub tx_gas_limit_cap: AtomicU64,
}

impl ForkTracker {
    /// Returns `true` if Shanghai fork is activated.
    pub fn is_shanghai_activated(&self) -> bool {
        self.shanghai.load(std::sync::atomic::Ordering::Relaxed)
    }

    /// Returns `true` if Cancun fork is activated.
    pub fn is_cancun_activated(&self) -> bool {
        self.cancun.load(std::sync::atomic::Ordering::Relaxed)
    }

    /// Returns `true` if Prague fork is activated.
    pub fn is_prague_activated(&self) -> bool {
        self.prague.load(std::sync::atomic::Ordering::Relaxed)
    }

    /// Returns `true` if Osaka fork is activated.
    pub fn is_osaka_activated(&self) -> bool {
        self.osaka.load(std::sync::atomic::Ordering::Relaxed)
    }

    /// Returns `true` if Amsterdam fork is activated.
    pub fn is_amsterdam_activated(&self) -> bool {
        self.amsterdam.load(std::sync::atomic::Ordering::Relaxed)
    }

    /// Returns the timestamp of the tip block.
    pub fn tip_timestamp(&self) -> u64 {
        self.tip_timestamp.load(std::sync::atomic::Ordering::Relaxed)
    }

    /// Returns the max allowed blob count per transaction.
    pub fn max_blob_count(&self) -> u64 {
        self.max_blob_count.load(std::sync::atomic::Ordering::Relaxed)
    }
}

/// Ensures that gas limit of the transaction exceeds the intrinsic gas of the transaction.
///
/// Caution: This only checks past the Merge hardfork.
pub fn ensure_intrinsic_gas(
    transaction: &crate::BasePooledTransaction,
    fork_tracker: &ForkTracker,
) -> Result<(), InvalidPoolTransactionError> {
    use base_execution_evm_runtime::primitives::hardfork::SpecId;
    let spec_id = if fork_tracker.is_amsterdam_activated() {
        SpecId::AMSTERDAM
    } else if fork_tracker.is_prague_activated() {
        SpecId::PRAGUE
    } else if fork_tracker.is_shanghai_activated() {
        SpecId::SHANGHAI
    } else {
        SpecId::MERGE
    };

    // EIP-2780 replaces the flat intrinsic base cost with a decomposed one that depends on
    // `tx.to` and `tx.value`.
    let eip2780 = fork_tracker.is_amsterdam_activated().then(|| {
        base_evm_context::Eip2780TxInfo {
            value: transaction.value(),
            // Self-transfer: a `Call` whose recipient is the sender itself.
            is_self_transfer: transaction.kind().to() == Some(&transaction.sender()),
        }
    });

    let gas = base_execution_evm_runtime::interpreter::gas::calculate_initial_tx_gas(
        spec_id,
        transaction.input(),
        transaction.is_create(),
        transaction.access_list().map(|l| l.len()).unwrap_or_default() as u64,
        transaction
            .access_list()
            .map(|l| l.iter().map(|i| i.storage_keys.len()).sum::<usize>())
            .unwrap_or_default() as u64,
        transaction.authorization_list().map(|l| l.len()).unwrap_or_default() as u64,
        eip2780,
    );

    let gas_limit = transaction.gas_limit();
    if gas_limit < gas.initial_total_gas() || gas_limit < gas.floor_gas {
        Err(InvalidPoolTransactionError::IntrinsicGasTooLow)
    } else {
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use alloy_eips::{
        eip2718::{Decodable2718, Encodable2718},
        eip2930::{AccessList, AccessListItem},
    };
    use alloy_primitives::{Address, B256, Bytes, U256, hex};
    use base_common_types_chain::Transaction;
    use base_execution_evm_runtime::primitives::eip3860::MAX_INITCODE_SIZE;
    use reth_primitives_traits::SignedTransaction;
    use reth_provider::test_utils::{ExtendedAccount, MockEthProvider};

    use super::*;
    use crate::{
        BaseOrdering, BasePooledTransaction, Pool, TransactionPool, blobstore::InMemoryBlobStore,
        error::PoolErrorKind, test_utils::TransactionBuilder,
    };

    fn test_evm_config() -> BaseEvmConfig {
        let mut chain_spec = (**BaseEvmConfig::default().chain_spec()).clone();
        chain_spec.config.chain_id = 1;
        BaseEvmConfig::new(Arc::new(chain_spec))
    }

    fn get_transaction() -> BasePooledTransaction {
        let raw = "0x02f914950181ad84b2d05e0085117553845b830f7df88080b9143a6040608081523462000414576200133a803803806200001e8162000419565b9283398101608082820312620004145781516001600160401b03908181116200041457826200004f9185016200043f565b92602092838201519083821162000414576200006d9183016200043f565b8186015190946001600160a01b03821692909183900362000414576060015190805193808511620003145760038054956001938488811c9816801562000409575b89891014620003f3578190601f988981116200039d575b50899089831160011462000336576000926200032a575b505060001982841b1c191690841b1781555b8751918211620003145760049788548481811c9116801562000309575b89821014620002f457878111620002a9575b5087908784116001146200023e5793839491849260009562000232575b50501b92600019911b1c19161785555b6005556007805460ff60a01b19169055600880546001600160a01b0319169190911790553015620001f3575060025469d3c21bcecceda100000092838201809211620001de57506000917fddf252ad1be2c89b69c2b068fc378daa952ba7f163c4a11628f55a4df523b3ef9160025530835282815284832084815401905584519384523093a351610e889081620004b28239f35b601190634e487b7160e01b6000525260246000fd5b90606493519262461bcd60e51b845283015260248201527f45524332303a206d696e7420746f20746865207a65726f2061646472657373006044820152fd5b0151935038806200013a565b9190601f198416928a600052848a6000209460005b8c8983831062000291575050501062000276575b50505050811b0185556200014a565b01519060f884600019921b161c191690553880808062000267565b86860151895590970196948501948893500162000253565b89600052886000208880860160051c8201928b8710620002ea575b0160051c019085905b828110620002dd5750506200011d565b60008155018590620002cd565b92508192620002c4565b60228a634e487b7160e01b6000525260246000fd5b90607f16906200010b565b634e487b7160e01b600052604160045260246000fd5b015190503880620000dc565b90869350601f19831691856000528b6000209260005b8d8282106200038657505084116200036d575b505050811b018155620000ee565b015160001983861b60f8161c191690553880806200035f565b8385015186558a979095019493840193016200034c565b90915083600052896000208980850160051c8201928c8610620003e9575b918891869594930160051c01915b828110620003d9575050620000c5565b60008155859450889101620003c9565b92508192620003bb565b634e487b7160e01b600052602260045260246000fd5b97607f1697620000ae565b600080fd5b6040519190601f01601f191682016001600160401b038111838210176200031457604052565b919080601f84011215620004145782516001600160401b038111620003145760209062000475601f8201601f1916830162000419565b92818452828287010111620004145760005b8181106200049d57508260009394955001015290565b85810183015184820184015282016200048756fe608060408181526004918236101561001657600080fd5b600092833560e01c91826306fdde0314610a1c57508163095ea7b3146109f257816318160ddd146109d35781631b4c84d2146109ac57816323b872dd14610833578163313ce5671461081757816339509351146107c357816370a082311461078c578163715018a6146107685781638124f7ac146107495781638da5cb5b1461072057816395d89b411461061d578163a457c2d714610575578163a9059cbb146104e4578163c9567bf914610120575063dd62ed3e146100d557600080fd5b3461011c578060031936011261011c57806020926100f1610b5a565b6100f9610b75565b6001600160a01b0391821683526001865283832091168252845220549051908152f35b5080fd5b905082600319360112610338576008546001600160a01b039190821633036104975760079283549160ff8360a01c1661045557737a250d5630b4cf539739df2c5dacb4c659f2488d92836bffffffffffffffffffffffff60a01b8092161786553087526020938785528388205430156104065730895260018652848920828a52865280858a205584519081527f8c5be1e5ebec7d5bd14f71427d1e84f3dd0314c0f7b2291e5b200ac8c7c3b925863092a38554835163c45a015560e01b815290861685828581845afa9182156103dd57849187918b946103e7575b5086516315ab88c960e31b815292839182905afa9081156103dd576044879289928c916103c0575b508b83895196879586946364e329cb60e11b8652308c870152166024850152165af19081156103b6579086918991610389575b50169060065416176006558385541660604730895288865260c4858a20548860085416928751958694859363f305d71960e01b8552308a86015260248501528d60448501528d606485015260848401524260a48401525af1801561037f579084929161034c575b50604485600654169587541691888551978894859363095ea7b360e01b855284015260001960248401525af1908115610343575061030c575b5050805460ff60a01b1916600160a01b17905580f35b81813d831161033c575b6103208183610b8b565b8101031261033857518015150361011c5738806102f6565b8280fd5b503d610316565b513d86823e3d90fd5b6060809293503d8111610378575b6103648183610b8b565b81010312610374578290386102bd565b8580fd5b503d61035a565b83513d89823e3d90fd5b6103a99150863d88116103af575b6103a18183610b8b565b810190610e33565b38610256565b503d610397565b84513d8a823e3d90fd5b6103d79150843d86116103af576103a18183610b8b565b38610223565b85513d8b823e3d90fd5b6103ff919450823d84116103af576103a18183610b8b565b92386101fb565b845162461bcd60e51b81528085018790526024808201527f45524332303a20617070726f76652066726f6d20746865207a65726f206164646044820152637265737360e01b6064820152608490fd5b6020606492519162461bcd60e51b8352820152601760248201527f74726164696e6720697320616c7265616479206f70656e0000000000000000006044820152fd5b608490602084519162461bcd60e51b8352820152602160248201527f4f6e6c79206f776e65722063616e2063616c6c20746869732066756e6374696f6044820152603760f91b6064820152fd5b9050346103385781600319360112610338576104fe610b5a565b9060243593303303610520575b602084610519878633610bc3565b5160018152f35b600594919454808302908382041483151715610562576127109004820391821161054f5750925080602061050b565b634e487b7160e01b815260118552602490fd5b634e487b7160e01b825260118652602482fd5b9050823461061a578260031936011261061a57610590610b5a565b918360243592338152600160205281812060018060a01b03861682526020522054908282106105c9576020856105198585038733610d31565b608490602086519162461bcd60e51b8352820152602560248201527f45524332303a2064656372656173656420616c6c6f77616e63652062656c6f77604482015264207a65726f60d81b6064820152fd5b80fd5b83833461011c578160031936011261011c57805191809380549160019083821c92828516948515610716575b6020958686108114610703578589529081156106df5750600114610687575b6106838787610679828c0383610b8b565b5191829182610b11565b0390f35b81529295507f8a35acfbc15ff81a39ae7d344fd709f28e8600b4aa8c65c6b64bfe7fe36bd19b5b8284106106cc57505050826106839461067992820101948680610668565b80548685018801529286019281016106ae565b60ff19168887015250505050151560051b8301019250610679826106838680610668565b634e487b7160e01b845260228352602484fd5b93607f1693610649565b50503461011c578160031936011261011c5760085490516001600160a01b039091168152602090f35b50503461011c578160031936011261011c576020906005549051908152f35b833461061a578060031936011261061a57600880546001600160a01b031916905580f35b50503461011c57602036600319011261011c5760209181906001600160a01b036107b4610b5a565b16815280845220549051908152f35b82843461061a578160031936011261061a576107dd610b5a565b338252600160209081528383206001600160a01b038316845290528282205460243581019290831061054f57602084610519858533610d31565b50503461011c578160031936011261011c576020905160128152f35b83833461011c57606036600319011261011c5761084e610b5a565b610856610b75565b6044359160018060a01b0381169485815260209560018752858220338352875285822054976000198903610893575b505050906105199291610bc3565b85891061096957811561091a5733156108cc5750948481979861051997845260018a528284203385528a52039120558594938780610885565b865162461bcd60e51b8152908101889052602260248201527f45524332303a20617070726f766520746f20746865207a65726f206164647265604482015261737360f01b6064820152608490fd5b865162461bcd60e51b81529081018890526024808201527f45524332303a20617070726f76652066726f6d20746865207a65726f206164646044820152637265737360e01b6064820152608490fd5b865162461bcd60e51b8152908101889052601d60248201527f45524332303a20696e73756666696369656e7420616c6c6f77616e63650000006044820152606490fd5b50503461011c578160031936011261011c5760209060ff60075460a01c1690519015158152f35b50503461011c578160031936011261011c576020906002549051908152f35b50503461011c578060031936011261011c57602090610519610a12610b5a565b6024359033610d31565b92915034610b0d5783600319360112610b0d57600354600181811c9186908281168015610b03575b6020958686108214610af05750848852908115610ace5750600114610a75575b6106838686610679828b0383610b8b565b929550600383527fc2575a0e9e593c00f959f8c92f12db2869c3395a3b0502d05e2516446f71f85b5b828410610abb575050508261068394610679928201019438610a64565b8054868501880152928601928101610a9e565b60ff191687860152505050151560051b83010192506106798261068338610a64565b634e487b7160e01b845260229052602483fd5b93607f1693610a44565b8380fd5b6020808252825181830181905290939260005b828110610b4657505060409293506000838284010152601f8019910116010190565b818101860151848201604001528501610b24565b600435906001600160a01b0382168203610b7057565b600080fd5b602435906001600160a01b0382168203610b7057565b90601f8019910116810190811067ffffffffffffffff821117610bad57604052565b634e487b7160e01b600052604160045260246000fd5b6001600160a01b03908116918215610cde5716918215610c8d57600082815280602052604081205491808310610c3957604082827fddf252ad1be2c89b69c2b068fc378daa952ba7f163c4a11628f55a4df523b3ef958760209652828652038282205586815220818154019055604051908152a3565b60405162461bcd60e51b815260206004820152602660248201527f45524332303a207472616e7366657220616d6f756e7420657863656564732062604482015265616c616e636560d01b6064820152608490fd5b60405162461bcd60e51b815260206004820152602360248201527f45524332303a207472616e7366657220746f20746865207a65726f206164647260448201526265737360e81b6064820152608490fd5b60405162461bcd60e51b815260206004820152602560248201527f45524332303a207472616e736665722066726f6d20746865207a65726f206164604482015264647265737360d81b6064820152608490fd5b6001600160a01b03908116918215610de25716918215610d925760207f8c5be1e5ebec7d5bd14f71427d1e84f3dd0314c0f7b2291e5b200ac8c7c3b925918360005260018252604060002085600052825280604060002055604051908152a3565b60405162461bcd60e51b815260206004820152602260248201527f45524332303a20617070726f766520746f20746865207a65726f206164647265604482015261737360f01b6064820152608490fd5b60405162461bcd60e51b8152602060048201526024808201527f45524332303a20617070726f76652066726f6d20746865207a65726f206164646044820152637265737360e01b6064820152608490fd5b90816020910312610b7057516001600160a01b0381168103610b70579056fea2646970667358221220285c200b3978b10818ff576bb83f2dc4a2a7c98dfb6a36ea01170de792aa652764736f6c63430008140033000000000000000000000000000000000000000000000000000000000000008000000000000000000000000000000000000000000000000000000000000000c0000000000000000000000000d3fd4f95820a9aa848ce716d6c200eaefb9a2e4900000000000000000000000000000000000000000000000000000000000000640000000000000000000000000000000000000000000000000000000000000003543131000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000035431310000000000000000000000000000000000000000000000000000000000c001a04e551c75810ffdfe6caff57da9f5a8732449f42f0f4c57f935b05250a76db3b6a046cd47e6d01914270c1ec0d9ac7fae7dfb240ec9a8b6ec7898c4d6aa174388f2";

        let data = hex::decode(raw).unwrap();
        let tx = base_common_types_chain::BasePooledTransaction::decode_2718(&mut data.as_ref())
            .unwrap();

        BasePooledTransaction::from_pooled(tx.try_into_recovered().unwrap())
    }

    fn eip1559_tx(
        to: Address,
        sender: Address,
        value: u64,
        gas_limit: u64,
    ) -> BasePooledTransaction {
        let tx = base_common_types_chain::TxEip1559 {
            chain_id: 1,
            nonce: 0,
            gas_limit,
            max_fee_per_gas: 1,
            max_priority_fee_per_gas: 0,
            to: to.into(),
            value: U256::from(value),
            ..Default::default()
        };
        let signed = base_common_types_chain::BaseTxEnvelope::new_unhashed(
            tx.into(),
            alloy_primitives::Signature::test_signature(),
        );
        BasePooledTransaction::new(
            base_common_types_chain::transaction::Recovered::new_unchecked(signed, sender),
            200,
        )
    }

    /// EIP-2780 replaces the flat 21k intrinsic base with a decomposed one: 12k base, plus a cold
    /// account access for `tx.to` and a transfer charge when `tx.value` is non-zero, with a
    /// carve-out for self-transfers.
    #[test]
    fn intrinsic_gas_eip2780() {
        let sender = Address::repeat_byte(1);
        let recipient = Address::repeat_byte(2);

        let amsterdam = || ForkTracker {
            shanghai: true.into(),
            cancun: true.into(),
            prague: true.into(),
            osaka: true.into(),
            amsterdam: true.into(),
            tip_timestamp: 0.into(),
            max_blob_count: 0.into(),
            max_initcode_size: AtomicUsize::new(MAX_INITCODE_SIZE),
            tx_gas_limit_cap: AtomicU64::new(0),
        };
        let pre_amsterdam = || ForkTracker { amsterdam: false.into(), ..amsterdam() };

        // Self-transfer: base cost only (12k), where pre-Amsterdam it pays the flat 21k.
        let self_transfer = eip1559_tx(sender, sender, 1, 15_000);
        assert!(ensure_intrinsic_gas(&self_transfer, &amsterdam()).is_ok());
        assert!(ensure_intrinsic_gas(&self_transfer, &pre_amsterdam()).is_err());

        // Zero-value call to another account: base + cold account access (15k).
        let zero_value = eip1559_tx(recipient, sender, 0, 15_000);
        assert!(ensure_intrinsic_gas(&zero_value, &amsterdam()).is_ok());
        assert!(
            ensure_intrinsic_gas(&eip1559_tx(recipient, sender, 0, 14_999), &amsterdam()).is_err()
        );

        // Value transfer to another account: base + cold access + transfer log + value cost (21k).
        assert!(
            ensure_intrinsic_gas(&eip1559_tx(recipient, sender, 1, 15_000), &amsterdam()).is_err()
        );
        assert!(
            ensure_intrinsic_gas(&eip1559_tx(recipient, sender, 1, 21_000), &amsterdam()).is_ok()
        );
    }

    // <https://github.com/paradigmxyz/reth/issues/5178>
    #[tokio::test]
    async fn validate_transaction() {
        let transaction = get_transaction();
        let mut fork_tracker = ForkTracker {
            shanghai: false.into(),
            cancun: false.into(),
            prague: false.into(),
            osaka: false.into(),
            amsterdam: false.into(),
            tip_timestamp: 0.into(),
            max_blob_count: 0.into(),
            max_initcode_size: AtomicUsize::new(MAX_INITCODE_SIZE),
            tx_gas_limit_cap: AtomicU64::new(0),
        };

        let res = ensure_intrinsic_gas(&transaction, &fork_tracker);
        assert!(res.is_ok());

        fork_tracker.shanghai = true.into();
        let res = ensure_intrinsic_gas(&transaction, &fork_tracker);
        assert!(res.is_ok());

        let provider = MockEthProvider::default()
            .with_chain_spec((**test_evm_config().chain_spec()).clone())
            .with_genesis_block();
        provider.add_account(
            transaction.sender(),
            ExtendedAccount::new(transaction.nonce(), U256::MAX),
        );
        let blob_store = InMemoryBlobStore::default();
        let validator = EthTransactionValidatorBuilder::new(provider, test_evm_config()).build();

        let outcome = validator.validate_one(TransactionOrigin::External, transaction.clone());

        assert!(outcome.is_valid());

        let pool = Pool::new(validator, BaseOrdering::default(), blob_store, Default::default());

        let res = pool.add_external_transaction(transaction.clone()).await;
        assert!(res.is_ok());
        let tx = pool.get(transaction.hash());
        assert!(tx.is_some());
    }

    #[test]
    fn accepts_sender_with_empty_bytecode() {
        let transaction = get_transaction();
        let provider = MockEthProvider::default()
            .with_chain_spec((**test_evm_config().chain_spec()).clone())
            .with_genesis_block();
        provider.add_account(
            transaction.sender(),
            ExtendedAccount::new(transaction.nonce(), U256::MAX).with_bytecode(Bytes::new()),
        );
        let validator = EthTransactionValidatorBuilder::new(provider, test_evm_config()).build();

        let outcome = validator.validate_one(TransactionOrigin::External, transaction);

        assert!(outcome.is_valid());
    }

    #[test]
    fn validates_configured_chain_id() {
        let provider = MockEthProvider::default()
            .with_chain_spec((**test_evm_config().chain_spec()).clone())
            .with_genesis_block();
        let validator = EthTransactionValidatorBuilder::new(provider, test_evm_config()).build();
        let transaction = |chain_id| -> BasePooledTransaction {
            BasePooledTransaction::try_from_consensus(
                TransactionBuilder::default()
                    .chain_id(chain_id)
                    .gas_limit(21_000)
                    .to(Address::ZERO)
                    .into_eip1559()
                    .try_into_recovered()
                    .unwrap(),
            )
            .unwrap()
        };

        assert!(
            validator
                .validate_stateless(TransactionOrigin::External, &transaction(validator.chain_id()))
                .is_ok()
        );
        assert!(matches!(
            validator.validate_stateless(
                TransactionOrigin::External,
                &transaction(validator.chain_id() + 1)
            ),
            Err(InvalidPoolTransactionError::Consensus(InvalidTransactionError::ChainIdMismatch))
        ));
    }

    // <https://github.com/paradigmxyz/reth/issues/8550>
    #[tokio::test]
    async fn invalid_on_gas_limit_too_high() {
        let transaction = get_transaction();

        let provider = MockEthProvider::default()
            .with_chain_spec((**test_evm_config().chain_spec()).clone())
            .with_genesis_block();
        provider.add_account(
            transaction.sender(),
            ExtendedAccount::new(transaction.nonce(), U256::MAX),
        );

        let blob_store = InMemoryBlobStore::default();
        let validator = EthTransactionValidatorBuilder::new(provider, test_evm_config())
            .set_block_gas_limit(1_000_000) // tx gas limit is 1_015_288
            .build();

        let outcome = validator.validate_one(TransactionOrigin::External, transaction.clone());

        assert!(outcome.is_invalid());

        let pool = Pool::new(validator, BaseOrdering::default(), blob_store, Default::default());

        let res = pool.add_external_transaction(transaction.clone()).await;
        assert!(res.is_err());
        assert!(matches!(
            res.unwrap_err().kind,
            PoolErrorKind::InvalidTransaction(InvalidPoolTransactionError::ExceedsGasLimit(
                1_015_288, 1_000_000
            ))
        ));
        let tx = pool.get(transaction.hash());
        assert!(tx.is_none());
    }

    #[tokio::test]
    async fn invalid_on_fee_cap_exceeded() {
        let transaction = get_transaction();
        let provider = MockEthProvider::default()
            .with_chain_spec((**test_evm_config().chain_spec()).clone())
            .with_genesis_block();
        provider.add_account(
            transaction.sender(),
            ExtendedAccount::new(transaction.nonce(), U256::MAX),
        );

        let blob_store = InMemoryBlobStore::default();
        let validator = EthTransactionValidatorBuilder::new(provider, test_evm_config())
            .set_tx_fee_cap(100) // 100 wei cap
            .build();

        let outcome = validator.validate_one(TransactionOrigin::Local, transaction.clone());
        assert!(outcome.is_invalid());

        if let TransactionValidationOutcome::Invalid(_, err) = outcome {
            assert!(matches!(
                err,
                InvalidPoolTransactionError::ExceedsFeeCap { max_tx_fee_wei, tx_fee_cap_wei }
                if (max_tx_fee_wei > tx_fee_cap_wei)
            ));
        }

        let pool = Pool::new(validator, BaseOrdering::default(), blob_store, Default::default());
        let res = pool.add_transaction(TransactionOrigin::Local, transaction.clone()).await;
        assert!(res.is_err());
        assert!(matches!(
            res.unwrap_err().kind,
            PoolErrorKind::InvalidTransaction(InvalidPoolTransactionError::ExceedsFeeCap { .. })
        ));
        let tx = pool.get(transaction.hash());
        assert!(tx.is_none());
    }

    #[tokio::test]
    async fn valid_on_zero_fee_cap() {
        let transaction = get_transaction();
        let provider = MockEthProvider::default()
            .with_chain_spec((**test_evm_config().chain_spec()).clone())
            .with_genesis_block();
        provider.add_account(
            transaction.sender(),
            ExtendedAccount::new(transaction.nonce(), U256::MAX),
        );

        let validator = EthTransactionValidatorBuilder::new(provider, BaseEvmConfig::default())
            .set_tx_fee_cap(0) // no cap
            .build();

        let outcome = validator.validate_one(TransactionOrigin::Local, transaction);
        assert!(outcome.is_valid());
    }

    #[tokio::test]
    async fn valid_on_normal_fee_cap() {
        let transaction = get_transaction();
        let provider = MockEthProvider::default()
            .with_chain_spec((**test_evm_config().chain_spec()).clone())
            .with_genesis_block();
        provider.add_account(
            transaction.sender(),
            ExtendedAccount::new(transaction.nonce(), U256::MAX),
        );

        let validator = EthTransactionValidatorBuilder::new(provider, BaseEvmConfig::default())
            .set_tx_fee_cap(2e18 as u128) // 2 ETH cap
            .build();

        let outcome = validator.validate_one(TransactionOrigin::Local, transaction);
        assert!(outcome.is_valid());
    }

    #[tokio::test]
    async fn invalid_on_max_tx_gas_limit_exceeded() {
        let transaction = get_transaction();
        let provider = MockEthProvider::default()
            .with_chain_spec((**test_evm_config().chain_spec()).clone())
            .with_genesis_block();
        provider.add_account(
            transaction.sender(),
            ExtendedAccount::new(transaction.nonce(), U256::MAX),
        );

        let blob_store = InMemoryBlobStore::default();
        let validator = EthTransactionValidatorBuilder::new(provider, BaseEvmConfig::default())
            .with_max_tx_gas_limit(Some(500_000)) // Set limit lower than transaction gas limit (1_015_288)
            .build();

        let outcome = validator.validate_one(TransactionOrigin::External, transaction.clone());
        assert!(outcome.is_invalid());

        let pool = Pool::new(validator, BaseOrdering::default(), blob_store, Default::default());

        let res = pool.add_external_transaction(transaction.clone()).await;
        assert!(res.is_err());
        assert!(matches!(
            res.unwrap_err().kind,
            PoolErrorKind::InvalidTransaction(InvalidPoolTransactionError::MaxTxGasLimitExceeded(
                1_015_288, 500_000
            ))
        ));
        let tx = pool.get(transaction.hash());
        assert!(tx.is_none());
    }

    #[tokio::test]
    async fn valid_on_max_tx_gas_limit_disabled() {
        let transaction = get_transaction();
        let provider = MockEthProvider::default()
            .with_chain_spec((**test_evm_config().chain_spec()).clone())
            .with_genesis_block();
        provider.add_account(
            transaction.sender(),
            ExtendedAccount::new(transaction.nonce(), U256::MAX),
        );

        let validator = EthTransactionValidatorBuilder::new(provider, BaseEvmConfig::default())
            .with_max_tx_gas_limit(None) // disabled
            .build();

        let outcome = validator.validate_one(TransactionOrigin::External, transaction);
        assert!(outcome.is_valid());
    }

    #[tokio::test]
    async fn valid_on_max_tx_gas_limit_within_limit() {
        let transaction = get_transaction();
        let provider = MockEthProvider::default()
            .with_chain_spec((**test_evm_config().chain_spec()).clone())
            .with_genesis_block();
        provider.add_account(
            transaction.sender(),
            ExtendedAccount::new(transaction.nonce(), U256::MAX),
        );

        let validator = EthTransactionValidatorBuilder::new(provider, BaseEvmConfig::default())
            .with_max_tx_gas_limit(Some(2_000_000)) // Set limit higher than transaction gas limit (1_015_288)
            .build();

        let outcome = validator.validate_one(TransactionOrigin::External, transaction);
        assert!(outcome.is_valid());
    }

    // Helper function to set up common test infrastructure for priority fee tests
    fn setup_priority_fee_test() -> (BasePooledTransaction, MockEthProvider) {
        let transaction = get_transaction();
        let provider = MockEthProvider::default()
            .with_chain_spec((**test_evm_config().chain_spec()).clone())
            .with_genesis_block();
        provider.add_account(
            transaction.sender(),
            ExtendedAccount::new(transaction.nonce(), U256::MAX),
        );
        (transaction, provider)
    }

    // Helper function to create a validator with minimum priority fee
    fn create_validator_with_minimum_fee(
        provider: MockEthProvider,
        minimum_priority_fee: Option<u128>,
        local_config: Option<LocalTransactionConfig>,
    ) -> EthTransactionValidator<MockEthProvider> {
        let mut builder = EthTransactionValidatorBuilder::new(provider, test_evm_config())
            .with_minimum_priority_fee(minimum_priority_fee);

        if let Some(config) = local_config {
            builder = builder.with_local_transactions_config(config);
        }

        builder.build()
    }

    #[tokio::test]
    async fn invalid_on_priority_fee_lower_than_configured_minimum() {
        let (transaction, provider) = setup_priority_fee_test();

        // Verify the test transaction is a dynamic fee transaction
        assert!(transaction.is_dynamic_fee());

        // Set minimum priority fee to be double the transaction's priority fee
        let minimum_priority_fee =
            transaction.max_priority_fee_per_gas().expect("priority fee is expected") * 2;

        let validator =
            create_validator_with_minimum_fee(provider, Some(minimum_priority_fee), None);

        // External transaction should be rejected due to low priority fee
        let outcome = validator.validate_one(TransactionOrigin::External, transaction.clone());
        assert!(outcome.is_invalid());

        if let TransactionValidationOutcome::Invalid(_, err) = outcome {
            assert!(matches!(
                err,
                InvalidPoolTransactionError::PriorityFeeBelowMinimum { minimum_priority_fee: min_fee }
                if min_fee == minimum_priority_fee
            ));
        }

        // Test pool integration
        let blob_store = InMemoryBlobStore::default();
        let pool = Pool::new(validator, BaseOrdering::default(), blob_store, Default::default());

        let res = pool.add_external_transaction(transaction.clone()).await;
        assert!(res.is_err());
        assert!(matches!(
            res.unwrap_err().kind,
            PoolErrorKind::InvalidTransaction(
                InvalidPoolTransactionError::PriorityFeeBelowMinimum { .. }
            )
        ));
        let tx = pool.get(transaction.hash());
        assert!(tx.is_none());

        // Local transactions should still be accepted regardless of minimum priority fee
        let (_, local_provider) = setup_priority_fee_test();
        let validator_local =
            create_validator_with_minimum_fee(local_provider, Some(minimum_priority_fee), None);

        let local_outcome = validator_local.validate_one(TransactionOrigin::Local, transaction);
        assert!(local_outcome.is_valid());
    }

    #[tokio::test]
    async fn valid_on_priority_fee_equal_to_minimum() {
        let (transaction, provider) = setup_priority_fee_test();

        // Set minimum priority fee equal to transaction's priority fee
        let tx_priority_fee =
            transaction.max_priority_fee_per_gas().expect("priority fee is expected");
        let validator = create_validator_with_minimum_fee(provider, Some(tx_priority_fee), None);

        let outcome = validator.validate_one(TransactionOrigin::External, transaction);
        assert!(outcome.is_valid());
    }

    #[tokio::test]
    async fn valid_on_priority_fee_above_minimum() {
        let (transaction, provider) = setup_priority_fee_test();

        // Set minimum priority fee below transaction's priority fee
        let tx_priority_fee =
            transaction.max_priority_fee_per_gas().expect("priority fee is expected");
        let minimum_priority_fee = tx_priority_fee / 2; // Half of transaction's priority fee

        let validator =
            create_validator_with_minimum_fee(provider, Some(minimum_priority_fee), None);

        let outcome = validator.validate_one(TransactionOrigin::External, transaction);
        assert!(outcome.is_valid());
    }

    #[tokio::test]
    async fn valid_on_minimum_priority_fee_disabled() {
        let (transaction, provider) = setup_priority_fee_test();

        // No minimum priority fee set (default is None)
        let validator = create_validator_with_minimum_fee(provider, None, None);

        let outcome = validator.validate_one(TransactionOrigin::External, transaction);
        assert!(outcome.is_valid());
    }

    #[tokio::test]
    async fn priority_fee_validation_applies_to_private_transactions() {
        let (transaction, provider) = setup_priority_fee_test();

        // Set minimum priority fee to be double the transaction's priority fee
        let minimum_priority_fee =
            transaction.max_priority_fee_per_gas().expect("priority fee is expected") * 2;

        let validator =
            create_validator_with_minimum_fee(provider, Some(minimum_priority_fee), None);

        // Private transactions are also subject to minimum priority fee validation
        // because they are not considered "local" by default unless specifically configured
        let outcome = validator.validate_one(TransactionOrigin::Private, transaction);
        assert!(outcome.is_invalid());

        if let TransactionValidationOutcome::Invalid(_, err) = outcome {
            assert!(matches!(
                err,
                InvalidPoolTransactionError::PriorityFeeBelowMinimum { minimum_priority_fee: min_fee }
                if min_fee == minimum_priority_fee
            ));
        }
    }

    #[tokio::test]
    async fn valid_on_local_config_exempts_private_transactions() {
        let (transaction, provider) = setup_priority_fee_test();

        // Set minimum priority fee to be double the transaction's priority fee
        let minimum_priority_fee =
            transaction.max_priority_fee_per_gas().expect("priority fee is expected") * 2;

        // Configure local transactions to include all private transactions
        let local_config =
            LocalTransactionConfig { propagate_local_transactions: true, ..Default::default() };

        let validator = create_validator_with_minimum_fee(
            provider,
            Some(minimum_priority_fee),
            Some(local_config),
        );

        // With appropriate local config, the behavior depends on the local transaction logic
        // This test documents the current behavior - private transactions are still validated
        // unless the sender is specifically whitelisted in local_transactions_config
        let outcome = validator.validate_one(TransactionOrigin::Private, transaction);
        assert!(outcome.is_invalid()); // Still invalid because sender not in whitelist
    }

    #[test]
    fn reject_oversized_tx() {
        let mut transaction = get_transaction();
        transaction.encoded_length = DEFAULT_MAX_TX_INPUT_BYTES + 1;
        let provider = MockEthProvider::default()
            .with_chain_spec((**test_evm_config().chain_spec()).clone())
            .with_genesis_block();

        // No minimum priority fee set (default is None)
        let validator = create_validator_with_minimum_fee(provider, None, None);

        let outcome = validator.validate_one(TransactionOrigin::External, transaction);
        let invalid = outcome.as_invalid().unwrap();
        assert!(invalid.is_oversized());
    }

    #[test]
    fn reject_tx_with_oversized_access_list() {
        let max_tx_input_bytes = 512;
        let provider = MockEthProvider::default()
            .with_chain_spec((**test_evm_config().chain_spec()).clone())
            .with_genesis_block();
        let validator = EthTransactionValidatorBuilder::new(provider, test_evm_config())
            .with_max_tx_input_bytes(max_tx_input_bytes)
            .build();

        let tx_with_access_list = |storage_keys: usize| {
            let access_list = AccessList(vec![AccessListItem {
                address: Address::random(),
                storage_keys: (0..storage_keys).map(|_| B256::random()).collect(),
            }]);
            let tx = TransactionBuilder::default()
                .access_list(access_list)
                .into_eip1559()
                .try_into_recovered()
                .unwrap();
            let encoded_length = tx.encode_2718_len();
            BasePooledTransaction::new(tx, encoded_length)
        };

        let is_oversized = |tx: &BasePooledTransaction| {
            matches!(
                validator.validate_stateless(TransactionOrigin::External, tx),
                Err(InvalidPoolTransactionError::OversizedData { .. })
            )
        };

        let small = tx_with_access_list(1);
        assert!(!is_oversized(&small));

        let large = tx_with_access_list(64);
        assert!(large.input().is_empty());
        assert!(is_oversized(&large));
    }

    #[tokio::test]
    async fn valid_with_disabled_balance_check() {
        let transaction = get_transaction();
        let provider = MockEthProvider::default()
            .with_chain_spec((**test_evm_config().chain_spec()).clone())
            .with_genesis_block();

        // Set account with 0 balance
        provider.add_account(
            transaction.sender(),
            ExtendedAccount::new(transaction.nonce(), alloy_primitives::U256::ZERO),
        );

        // Validate with balance check enabled
        let validator =
            EthTransactionValidatorBuilder::new(provider.clone(), BaseEvmConfig::default()).build();

        let outcome = validator.validate_one(TransactionOrigin::External, transaction.clone());
        let expected_cost = *transaction.cost();
        if let TransactionValidationOutcome::Invalid(_, err) = outcome {
            assert!(matches!(
                err,
                InvalidPoolTransactionError::Consensus(InvalidTransactionError::InsufficientFunds(ref funds_err))
                if funds_err.got == alloy_primitives::U256::ZERO && funds_err.expected == expected_cost
            ));
        } else {
            panic!("Expected Invalid outcome with InsufficientFunds error");
        }

        // Validate with balance check disabled
        let validator = EthTransactionValidatorBuilder::new(provider, BaseEvmConfig::default())
            .disable_balance_check()
            .build();

        let outcome = validator.validate_one(TransactionOrigin::External, transaction);
        assert!(outcome.is_valid()); // Should be valid because balance check is disabled
    }
}
