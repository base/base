//! EVM traits.

use crate::{env::BlockEnvironment, Database};
use alloc::boxed::Box;
use alloy_primitives::{Address, Bytes, Log, TxKind, B256, U256};
use core::{error::Error, fmt, fmt::Debug};
use revm::{
    context::{
        journaled_state::{
            account::JournaledAccountTr, JournalCheckpoint, JournalLoadError, TransferError,
        },
        result::InvalidTransaction,
        Cfg, ContextTr, DBErrorMarker, JournalTr,
    },
    interpreter::{SStoreResult, StateLoad},
    primitives::{StorageKey, StorageValue},
    state::{Account, AccountInfo, Bytecode},
};

/// Erased error type.
#[derive(thiserror::Error, Debug)]
#[error(transparent)]
pub struct ErasedError(Box<dyn Error + Send + Sync + 'static>);

impl ErasedError {
    /// Creates a new [`ErasedError`].
    pub fn new(error: impl Error + Send + Sync + 'static) -> Self {
        Self(Box::new(error))
    }
}

impl DBErrorMarker for ErasedError {}

/// Errors returned by [`EvmInternals`].
#[derive(Debug, thiserror::Error)]
pub enum EvmInternalsError {
    /// Database error.
    #[error(transparent)]
    Database(ErasedError),
}

impl EvmInternalsError {
    /// Creates a new [`EvmInternalsError::Database`]
    pub fn database(err: impl Error + Send + Sync + 'static) -> Self {
        Self::Database(ErasedError::new(err))
    }
}

/// Dyn-compatible wrapper around [`revm::context::Transaction`].
///
/// [`revm::context::Transaction`] is not dyn-compatible because of methods returning
/// associated types (e.g. `access_list`, `authorization_list`). This trait mirrors
/// the dyn-compatible subset of those methods, allowing transaction data to be
/// accessed through `&dyn TransactionTr` in [`EvmInternals`].
pub trait TransactionTr {
    /// Returns the transaction type.
    ///
    /// Depending on this field other functions should be called.
    fn tx_type(&self) -> u8;

    /// Caller aka Author aka transaction signer.
    ///
    /// Note : Common field for all transactions.
    fn caller(&self) -> Address;

    /// The maximum amount of gas the transaction can use.
    ///
    /// Note : Common field for all transactions.
    fn gas_limit(&self) -> u64;

    /// The value sent to the receiver of [`TxKind::Call`].
    ///
    /// Note : Common field for all transactions.
    fn value(&self) -> U256;

    /// Returns the input data of the transaction.
    ///
    /// Note : Common field for all transactions.
    fn input(&self) -> &Bytes;

    /// The nonce of the transaction.
    ///
    /// Note : Common field for all transactions.
    fn nonce(&self) -> u64;

    /// Transaction kind. It can be Call or Create.
    ///
    /// Kind is applicable for: Legacy, EIP-2930, EIP-1559
    /// And is Call for EIP-4844 and EIP-7702 transactions.
    fn kind(&self) -> TxKind;

    /// Chain Id is optional for legacy transactions.
    ///
    /// As it was introduced in EIP-155.
    fn chain_id(&self) -> Option<u64>;

    /// Gas price for the transaction.
    /// It is only applicable for Legacy and EIP-2930 transactions.
    /// For Eip1559 it is max_fee_per_gas.
    fn gas_price(&self) -> u128;

    /// Returns vector of fixed size hash(32 bytes)
    ///
    /// Note : EIP-4844 transaction field.
    fn blob_versioned_hashes(&self) -> &[B256];

    /// Max fee per data gas
    ///
    /// Note : EIP-4844 transaction field.
    fn max_fee_per_blob_gas(&self) -> u128;

    /// Total gas for all blobs. Max number of blocks is already checked
    /// so we dont need to check for overflow.
    fn total_blob_gas(&self) -> u64;

    /// Calculates the maximum [EIP-4844] `data_fee` of the transaction.
    ///
    /// This is used for ensuring that the user has at least enough funds to pay the
    /// `max_fee_per_blob_gas * total_blob_gas`, on top of regular gas costs.
    ///
    /// See EIP-4844:
    /// <https://github.com/ethereum/EIPs/blob/master/EIPS/eip-4844.md#execution-layer-validation>
    fn calc_max_data_fee(&self) -> U256;

    /// Returns length of the authorization list.
    ///
    /// # Note
    ///
    /// Transaction is considered invalid if list is empty.
    fn authorization_list_len(&self) -> usize;

    /// Returns maximum fee that can be paid for the transaction.
    fn max_fee_per_gas(&self) -> u128;

    /// Maximum priority fee per gas.
    fn max_priority_fee_per_gas(&self) -> Option<u128>;

    /// Returns effective gas price is gas price field for Legacy and Eip2930 transaction.
    ///
    /// While for transactions after Eip1559 it is minimum of max_fee and `base + max_priority_fee`.
    fn effective_gas_price(&self, base_fee: u128) -> u128;

    /// Returns the maximum balance that can be spent by the transaction.
    ///
    /// Return U256 or error if all values overflow U256 number.
    fn max_balance_spending(&self) -> Result<U256, InvalidTransaction>;

    /// Returns the effective balance that is going to be spent that depends on base_fee
    /// Multiplication for gas are done in u128 type (saturated) and value is added as U256 type.
    ///
    /// # Reason
    ///
    /// This is done for performance reasons and it is known to be safe as there is no more that
    /// u128::MAX value of eth in existence.
    ///
    /// This is always strictly less than [`Self::max_balance_spending`].
    ///
    /// Return U256 or error if all values overflow U256 number.
    fn effective_balance_spending(
        &self,
        base_fee: u128,
        blob_price: u128,
    ) -> Result<U256, InvalidTransaction>;
}

impl<T> TransactionTr for T
where
    T: revm::context::Transaction,
{
    fn tx_type(&self) -> u8 {
        revm::context::Transaction::tx_type(self)
    }

    fn caller(&self) -> Address {
        revm::context::Transaction::caller(self)
    }

    fn gas_limit(&self) -> u64 {
        revm::context::Transaction::gas_limit(self)
    }

    fn value(&self) -> U256 {
        revm::context::Transaction::value(self)
    }

    fn input(&self) -> &Bytes {
        revm::context::Transaction::input(self)
    }

    fn nonce(&self) -> u64 {
        revm::context::Transaction::nonce(self)
    }

    fn kind(&self) -> TxKind {
        revm::context::Transaction::kind(self)
    }

    fn chain_id(&self) -> Option<u64> {
        revm::context::Transaction::chain_id(self)
    }

    fn gas_price(&self) -> u128 {
        revm::context::Transaction::gas_price(self)
    }

    fn blob_versioned_hashes(&self) -> &[B256] {
        revm::context::Transaction::blob_versioned_hashes(self)
    }

    fn max_fee_per_blob_gas(&self) -> u128 {
        revm::context::Transaction::max_fee_per_blob_gas(self)
    }

    fn total_blob_gas(&self) -> u64 {
        revm::context::Transaction::total_blob_gas(self)
    }

    fn calc_max_data_fee(&self) -> U256 {
        revm::context::Transaction::calc_max_data_fee(self)
    }

    fn authorization_list_len(&self) -> usize {
        revm::context::Transaction::authorization_list_len(self)
    }

    fn max_fee_per_gas(&self) -> u128 {
        revm::context::Transaction::max_fee_per_gas(self)
    }

    fn max_priority_fee_per_gas(&self) -> Option<u128> {
        revm::context::Transaction::max_priority_fee_per_gas(self)
    }

    fn effective_gas_price(&self, base_fee: u128) -> u128 {
        revm::context::Transaction::effective_gas_price(self, base_fee)
    }

    fn max_balance_spending(&self) -> Result<U256, InvalidTransaction> {
        revm::context::Transaction::max_balance_spending(self)
    }

    fn effective_balance_spending(
        &self,
        base_fee: u128,
        blob_price: u128,
    ) -> Result<U256, InvalidTransaction> {
        revm::context::Transaction::effective_balance_spending(self, base_fee, blob_price)
    }
}

/// dyn-compatible trait for accessing and modifying EVM internals, particularly the journal.
///
/// This trait provides an abstraction over journal operations without exposing
/// associated types, making it object-safe and suitable for dynamic dispatch.
trait EvmInternalsTr: Database<Error = ErasedError> + Debug {
    fn load_account(&mut self, address: Address) -> Result<StateLoad<&Account>, EvmInternalsError>;

    fn load_account_mut_skip_cold_load<'a>(
        &'a mut self,
        address: Address,
        skip_cold_load: bool,
    ) -> Result<StateLoad<Box<dyn JournaledAccountTr + 'a>>, JournalLoadError<EvmInternalsError>>;

    /// Loads an account mutably.
    fn load_account_mut<'a>(
        &'a mut self,
        address: Address,
    ) -> Result<StateLoad<Box<dyn JournaledAccountTr + 'a>>, EvmInternalsError> {
        // safe to unwrap as skip_cold_load is false.
        self.load_account_mut_skip_cold_load(address, false)
            .map_err(JournalLoadError::unwrap_db_error)
    }

    fn load_account_code<'a>(
        &'a mut self,
        address: Address,
    ) -> Result<StateLoad<Box<dyn JournaledAccountTr + 'a>>, EvmInternalsError> {
        let mut account = self.load_account_mut(address)?;
        account.load_code().map_err(|e| EvmInternalsError::database(e.unwrap_db_error()))?;
        Ok(account)
    }

    /// Increments the balance of the account.
    fn balance_incr(&mut self, address: Address, balance: U256) -> Result<(), EvmInternalsError> {
        self.load_account_mut(address)?.incr_balance(balance);
        Ok(())
    }

    /// Sets the balance of the the account
    ///
    /// Touches the account in all cases.
    ///
    /// If the given `balance` is the same as the account's, no journal entry is created.
    fn set_balance(&mut self, address: Address, balance: U256) -> Result<(), EvmInternalsError> {
        self.load_account_mut(address)?.set_balance(balance);
        Ok(())
    }

    /// Transfers the balance from one account to another.
    ///
    /// This will load both accounts
    fn transfer(
        &mut self,
        from: Address,
        to: Address,
        balance: U256,
    ) -> Result<Option<TransferError>, EvmInternalsError>;

    /// Increments the nonce of the account.
    ///
    /// This creates a new journal entry with this change.
    fn bump_nonce(&mut self, address: Address) -> Result<(), EvmInternalsError> {
        self.load_account_mut(address)?.bump_nonce();
        Ok(())
    }

    fn sload(
        &mut self,
        address: Address,
        key: StorageKey,
    ) -> Result<StateLoad<StorageValue>, EvmInternalsError> {
        self.load_account_mut(address)?
            .sload(key, false)
            .map(|i| i.map(|i| i.present_value()))
            .map_err(|e| EvmInternalsError::database(e.unwrap_db_error()))
    }

    fn touch_account(&mut self, address: Address) -> Result<(), EvmInternalsError> {
        self.load_account_mut(address)?.touch();
        Ok(())
    }

    /// Sets bytecode to the account. Internally calls [`EvmInternalsTr::set_code_with_hash`].
    ///
    /// This will load the account, mark it as touched and set the code and code hash.
    /// It will return an error if database error occurs.
    fn set_code(&mut self, address: Address, code: Bytecode) -> Result<(), EvmInternalsError> {
        let hash = code.hash_slow();
        self.set_code_with_hash(address, code, hash)
    }

    /// Sets bytecode with hash to the account.
    ///
    /// This will load the account, mark it as touched and set the code and code hash.
    /// It will return an error if database error occurs.
    fn set_code_with_hash(
        &mut self,
        address: Address,
        code: Bytecode,
        hash: B256,
    ) -> Result<(), EvmInternalsError> {
        self.load_account_mut(address)?.set_code(hash, code);
        Ok(())
    }

    fn sstore(
        &mut self,
        address: Address,
        key: StorageKey,
        value: StorageValue,
    ) -> Result<StateLoad<SStoreResult>, EvmInternalsError> {
        self.load_account_mut(address)?
            .sstore(key, value, false)
            .map_err(|e| EvmInternalsError::database(e.unwrap_db_error()))
    }

    fn log(&mut self, log: Log);

    fn tload(&mut self, address: Address, key: StorageKey) -> StorageValue;

    fn tstore(&mut self, address: Address, key: StorageKey, value: StorageValue);

    fn checkpoint(&mut self) -> JournalCheckpoint;

    fn checkpoint_commit(&mut self);

    fn checkpoint_revert(&mut self, checkpoint: JournalCheckpoint);
}

/// Helper internal struct for implementing [`EvmInternals`].
#[derive(Debug)]
struct EvmInternalsImpl<'a, T>(&'a mut T);

impl<T> revm::Database for EvmInternalsImpl<'_, T>
where
    T: JournalTr<Database: Database>,
{
    type Error = ErasedError;

    fn basic(&mut self, address: Address) -> Result<Option<AccountInfo>, Self::Error> {
        self.0.db_mut().basic(address).map_err(ErasedError::new)
    }

    fn code_by_hash(&mut self, code_hash: B256) -> Result<Bytecode, Self::Error> {
        self.0.db_mut().code_by_hash(code_hash).map_err(ErasedError::new)
    }

    fn storage(
        &mut self,
        address: Address,
        index: StorageKey,
    ) -> Result<StorageValue, Self::Error> {
        self.0.db_mut().storage(address, index).map_err(ErasedError::new)
    }

    fn block_hash(&mut self, number: u64) -> Result<B256, Self::Error> {
        self.0.db_mut().block_hash(number).map_err(ErasedError::new)
    }
}

impl<T> EvmInternalsTr for EvmInternalsImpl<'_, T>
where
    T: JournalTr<Database: Database> + Debug,
{
    fn load_account(&mut self, address: Address) -> Result<StateLoad<&Account>, EvmInternalsError> {
        self.0.load_account(address).map_err(EvmInternalsError::database)
    }

    fn load_account_mut_skip_cold_load<'a>(
        &'a mut self,
        address: Address,
        skip_cold_load: bool,
    ) -> Result<StateLoad<Box<dyn JournaledAccountTr + 'a>>, JournalLoadError<EvmInternalsError>>
    {
        self.0
            .load_account_mut_skip_cold_load(address, skip_cold_load)
            .map(|state_load| {
                state_load.map(|journaled_account| {
                    Box::new(journaled_account) as Box<dyn JournaledAccountTr + 'a>
                })
            })
            .map_err(|e| e.map(EvmInternalsError::database))
    }

    fn transfer(
        &mut self,
        from: Address,
        to: Address,
        balance: U256,
    ) -> Result<Option<TransferError>, EvmInternalsError> {
        self.0.transfer(from, to, balance).map_err(EvmInternalsError::database)
    }

    fn log(&mut self, log: Log) {
        self.0.log(log);
    }

    fn tload(&mut self, address: Address, key: StorageKey) -> StorageValue {
        self.0.tload(address, key)
    }

    fn tstore(&mut self, address: Address, key: StorageKey, value: StorageValue) {
        self.0.tstore(address, key, value);
    }

    fn checkpoint(&mut self) -> JournalCheckpoint {
        self.0.checkpoint()
    }

    fn checkpoint_commit(&mut self) {
        self.0.checkpoint_commit()
    }

    fn checkpoint_revert(&mut self, checkpoint: JournalCheckpoint) {
        self.0.checkpoint_revert(checkpoint)
    }
}

/// Helper type exposing hooks into EVM and access to evm internal settings.
pub struct EvmInternals<'a> {
    internals: Box<dyn EvmInternalsTr + 'a>,
    block_env: &'a dyn BlockEnvironment,
    chain_id: u64,
    tx_origin: Address,
    tx_env: &'a dyn TransactionTr,
}

impl<'a> EvmInternals<'a> {
    /// Creates a new [`EvmInternals`] instance.
    pub fn new<T>(
        journal: &'a mut T,
        block_env: &'a dyn BlockEnvironment,
        cfg_env: &'a impl Cfg,
        tx_env: &'a dyn TransactionTr,
    ) -> Self
    where
        T: JournalTr<Database: Database> + Debug,
    {
        Self {
            internals: Box::new(EvmInternalsImpl(journal)),
            block_env,
            chain_id: cfg_env.chain_id(),
            tx_origin: tx_env.caller(),
            tx_env,
        }
    }

    /// Creates a new [`EvmInternals`] instance from a [`ContextTr`].
    pub fn from_context<CTX>(ctx: &'a mut CTX) -> Self
    where
        CTX: ContextTr<Block: BlockEnvironment, Journal: JournalTr<Database: Database> + Debug>,
    {
        let (block, tx, cfg, journaled_state, ..) = ctx.all_mut();
        Self::new(journaled_state, block, cfg, tx)
    }

    /// Returns the  evm's block information.
    pub const fn block_env(&self) -> &dyn BlockEnvironment {
        self.block_env
    }

    /// Attempts to downcast the block environment to a concrete type.
    pub fn block_env_downcast_ref<T: BlockEnvironment>(&self) -> Option<&T> {
        (self.block_env as &dyn core::any::Any).downcast_ref()
    }

    /// Returns the current block number.
    pub fn block_number(&self) -> U256 {
        self.block_env.number()
    }

    /// Returns the current block timestamp.
    pub fn block_timestamp(&self) -> U256 {
        self.block_env.timestamp()
    }

    /// Returns the chain ID.
    pub const fn chain_id(&self) -> u64 {
        self.chain_id
    }

    /// Returns the caller of the top-level call.
    ///
    /// Note that this might be different from the caller of the specific precompile call.
    pub const fn tx_origin(&self) -> Address {
        self.tx_origin
    }

    /// Returns the current transaction information.
    pub fn tx_env(&self) -> &dyn TransactionTr {
        self.tx_env
    }

    /// Returns a mutable reference to [`Database`] implementation with erased error type.
    ///
    /// Users should prefer using other methods for accessing state that rely on cached state in the
    /// journal instead.
    pub fn db_mut(&mut self) -> impl Database<Error = ErasedError> + '_ {
        &mut *self.internals
    }

    /// Loads an account.
    pub fn load_account(
        &mut self,
        address: Address,
    ) -> Result<StateLoad<&Account>, EvmInternalsError> {
        self.internals.load_account(address)
    }

    /// Loads an account.
    pub fn load_account_mut<'b>(
        &'b mut self,
        address: Address,
    ) -> Result<StateLoad<Box<dyn JournaledAccountTr + 'b>>, EvmInternalsError> {
        self.internals.load_account_mut(address)
    }

    /// Loads an account mutably, skipping cold load if specified.
    pub fn load_account_mut_skip_cold_load<'b>(
        &'b mut self,
        address: Address,
        skip_cold_load: bool,
    ) -> Result<StateLoad<Box<dyn JournaledAccountTr + 'b>>, JournalLoadError<EvmInternalsError>>
    {
        self.internals.load_account_mut_skip_cold_load(address, skip_cold_load)
    }

    /// Loads an account AND it's code.
    pub fn load_account_code<'b>(
        &'b mut self,
        address: Address,
    ) -> Result<StateLoad<Box<dyn JournaledAccountTr + 'b>>, EvmInternalsError> {
        self.internals.load_account_code(address)
    }

    /// Increments the balance of the account.
    pub fn balance_incr(
        &mut self,
        address: Address,
        balance: U256,
    ) -> Result<(), EvmInternalsError> {
        self.internals.balance_incr(address, balance)
    }

    /// Sets the balance of the the account
    ///
    /// Touches the account in all cases.
    ///
    /// If the given `balance` is the same as the account's, no journal entry is created.
    pub fn set_balance(
        &mut self,
        address: Address,
        balance: U256,
    ) -> Result<(), EvmInternalsError> {
        self.internals.set_balance(address, balance)
    }

    /// Transfers the balance from one account to another.
    ///
    /// This will load both accounts and return an error if the transfer fails.
    pub fn transfer(
        &mut self,
        from: Address,
        to: Address,
        balance: U256,
    ) -> Result<Option<TransferError>, EvmInternalsError> {
        self.internals.transfer(from, to, balance)
    }

    /// Increments the nonce of the account.
    ///
    /// This creates a new journal entry with this change.
    pub fn bump_nonce(&mut self, address: Address) -> Result<(), EvmInternalsError> {
        self.internals.bump_nonce(address)
    }

    /// Loads a storage slot.
    pub fn sload(
        &mut self,
        address: Address,
        key: StorageKey,
    ) -> Result<StateLoad<StorageValue>, EvmInternalsError> {
        self.internals.sload(address, key)
    }

    /// Touches the account.
    ///
    /// This will load the account and return an error if database error occurs.
    pub fn touch_account(&mut self, address: Address) -> Result<(), EvmInternalsError> {
        self.internals.touch_account(address)
    }

    /// Sets bytecode to the account.
    ///
    /// This will load the account, mark it as touched and set the code and code hash.
    /// It will return an error if database error occurs.
    pub fn set_code(&mut self, address: Address, code: Bytecode) -> Result<(), EvmInternalsError> {
        self.internals.set_code(address, code)
    }

    /// Sets bytecode with hash to the account.
    ///
    /// This will load the account, mark it as touched and set the code and code hash.
    /// It will return an error if database error occurs.
    pub fn set_code_with_hash(
        &mut self,
        address: Address,
        code: Bytecode,
        hash: B256,
    ) -> Result<(), EvmInternalsError> {
        self.internals.set_code_with_hash(address, code, hash)
    }

    /// Stores the storage value in Journal state.
    ///
    /// This will load the account and storage value and return an error if database error occurs.
    pub fn sstore(
        &mut self,
        address: Address,
        key: StorageKey,
        value: StorageValue,
    ) -> Result<StateLoad<SStoreResult>, EvmInternalsError> {
        self.internals.sstore(address, key, value)
    }

    /// Logs the log in Journal state.
    pub fn log(&mut self, log: Log) {
        self.internals.log(log);
    }

    /// Loads a transient storage value.
    pub fn tload(&mut self, address: Address, key: StorageKey) -> StorageValue {
        self.internals.tload(address, key)
    }

    /// Stores a transient storage value.
    pub fn tstore(&mut self, address: Address, key: StorageKey, value: StorageValue) {
        self.internals.tstore(address, key, value);
    }

    /// Creates a journal checkpoint.
    pub fn checkpoint(&mut self) -> JournalCheckpoint {
        self.internals.checkpoint()
    }

    /// Commits the last journal checkpoint.
    pub fn checkpoint_commit(&mut self) {
        self.internals.checkpoint_commit()
    }

    /// Reverts to a previously created journal checkpoint.
    pub fn checkpoint_revert(&mut self, checkpoint: JournalCheckpoint) {
        self.internals.checkpoint_revert(checkpoint)
    }
}

impl<'a> fmt::Debug for EvmInternals<'a> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("EvmInternals")
            .field("internals", &self.internals)
            .field("block_env", &"{{}}")
            .finish_non_exhaustive()
    }
}
