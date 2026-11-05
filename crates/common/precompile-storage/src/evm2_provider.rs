//! Production evm2-backed [`PrecompileStorageProvider`].
//!
//! [`Evm2PrecompileStorageProvider`] borrows the running evm2 [`Evm`] host for one native
//! precompile call and implements [`PrecompileStorageProvider`] against its state journal, with
//! the same gas accounting as the revm-backed
//! [`EvmPrecompileStorageProvider`](crate::EvmPrecompileStorageProvider).

use alloc::{borrow::Cow, string::ToString};
use core::fmt::Display;

use alloy_primitives::{Address, B256, KECCAK256_EMPTY, LogData, U256};
use evm2::{
    Evm, EvmTypes,
    evm::{AccountLoad, StateCheckpoint},
    interpreter::{GasTracker, Host, Message, gas::LOG},
    version::{GasId, GasParams},
};

use crate::{
    error::{BasePrecompileError, Result},
    evm2_compat::Evm2TxOrigin,
    neutral::{AccountInfo, Bytecode, CheckpointStack, StorageCheckpoint},
    provider::{
        KECCAK256_GAS, KECCAK256_WORD_GAS, PrecompileStorageProvider, StorageFeatures,
        validate_loaded_code_presence,
    },
};

/// Production [`PrecompileStorageProvider`] backed by a live evm2 [`Evm`] host.
///
/// Constructed for one precompile call from the host and the call's [`Message`]. Pass `&mut self`
/// to [`StorageCtx::enter`](crate::StorageCtx::enter) so `#[contract]` storage types read and
/// write the evm2 state journal.
#[derive(Debug)]
pub struct Evm2PrecompileStorageProvider<'e, 'db, T: EvmTypes> {
    evm: &'e mut Evm<'db, T>,
    caller: Address,
    call_value: U256,
    gas: GasTracker,
    gas_params: GasParams,
    is_static: bool,
    block_number: u64,
    timestamp: U256,
    chain_id: u64,
    beneficiary: Address,
    origin: Address,
    state_gas_used: u64,
    storage_features: StorageFeatures,
    checkpoints: CheckpointStack<StateCheckpoint>,
}

impl<'e, 'db, T> Evm2PrecompileStorageProvider<'e, 'db, T>
where
    T: EvmTypes,
    T::EvmExt: Evm2TxOrigin,
{
    /// Builds the provider for the precompile call described by `message`.
    ///
    /// `gas_params` drives all EIP-2929/2200/3529 cost calculations.
    pub fn new(
        evm: &'e mut Evm<'db, T>,
        message: &Message<T>,
        gas_params: GasParams,
        storage_features: StorageFeatures,
    ) -> Self {
        let block = evm.block();
        let block_number = block.number.to::<u64>();
        let timestamp = block.timestamp;
        let beneficiary = block.beneficiary;
        let chain_id = evm.version().chain_id;
        let origin = evm.ext().tx_origin();

        Self {
            evm,
            caller: message.caller,
            call_value: message.value,
            gas: GasTracker::new(message.gas_limit),
            gas_params,
            is_static: message.is_static(),
            block_number,
            timestamp,
            chain_id,
            beneficiary,
            origin,
            state_gas_used: 0,
            storage_features,
            checkpoints: CheckpointStack::default(),
        }
    }
}

impl<T: EvmTypes> Evm2PrecompileStorageProvider<'_, '_, T> {
    fn host_failure(error: impl Display) -> BasePrecompileError {
        BasePrecompileError::Fatal(error.to_string())
    }

    /// Loads (and warms) `address` without charging gas.
    fn load_account(&mut self, address: Address, load_code: bool) -> Result<AccountLoad> {
        Host::<T>::load_account(self.evm, &address, load_code, false).map_err(Self::host_failure)
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

    /// EIP-2929 account access: the warm read cost, plus the cold surcharge for a cold account.
    fn charge_account_access(&mut self, is_cold: bool) -> Result<()> {
        self.deduct_gas(self.gas_params.get(GasId::WarmStorageReadCost))?;
        if is_cold {
            self.deduct_gas(self.gas_params.cold_account_additional_cost())?;
        }
        Ok(())
    }

    /// Runs `f`, rolling every state change it made back if it fails.
    fn atomically<R>(&mut self, f: impl FnOnce(&mut Self) -> Result<R>) -> Result<R> {
        let checkpoint = self.evm.state().checkpoint();
        let result = f(self);
        if result.is_err() {
            let features = self.evm.version().features;
            self.evm.state_mut().rollback(checkpoint, features);
        }
        result
    }
}

impl<T> PrecompileStorageProvider for Evm2PrecompileStorageProvider<'_, '_, T>
where
    T: EvmTypes,
{
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
        if self.is_static {
            return Err(BasePrecompileError::StaticCallViolation);
        }

        let code_len = code.len();
        self.deduct_gas(
            self.gas_params.get(GasId::CodeDepositCost).saturating_mul(code_len as u64),
        )?;

        // Charge CREATE-equivalent costs whenever code lands on an account without code, even a
        // prefunded one (see `EvmPrecompileStorageProvider::set_code`).
        let account = self.load_account(address, false)?;
        if Self::code_hash(&account) == KECCAK256_EMPTY {
            self.deduct_gas(self.gas_params.get(GasId::Create))?;
            let num_words = code_len.div_ceil(32) as u64;
            self.deduct_gas(
                KECCAK256_GAS.saturating_add(KECCAK256_WORD_GAS.saturating_mul(num_words)),
            )?;
        }

        self.evm
            .state_mut()
            .account(&address)
            .map_err(Self::host_failure)?
            .set_code_slow(code.into());
        Ok(())
    }

    fn with_account_info(
        &mut self,
        address: Address,
        f: &mut dyn FnMut(&AccountInfo),
    ) -> Result<()> {
        let account = self.load_account(address, false)?;
        self.charge_account_access(account.is_cold)?;
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
        self.charge_account_access(account.is_cold)?;
        f(&code);
        Ok(())
    }

    fn sload(&mut self, address: Address, key: U256) -> Result<U256> {
        self.atomically(|this| {
            // revm loads (and warms) the account before reading a slot; mirror it so later
            // account reads see the same warm state on both engines.
            this.load_account(address, false)?;
            let slot =
                Host::<T>::sload(this.evm, &address, &key, false).map_err(Self::host_failure)?;

            this.deduct_gas(this.gas_params.get(GasId::WarmStorageReadCost))?;
            if slot.is_cold {
                this.deduct_gas(this.gas_params.get(GasId::ColdStorageAdditionalCost))?;
            }
            Ok(slot.value)
        })
    }

    fn tload(&mut self, address: Address, key: U256) -> Result<U256> {
        self.deduct_gas(self.gas_params.get(GasId::WarmStorageReadCost))?;
        Ok(Host::<T>::tload(self.evm, &address, &key))
    }

    fn tload_unmetered(&mut self, address: Address, key: U256) -> Result<U256> {
        Ok(Host::<T>::tload(self.evm, &address, &key))
    }

    fn sstore(&mut self, address: Address, key: U256, value: U256) -> Result<()> {
        if self.is_static {
            return Err(BasePrecompileError::StaticCallViolation);
        }
        // EIP-2200 stipend sentry: see `EvmPrecompileStorageProvider::sstore`.
        if self.gas.remaining() <= self.gas_params.get(GasId::CallStipend) {
            return Err(BasePrecompileError::OutOfGas);
        }
        self.atomically(|this| {
            this.load_account(address, false)?;
            let store = Host::<T>::sstore(this.evm, &address, &key, &value, false)
                .map_err(Self::host_failure)?;

            this.deduct_gas(this.gas_params.get(GasId::SstoreStatic))?;
            this.deduct_gas(this.gas_params.sstore_dynamic_gas(true, &store))?;
            this.refund_gas(this.gas_params.sstore_refund(true, &store));
            Ok(())
        })
    }

    fn tstore(&mut self, address: Address, key: U256, value: U256) -> Result<()> {
        if self.is_static {
            return Err(BasePrecompileError::StaticCallViolation);
        }
        self.deduct_gas(self.gas_params.get(GasId::WarmStorageReadCost))?;
        Host::<T>::tstore(self.evm, &address, &key, &value);
        Ok(())
    }

    fn emit_event(&mut self, address: Address, event: LogData) -> Result<()> {
        if self.is_static {
            return Err(BasePrecompileError::StaticCallViolation);
        }
        let cost = u64::from(LOG)
            .saturating_add(self.gas_params.log_cost(event.topics().len() as u8, event.data.len()));
        self.deduct_gas(cost)?;
        Host::<T>::log(
            self.evm,
            &address,
            Cow::Borrowed(event.topics()),
            Cow::Borrowed(event.data.as_ref()),
        );
        Ok(())
    }

    fn deduct_gas(&mut self, gas: u64) -> Result<()> {
        self.gas.try_spend(gas).map_err(|_| BasePrecompileError::OutOfGas)
    }

    fn deduct_state_gas(&mut self, gas: u64) -> Result<()> {
        // No separate reservoir in the precompile context; state gas is drawn from regular gas.
        self.deduct_gas(gas)?;
        self.state_gas_used = self.state_gas_used.saturating_add(gas);
        Ok(())
    }

    fn refund_gas(&mut self, gas: i64) {
        self.gas.record_refund(gas);
    }

    fn gas_limit(&self) -> u64 {
        self.gas.limit()
    }

    fn gas_used(&self) -> u64 {
        self.gas.spent()
    }

    fn state_gas_used(&self) -> u64 {
        self.state_gas_used
    }

    fn gas_refunded(&self) -> i64 {
        self.gas.refunded()
    }

    fn reservoir(&self) -> u64 {
        0
    }

    fn storage_features(&self) -> StorageFeatures {
        self.storage_features
    }

    fn is_static(&self) -> bool {
        self.is_static
    }

    fn call_value(&self) -> U256 {
        self.call_value
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
}
