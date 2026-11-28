//! Base native precompiles on evm2.
//!
//! [`BasePrecompiles`] is the precompile provider the EVM runs with: the Ethereum set for the spec
//! (with the Base input caps, from [`BaseEvmTypes::precompiles`]) plus the Base native set
//! ([`NativePrecompiles`]). [`EvmStorageProvider`] is the
//! [`PrecompileStorageProvider`] the native precompiles run on: it reads and writes the live evm2
//! state, charges gas, and opens checkpoints exactly as base-reth's
//! `EvmPrecompileStorageProvider` does on the revm journal:
//!
//! - every storage access first loads and warms the owning account (revm's `load_account_mut`),
//!   and SSTORE also touches it;
//! - SLOAD/SSTORE run under a checkpoint that reverts the warming when gas runs out;
//! - gas comes from the fixed [`GasSchedule`] (revm's default Osaka table, whatever the fork);
//! - the call's gas is tracked here and applied to the frame's [`GasTracker`] once, from the
//!   precompile output, as revm's `precompile_output_to_interpreter_result` does.

use alloy_primitives::{Address, B256, Bytes, KECCAK256_EMPTY, Log, LogData, U256};
use base_tiger_precompiles::{
    AccountInfo, BasePrecompileError, Checkpoint, GasSchedule, NativePrecompile, NativePrecompiles,
    PrecompileStatus, PrecompileStorageProvider, Result, SstoreValues, StorageFeatures,
};
use evm2::{
    ErrorCode, Evm, EvmFeatures, PrecompileError, PrecompileHalt, Precompiles,
    evm::StateCheckpoint,
    interpreter::{GasTracker, Message, MessageKind},
    precompile::{PrecompileOutput, PrecompileProvider},
    precompiles::PrecompileId,
};

use crate::BaseEvmTypes;

/// Precompile provider for Base: the Ethereum set plus the Base native precompiles.
#[derive(Clone, Debug)]
pub struct BasePrecompiles {
    /// The Ethereum precompiles with the Base input caps and P256VERIFY.
    pub eth: Precompiles<BaseEvmTypes>,
    /// The Base native precompiles of the fork.
    pub native: NativePrecompiles,
    /// Chain id the native precompiles report (EIP-712 domains), base-reth's `cfg.chain_id`.
    pub chain_id: u64,
}

impl BasePrecompiles {
    /// Runs a native precompile call and applies its gas to `gas`.
    fn execute_native(
        &self,
        evm: &mut Evm<'_, BaseEvmTypes>,
        message: &Message<BaseEvmTypes>,
        gas: &mut GasTracker,
        precompile: NativePrecompile,
    ) -> core::result::Result<PrecompileOutput, PrecompileError> {
        let address = message.code_address;
        // DELEGATECALL and CALLCODE run the precompile's code in another account.
        let is_direct_call = message.destination == address;
        let is_static = message.caller_is_static || message.kind == MessageKind::StaticCall;
        let gas_limit = message.gas_limit;
        assert_eq!(gas.remaining(), gas_limit, "precompile frames start with their full gas");

        let mut provider = EvmStorageProvider::new(
            evm,
            CallEnv {
                caller: message.caller,
                call_value: message.value,
                is_static,
                gas_limit,
                chain_id: self.chain_id,
                storage_features: self.native.storage_features(),
            },
        );
        let result =
            self.native.run(precompile, address, &message.input, is_direct_call, &mut provider);
        assert!(provider.checkpoints.is_empty(), "native precompile left a checkpoint open");

        let output = match result {
            Ok(output) => output,
            Err(base_tiger_precompiles::PrecompileError::Fatal(message)) => {
                return Err(PrecompileError::Fatal(message.into()));
            }
        };
        // revm downgrades a precompile that reports more gas than it was given to an OOG halt.
        if output.gas_used > gas_limit {
            return Err(PrecompileError::Halt(PrecompileHalt::OutOfGas));
        }
        match output.status {
            PrecompileStatus::Success => {
                gas.spend(output.gas_used).expect("gas used is within the limit");
                gas.record_refund(output.gas_refunded);
                Ok(PrecompileOutput::new(output.bytes))
            }
            PrecompileStatus::Revert => {
                gas.spend(output.gas_used).expect("gas used is within the limit");
                Err(PrecompileError::Revert(output.bytes))
            }
            PrecompileStatus::Halt => Err(PrecompileError::Halt(PrecompileHalt::OutOfGas)),
        }
    }
}

impl PrecompileProvider<BaseEvmTypes> for BasePrecompiles {
    fn addresses(&self) -> Vec<Address> {
        let mut addresses = self.eth.addresses();
        addresses.extend(self.native.fixed().into_iter().map(|(address, _)| address));
        addresses
    }

    fn precompile_ids(&self) -> Vec<(Address, PrecompileId)> {
        let mut ids = self.eth.precompile_ids();
        ids.extend(self.native.fixed().into_iter().map(|(address, precompile)| {
            let name = match precompile {
                NativePrecompile::ActivationRegistry => "ActivationRegistry",
                NativePrecompile::B20Factory => "B20Factory",
                NativePrecompile::PolicyRegistry => "PolicyRegistry",
                NativePrecompile::TxContext => "TxContext",
                NativePrecompile::NonceManager => "NonceManager",
                NativePrecompile::B20Asset | NativePrecompile::B20Stablecoin => {
                    unreachable!("B-20 tokens have no fixed address")
                }
            };
            (address, PrecompileId::custom(name))
        }));
        ids
    }

    fn contains(&self, address: &Address) -> bool {
        self.eth.contains(address) || self.native.lookup(*address).is_some()
    }

    fn execute(
        &mut self,
        evm: &mut Evm<'_, BaseEvmTypes>,
        message: &Message<BaseEvmTypes>,
        gas: &mut GasTracker,
    ) -> Option<core::result::Result<PrecompileOutput, PrecompileError>> {
        if let Some(precompile) = self.native.lookup(message.code_address) {
            return Some(self.execute_native(evm, message, gas, precompile));
        }
        self.eth.execute(evm, message, gas)
    }
}

/// Per-call inputs of [`EvmStorageProvider`] that do not come from the EVM.
#[derive(Clone, Copy, Debug)]
pub struct CallEnv {
    /// The precompile's caller.
    pub caller: Address,
    /// Value attached to the call.
    pub call_value: U256,
    /// Whether the call frame is static.
    pub is_static: bool,
    /// Gas the call was given.
    pub gas_limit: u64,
    /// Chain id reported to the precompile.
    pub chain_id: u64,
    /// Storage features of the fork.
    pub storage_features: StorageFeatures,
}

/// [`PrecompileStorageProvider`] over a live evm2 [`Evm`] for one precompile call.
#[derive(Debug)]
pub struct EvmStorageProvider<'e, 'a> {
    evm: &'e mut Evm<'a, BaseEvmTypes>,
    env: CallEnv,
    gas_remaining: u64,
    gas_refunded: i64,
    /// Open checkpoints, innermost last.
    checkpoints: Vec<StateCheckpoint>,
}

impl<'e, 'a> EvmStorageProvider<'e, 'a> {
    /// Most nested checkpoints a native precompile call opens.
    const CHECKPOINT_DEPTH_MAX: usize = 64;

    /// Creates a provider for one call with `env.gas_limit` gas.
    pub const fn new(evm: &'e mut Evm<'a, BaseEvmTypes>, env: CallEnv) -> Self {
        Self {
            evm,
            env,
            gas_remaining: env.gas_limit,
            gas_refunded: 0,
            checkpoints: Vec::new(),
        }
    }

    fn fatal(code: ErrorCode) -> BasePrecompileError {
        BasePrecompileError::Fatal(format!("database error: {code:?}"))
    }

    fn rollback_features(&self) -> EvmFeatures {
        if self.evm.feature(EvmFeatures::EIP161) {
            EvmFeatures::EIP161
        } else {
            EvmFeatures::empty()
        }
    }

    /// Loads and warms `address`, returning whether it was cold, as revm's `load_account`.
    fn warm_account(&mut self, address: Address, touch: bool) -> Result<bool> {
        let mut account = self.evm.state_mut().account(&address, false).map_err(Self::fatal)?;
        let was_cold = account.warm();
        if touch {
            account.touch();
        }
        Ok(was_cold)
    }

    /// Runs `f` under a state checkpoint that is rolled back if `f` fails.
    fn atomic<T>(&mut self, f: impl FnOnce(&mut Self) -> Result<T>) -> Result<T> {
        let checkpoint = self.evm.state().checkpoint();
        let result = f(self);
        if result.is_err() {
            let features = self.rollback_features();
            self.evm.state_mut().rollback(checkpoint, features);
        }
        result
    }
}

impl PrecompileStorageProvider for EvmStorageProvider<'_, '_> {
    fn chain_id(&self) -> u64 {
        self.env.chain_id
    }

    fn timestamp(&self) -> U256 {
        self.evm.block().timestamp
    }

    fn origin(&self) -> Address {
        self.evm.ext().tx_origin
    }

    fn set_code(&mut self, address: Address, code: Bytes) -> Result<()> {
        if self.env.is_static {
            return Err(BasePrecompileError::StaticCallViolation);
        }
        let code_len = code.len();
        // Yellow Paper G_codedeposit per byte of deployed code.
        self.deduct_gas(GasSchedule::CODE_DEPOSIT.saturating_mul(code_len as u64))?;
        // Writing code to an account without code also pays G_create and the code hash, even
        // when the account is prefunded.
        self.warm_account(address, false)?;
        let is_new_code = {
            let account = self.evm.state_mut().account(&address, false).map_err(Self::fatal)?;
            account.code_hash() == KECCAK256_EMPTY
        };
        if is_new_code {
            self.deduct_gas(GasSchedule::CREATE)?;
            self.deduct_gas(GasSchedule::keccak256(code_len))?;
        }
        let code = evm2::bytecode::Bytecode::new_legacy(code);
        self.evm.state_mut().account(&address, false).map_err(Self::fatal)?.set_code_slow(code);
        Ok(())
    }

    fn with_account_info(
        &mut self,
        address: Address,
        f: &mut dyn FnMut(&AccountInfo),
    ) -> Result<()> {
        let is_cold = self.warm_account(address, false)?;
        let info = {
            let account = self.evm.state_mut().account(&address, false).map_err(Self::fatal)?;
            AccountInfo {
                balance: account.balance(),
                nonce: account.nonce(),
                code_hash: account.code_hash(),
            }
        };
        // EIP-2929: warm access cost, plus the cold surcharge.
        self.deduct_gas(GasSchedule::WARM_STORAGE_READ)?;
        if is_cold {
            self.deduct_gas(GasSchedule::COLD_ACCOUNT_ADDITIONAL)?;
        }
        f(&info);
        Ok(())
    }

    fn sload(&mut self, address: Address, key: U256) -> Result<U256> {
        self.atomic(|this| {
            this.warm_account(address, false)?;
            let (value, is_cold) = {
                let mut slot =
                    this.evm.state_mut().storage_slot(&address, key, false).map_err(Self::fatal)?;
                let is_cold = slot.warm();
                (slot.current(), is_cold)
            };
            this.deduct_gas(GasSchedule::WARM_STORAGE_READ)?;
            if is_cold {
                this.deduct_gas(GasSchedule::COLD_STORAGE_ADDITIONAL)?;
            }
            Ok(value)
        })
    }

    fn tload_unmetered(&mut self, address: Address, key: U256) -> Result<U256> {
        Ok(self.evm.state_mut().tload(&address, &key))
    }

    fn sstore(&mut self, address: Address, key: U256, value: U256) -> Result<()> {
        if self.env.is_static {
            return Err(BasePrecompileError::StaticCallViolation);
        }
        // EIP-2200 reentrancy sentry: SSTORE cannot run on the call stipend.
        if self.gas_remaining <= GasSchedule::CALL_STIPEND {
            return Err(BasePrecompileError::OutOfGas);
        }
        self.atomic(|this| {
            this.warm_account(address, true)?;
            let (values, is_cold) = {
                let mut slot =
                    this.evm.state_mut().storage_slot(&address, key, false).map_err(Self::fatal)?;
                let is_cold = slot.warm();
                let (original, present) = slot.write(value);
                (SstoreValues { original, present, new: value }, is_cold)
            };
            this.deduct_gas(GasSchedule::SSTORE_STATIC)?;
            this.deduct_gas(GasSchedule::sstore_dynamic(values, is_cold))?;
            this.gas_refunded += GasSchedule::sstore_refund(values);
            Ok(())
        })
    }

    fn tstore(&mut self, address: Address, key: U256, value: U256) -> Result<()> {
        if self.env.is_static {
            return Err(BasePrecompileError::StaticCallViolation);
        }
        self.deduct_gas(GasSchedule::WARM_STORAGE_READ)?;
        self.evm.state_mut().tstore(&address, &key, &value);
        Ok(())
    }

    fn emit_event(&mut self, address: Address, event: LogData) -> Result<()> {
        if self.env.is_static {
            return Err(BasePrecompileError::StaticCallViolation);
        }
        let cost = GasSchedule::log(event.topics().len() as u64, event.data.len() as u64);
        self.deduct_gas(cost)?;
        self.evm.state_mut().log(Log { address, data: event });
        Ok(())
    }

    fn deduct_gas(&mut self, gas: u64) -> Result<()> {
        let Some(remaining) = self.gas_remaining.checked_sub(gas) else {
            return Err(BasePrecompileError::OutOfGas);
        };
        self.gas_remaining = remaining;
        Ok(())
    }

    fn gas_used(&self) -> u64 {
        self.env.gas_limit - self.gas_remaining
    }

    fn gas_refunded(&self) -> i64 {
        self.gas_refunded
    }

    fn storage_features(&self) -> StorageFeatures {
        self.env.storage_features
    }

    fn is_static(&self) -> bool {
        self.env.is_static
    }

    fn call_value(&self) -> U256 {
        self.env.call_value
    }

    fn caller(&self) -> Address {
        self.env.caller
    }

    fn replace_caller(&mut self, caller: Address) -> Address {
        core::mem::replace(&mut self.env.caller, caller)
    }

    fn checkpoint(&mut self) -> Checkpoint {
        assert!(self.checkpoints.len() < Self::CHECKPOINT_DEPTH_MAX);
        let checkpoint = self.evm.state().checkpoint();
        let token =
            Checkpoint { journal_len: checkpoint.journal_len(), logs_len: checkpoint.logs_len() };
        self.checkpoints.push(checkpoint);
        token
    }

    fn commit_latest_checkpoint(&mut self) {
        // evm2 checkpoints are journal positions: committing keeps the entries and only closes
        // the scope.
        assert!(self.checkpoints.pop().is_some(), "commit without an open checkpoint");
    }

    fn checkpoint_revert(&mut self, checkpoint: Checkpoint) {
        let latest = self.checkpoints.pop().expect("revert without an open checkpoint");
        assert_eq!(
            (latest.journal_len(), latest.logs_len()),
            (checkpoint.journal_len, checkpoint.logs_len),
            "checkpoints revert innermost first"
        );
        let features = self.rollback_features();
        self.evm.state_mut().rollback(latest, features);
    }

    fn metered_keccak256(&mut self, data: &[u8]) -> Result<B256> {
        self.deduct_gas(GasSchedule::keccak256(data.len()))?;
        Ok(alloy_primitives::keccak256(data))
    }
}

#[cfg(test)]
mod tests {
    use alloy_primitives::address;
    use alloy_sol_types::SolCall;
    use base_common_genesis::BaseUpgrade;
    use base_tiger_precompiles::{ITransactionContext, StorageCtx, TxContextStorage};
    use evm2::{env::BlockEnv, evm::InMemoryDB};

    use super::*;
    use crate::{BaseSpecId, ChainSpec};

    const ACCOUNT: Address = address!("0x0000000000000000000000000000000000000042");

    fn evm(upgrade: BaseUpgrade) -> Evm<'static, BaseEvmTypes> {
        let spec = BaseSpecId::new(upgrade);
        Evm::new(
            spec,
            BlockEnv::<BaseEvmTypes>::default(),
            BaseEvmTypes::tx_registry(spec),
            InMemoryDB::default(),
            BaseEvmTypes::precompiles(spec, &ChainSpec::base_mainnet()),
        )
    }

    fn env(gas_limit: u64, is_static: bool, storage_features: StorageFeatures) -> CallEnv {
        CallEnv {
            caller: Address::ZERO,
            call_value: U256::ZERO,
            is_static,
            gas_limit,
            chain_id: 8453,
            storage_features,
        }
    }

    #[test]
    fn sstore_needs_more_than_the_call_stipend() {
        let mut evm = evm(BaseUpgrade::Cobalt);
        for gas in [GasSchedule::CALL_STIPEND - 1, GasSchedule::CALL_STIPEND] {
            let mut provider =
                EvmStorageProvider::new(&mut evm, env(gas, false, StorageFeatures::Legacy));
            assert_eq!(
                provider.sstore(ACCOUNT, U256::ZERO, U256::from(1)),
                Err(BasePrecompileError::OutOfGas)
            );
        }
        let mut provider = EvmStorageProvider::new(
            &mut evm,
            env(GasSchedule::CALL_STIPEND + 1_000_000, false, StorageFeatures::Legacy),
        );
        provider.sstore(ACCOUNT, U256::ZERO, U256::from(1)).unwrap();
        // Cold slot, zero to nonzero: 20_000 + 2_100.
        assert_eq!(provider.gas_used(), 22_100);
    }

    #[test]
    fn static_violation_is_checked_before_the_stipend() {
        let mut evm = evm(BaseUpgrade::Cobalt);
        let mut provider = EvmStorageProvider::new(
            &mut evm,
            env(GasSchedule::CALL_STIPEND, true, StorageFeatures::Legacy),
        );
        assert_eq!(
            provider.sstore(ACCOUNT, U256::ZERO, U256::from(1)),
            Err(BasePrecompileError::StaticCallViolation)
        );
        assert_eq!(
            provider.set_code(ACCOUNT, Bytes::from_static(&[0x60, 0x00])),
            Err(BasePrecompileError::StaticCallViolation)
        );
        assert_eq!(provider.gas_used(), 0);
    }

    #[test]
    fn failed_storage_access_leaves_the_slot_cold() {
        let cold_read = GasSchedule::WARM_STORAGE_READ + GasSchedule::COLD_STORAGE_ADDITIONAL;
        let mut evm = evm(BaseUpgrade::Cobalt);
        let key = U256::from(7);
        let sstore_gas = GasSchedule::CALL_STIPEND + GasSchedule::SSTORE_STATIC + 1;
        let mut provider =
            EvmStorageProvider::new(&mut evm, env(sstore_gas, false, StorageFeatures::Legacy));
        assert_eq!(
            provider.sstore(ACCOUNT, key, U256::from(99)),
            Err(BasePrecompileError::OutOfGas)
        );
        let mut provider = EvmStorageProvider::new(
            &mut evm,
            env(GasSchedule::WARM_STORAGE_READ - 1, false, StorageFeatures::Legacy),
        );
        assert_eq!(provider.sload(ACCOUNT, key), Err(BasePrecompileError::OutOfGas));

        let mut provider =
            EvmStorageProvider::new(&mut evm, env(1_000_000, false, StorageFeatures::Legacy));
        assert_eq!(provider.sload(ACCOUNT, key).unwrap(), U256::ZERO, "the failed write is undone");
        assert_eq!(provider.gas_used(), cold_read, "the failed accesses did not warm the slot");
    }

    #[test]
    fn cobalt_bytes_shrink_records_a_refund() {
        let mut evm = evm(BaseUpgrade::Cobalt);
        let mut provider =
            EvmStorageProvider::new(&mut evm, env(1_000_000, false, StorageFeatures::Cobalt));
        StorageCtx::enter(&mut provider, |storage| {
            let slot = U256::from(10);
            storage.write_string(ACCOUNT, slot, &"\u{11}".repeat(64)).unwrap();
            assert_eq!(storage.gas_refunded(), 0);
            storage.write_string(ACCOUNT, slot, "short").unwrap();
            assert!(storage.gas_refunded() > 0, "clearing long-data slots records a refund");
        });
    }

    #[test]
    fn unmetered_tload_charges_nothing() {
        let mut evm = evm(BaseUpgrade::Cobalt);
        let mut provider =
            EvmStorageProvider::new(&mut evm, env(1_000_000, false, StorageFeatures::Legacy));
        assert_eq!(provider.tload_unmetered(ACCOUNT, U256::ZERO).unwrap(), U256::ZERO);
        assert_eq!(provider.gas_used(), 0);
    }

    #[test]
    fn set_code_charges_new_code_on_prefunded_accounts() {
        let code = Bytes::from_static(&[0x60, 0x00]);
        let mut gas_used = Vec::new();
        for balance in [U256::ZERO, U256::from(1)] {
            let mut evm = evm(BaseUpgrade::Cobalt);
            evm.state_mut().account(&ACCOUNT, false).unwrap().set_balance(balance);
            let mut provider =
                EvmStorageProvider::new(&mut evm, env(1_000_000, false, StorageFeatures::Legacy));
            provider.set_code(ACCOUNT, code.clone()).unwrap();
            gas_used.push(provider.gas_used());
            let mut code_hash = B256::ZERO;
            provider.with_account_info(ACCOUNT, &mut |info| code_hash = info.code_hash).unwrap();
            assert_eq!(code_hash, alloy_primitives::keccak256(&code));
        }
        // 2 bytes * 200 + CREATE 32_000 + keccak of one word 36.
        assert_eq!(gas_used, [32_436, 32_436]);
    }

    fn message(destination: Address, input: Bytes, gas_limit: u64) -> Message<BaseEvmTypes> {
        Message::<BaseEvmTypes> {
            kind: MessageKind::Call,
            gas_limit,
            destination,
            code_address: TxContextStorage::ADDRESS,
            caller: ACCOUNT,
            input,
            ..Default::default()
        }
    }

    #[test]
    fn executes_native_precompiles_by_fork() {
        let input = Bytes::from(ITransactionContext::getTransactionSenderCall {}.abi_encode());
        let origin = address!("0x00000000000000000000000000000000000000aa");

        let mut evm = evm(BaseUpgrade::Cobalt);
        evm.ext_mut().tx_origin = origin;
        let mut precompiles = BaseEvmTypes::precompiles(
            BaseSpecId::new(BaseUpgrade::Cobalt),
            &ChainSpec::base_mainnet(),
        );
        assert!(precompiles.addresses().contains(&TxContextStorage::ADDRESS));

        let direct = message(TxContextStorage::ADDRESS, input.clone(), 100_000);
        let mut gas = GasTracker::new(direct.gas_limit);
        let output = precompiles.execute(&mut evm, &direct, &mut gas).unwrap().unwrap();
        assert_eq!(&output.bytes()[12..], origin.as_slice());
        assert!(gas.remaining() < direct.gas_limit);

        // DELEGATECALL reverts without spending gas.
        let delegated = message(ACCOUNT, input, 100_000);
        let mut gas = GasTracker::new(delegated.gas_limit);
        let result = precompiles.execute(&mut evm, &delegated, &mut gas).unwrap();
        assert!(matches!(result, Err(PrecompileError::Revert(_))));
        assert_eq!(gas.remaining(), delegated.gas_limit);

        let precompiles = BaseEvmTypes::precompiles(
            BaseSpecId::new(BaseUpgrade::Beryl),
            &ChainSpec::base_mainnet(),
        );
        assert!(!precompiles.contains(&TxContextStorage::ADDRESS));
    }
}
