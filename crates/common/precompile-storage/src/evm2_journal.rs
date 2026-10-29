//! Gas-free evm2-state-backed [`PrecompileStorageProvider`].
//!
//! [`Evm2JournalStorageProvider`] is the evm2 counterpart of the revm
//! [`JournalStorageProvider`](crate::JournalStorageProvider): it reads and writes the live evm2
//! [`Evm`] state without metering any gas. It exists for the *enshrined* EIP-8130 execution path,
//! which runs around (not inside) an EVM call and meters its own gas via the EIP-8130 intrinsic
//! schedule; the metered [`Evm2PrecompileStorageProvider`](crate::Evm2PrecompileStorageProvider)
//! would double-count storage gas there.
//!
//! Account and slot accesses warm exactly what the revm journal provider warms, so the calls that
//! follow observe the same EIP-2929 warm set on both engines.

use alloc::{
    borrow::Cow,
    string::{String, ToString},
};
use core::fmt::Display;

use alloy_primitives::{Address, B256, KECCAK256_EMPTY, LogData, U256, keccak256};
use evm2::{
    Evm, EvmTypes,
    evm::{AccountLoad, StateCheckpoint},
    interpreter::Host,
};

use crate::{
    error::{BasePrecompileError, Result},
    evm2_compat::Evm2TxOrigin,
    neutral::{AccountInfo, Bytecode, CheckpointStack, StorageCheckpoint},
    provider::{PrecompileStorageProvider, validate_loaded_code_presence},
};

/// Gas-free [`PrecompileStorageProvider`] backed by a live evm2 [`Evm`] state.
///
/// Pass `&mut self` to [`StorageCtx::enter`](crate::StorageCtx::enter) so `#[contract]` storage
/// types read and write the evm2 state. All gas accounting methods are no-ops:
/// [`gas_limit`](PrecompileStorageProvider::gas_limit) reports [`u64::MAX`] and nothing is ever
/// deducted.
///
/// State failures surface to the storage layer as [`BasePrecompileError::Fatal`], which callers
/// may stringify into a transaction-level error. The provider therefore also records the first
/// such failure so the caller can tell a database fault apart from an invalid transaction via
/// [`take_database_error`](Self::take_database_error).
#[derive(Debug)]
pub struct Evm2JournalStorageProvider<'e, 'db, T: EvmTypes> {
    evm: &'e mut Evm<'db, T>,
    database_error: Option<String>,
    caller: Address,
    block_number: u64,
    timestamp: U256,
    chain_id: u64,
    beneficiary: Address,
    origin: Address,
    checkpoints: CheckpointStack<StateCheckpoint>,
}

impl<'e, 'db, T> Evm2JournalStorageProvider<'e, 'db, T>
where
    T: EvmTypes,
    T::EvmExt: Evm2TxOrigin,
{
    /// Builds a gas-free provider over `evm`, attributing storage access to `caller`. Block
    /// metadata and the transaction origin are snapshotted from the EVM.
    pub fn new(evm: &'e mut Evm<'db, T>, caller: Address) -> Self {
        let block = evm.block();
        let block_number = block.number.to::<u64>();
        let timestamp = block.timestamp;
        let beneficiary = block.beneficiary;
        let chain_id = evm.version().chain_id;
        let origin = evm.ext().tx_origin();
        Self {
            evm,
            database_error: None,
            caller,
            block_number,
            timestamp,
            chain_id,
            beneficiary,
            origin,
            checkpoints: CheckpointStack::default(),
        }
    }
}

impl<T: EvmTypes> Evm2JournalStorageProvider<'_, '_, T> {
    /// Takes the first state failure observed since construction (or the previous take), if any.
    pub const fn take_database_error(&mut self) -> Option<String> {
        self.database_error.take()
    }

    fn database_failure(
        database_error: &mut Option<String>,
        error: impl Display,
    ) -> BasePrecompileError {
        let message = error.to_string();
        database_error.get_or_insert_with(|| message.clone());
        BasePrecompileError::Fatal(message)
    }

    /// Loads (and warms) `address`.
    fn load_account(&mut self, address: Address, load_code: bool) -> Result<AccountLoad> {
        Host::<T>::load_account(self.evm, &address, load_code, false)
            .map_err(|error| Self::database_failure(&mut self.database_error, error))
    }

    /// Code hash of a loaded account, reporting accounts without code as [`KECCAK256_EMPTY`].
    ///
    /// evm2 reports a zero code hash for accounts absent from state, where revm reports the empty
    /// code hash.
    fn code_hash(account: &AccountLoad) -> B256 {
        if !account.exists || account.code_hash.is_zero() {
            KECCAK256_EMPTY
        } else {
            account.code_hash
        }
    }
}

impl<T: EvmTypes> PrecompileStorageProvider for Evm2JournalStorageProvider<'_, '_, T> {
    fn chain_id(&self) -> u64 {
        self.chain_id
    }

    fn timestamp(&self) -> U256 {
        self.timestamp
    }

    fn beneficiary(&self) -> Address {
        self.beneficiary
    }

    fn block_number(&self) -> u64 {
        self.block_number
    }

    fn origin(&self) -> Address {
        self.origin
    }

    fn set_code(&mut self, address: Address, code: Bytecode) -> Result<()> {
        self.evm
            .state_mut()
            .account(&address)
            .map_err(|error| Self::database_failure(&mut self.database_error, error))?
            .set_code_slow(code.into());
        Ok(())
    }

    fn with_account_info(
        &mut self,
        address: Address,
        f: &mut dyn FnMut(&AccountInfo),
    ) -> Result<()> {
        let account = self.load_account(address, false)?;
        f(&AccountInfo {
            balance: account.balance,
            nonce: account.nonce,
            code_hash: Self::code_hash(&account),
        });
        Ok(())
    }

    fn with_account_code(&mut self, address: Address, f: &mut dyn FnMut(&Bytecode)) -> Result<()> {
        let account = self.load_account(address, true)?;
        let code = Bytecode::from(&account.code);
        validate_loaded_code_presence(account.code_hash, &code)?;
        f(&code);
        Ok(())
    }

    fn sload(&mut self, address: Address, key: U256) -> Result<U256> {
        // revm loads (and warms) the account before reading a slot; mirror it so later account
        // reads see the same warm state on both engines.
        self.load_account(address, false)?;
        Host::<T>::sload(self.evm, &address, &key, false)
            .map(|slot| slot.value)
            .map_err(|error| Self::database_failure(&mut self.database_error, error))
    }

    fn tload(&mut self, address: Address, key: U256) -> Result<U256> {
        Ok(Host::<T>::tload(self.evm, &address, &key))
    }

    fn tload_unmetered(&mut self, address: Address, key: U256) -> Result<U256> {
        Ok(Host::<T>::tload(self.evm, &address, &key))
    }

    fn sstore(&mut self, address: Address, key: U256, value: U256) -> Result<()> {
        // Writing only mutates storage; it deliberately does not touch nonce/balance/code. Code-less
        // enshrined system accounts that hold persistent storage (e.g. the 2D `NonceManager`) are
        // made EIP-161-non-empty out of band by the Everest transition's code stub.
        self.load_account(address, false)?;
        Host::<T>::sstore(self.evm, &address, &key, &value, false)
            .map(|_| ())
            .map_err(|error| Self::database_failure(&mut self.database_error, error))
    }

    fn tstore(&mut self, address: Address, key: U256, value: U256) -> Result<()> {
        Host::<T>::tstore(self.evm, &address, &key, &value);
        Ok(())
    }

    fn emit_event(&mut self, address: Address, event: LogData) -> Result<()> {
        Host::<T>::log(
            self.evm,
            &address,
            Cow::Borrowed(event.topics()),
            Cow::Borrowed(event.data.as_ref()),
        );
        Ok(())
    }

    fn deduct_gas(&mut self, _gas: u64) -> Result<()> {
        Ok(())
    }

    fn deduct_state_gas(&mut self, _gas: u64) -> Result<()> {
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
        self.caller
    }

    fn replace_caller(&mut self, caller: Address) -> Address {
        core::mem::replace(&mut self.caller, caller)
    }

    fn checkpoint(&mut self) -> StorageCheckpoint {
        self.checkpoints.push(self.evm.state().checkpoint())
    }

    fn commit_latest_checkpoint(&mut self) {
        self.checkpoints.commit_latest();
    }

    fn checkpoint_revert(&mut self, checkpoint: StorageCheckpoint) {
        if let Some(checkpoint) = self.checkpoints.take(checkpoint) {
            let features = self.evm.version().features;
            self.evm.state_mut().rollback(checkpoint, features);
        }
    }

    fn metered_keccak256(&mut self, data: &[u8]) -> Result<B256> {
        Ok(keccak256(data))
    }
}
