//! Production EVM-backed [`PrecompileStorageProvider`].
//!
//! [`EvmPrecompileStorageProvider`] wraps an alloy-evm [`PrecompileInput`] and implements
//! [`PrecompileStorageProvider`] by delegating to the live [`EvmInternals`] journal.
//! It is constructed inside each native precompile's `run()` function and passed to
//! [`StorageCtx::enter`] so that `#[contract]`-generated storage types read/write real EVM state.

use alloc::string::ToString;

use alloy_evm::precompiles::PrecompileInput;
use alloy_primitives::{Address, B256, Log, LogData, U256};
use revm::{
    context::{Block, journaled_state::JournalCheckpoint},
    context_interface::cfg::GasParams,
    interpreter::gas::{Gas, KECCAK256, KECCAK256WORD, LOG},
    primitives::keccak256,
    state::{AccountInfo, Bytecode},
};

use crate::{
    error::{BasePrecompileError, Result},
    provider::PrecompileStorageProvider,
};

/// Production [`PrecompileStorageProvider`] backed by a live EVM journal.
///
/// Constructed from a [`PrecompileInput`] inside each native precompile's `run()` function.
/// Pass `&mut self` to [`StorageCtx::enter`] to give `#[contract]` storage types access to
/// the real EVM journal.
#[derive(Debug)]
pub struct EvmPrecompileStorageProvider<'a> {
    internals: alloy_evm::EvmInternals<'a>,
    caller: Address,
    call_value: U256,
    gas: Gas,
    gas_params: GasParams,
    is_static: bool,
    block_number: u64,
    timestamp: U256,
    chain_id: u64,
    beneficiary: Address,
    state_gas_used: u64,
}

impl<'a> EvmPrecompileStorageProvider<'a> {
    /// Consume a [`PrecompileInput`] and build the provider.
    ///
    /// `gas_params` drives all EIP-2929/2200/3529 cost calculations.
    /// Pass [`GasParams::default`] when the active spec is unknown at call site.
    pub fn new(input: PrecompileInput<'a>, gas_params: GasParams) -> Self {
        let PrecompileInput { gas, caller, value, is_static, internals, .. } = input;

        let block_number = internals.block_env().number().to::<u64>();
        let timestamp = internals.block_env().timestamp();
        let chain_id = internals.chain_id();
        let beneficiary = internals.block_env().beneficiary();

        Self {
            internals,
            caller,
            call_value: value,
            gas: Gas::new(gas),
            gas_params,
            is_static,
            block_number,
            timestamp,
            chain_id,
            beneficiary,
            state_gas_used: 0,
        }
    }
}

impl PrecompileStorageProvider for EvmPrecompileStorageProvider<'_> {
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

    fn set_code(&mut self, address: Address, code: Bytecode) -> Result<()> {
        if self.is_static {
            return Err(BasePrecompileError::StaticCallViolation);
        }

        let code_len = code.len();

        // Yellow Paper G_codedeposit: 200 gas per byte of deployed bytecode.
        self.deduct_gas(self.gas_params.code_deposit_cost(code_len))?;

        let (is_new_account, has_empty_code) = {
            let state_load = self
                .internals
                .load_account(address)
                .map_err(|e| BasePrecompileError::Fatal(e.to_string()))?;
            let info = &state_load.data.info;
            (info.is_empty(), info.is_empty_code_hash())
        };

        if has_empty_code {
            // Yellow Paper G_create: base cost for creating a new contract account.
            self.deduct_gas(self.gas_params.create_cost())?;
            // Yellow Paper G_sha3 + G_sha3word: cost of computing the stored code hash.
            let num_words = code_len.div_ceil(32) as u64;
            self.deduct_gas(KECCAK256.saturating_add(KECCAK256WORD.saturating_mul(num_words)))?;
        }
        if is_new_account {
            // EIP-8037: charge for the new account entry in the state trie.
            self.deduct_state_gas(self.gas_params.create_state_gas())?;
        }
        if has_empty_code {
            // EIP-8037: charge for depositing code into an account whose code slot is
            // currently empty. Applies to both new accounts and existent accounts that
            // have only a nonzero balance or nonce (no prior code).
            self.deduct_state_gas(self.gas_params.code_deposit_state_gas(code_len))?;
        }

        self.internals
            .set_code(address, code)
            .map_err(|e| BasePrecompileError::Fatal(e.to_string()))
    }

    fn with_account_info(
        &mut self,
        address: Address,
        f: &mut dyn FnMut(&AccountInfo),
    ) -> Result<()> {
        // Extract is_cold and clone AccountInfo before releasing the internals borrow.
        let (info, is_cold) = {
            let state_load = self
                .internals
                .load_account(address)
                .map_err(|e| BasePrecompileError::Fatal(e.to_string()))?;
            (state_load.data.info.clone(), state_load.is_cold)
        };

        // EIP-2929: warm base cost always charged (100)
        self.deduct_gas(self.gas_params.warm_storage_read_cost())?;
        // dynamic cold penalty — total 2600 for a cold account access
        if is_cold {
            self.deduct_gas(self.gas_params.cold_account_additional_cost())?;
        }

        f(&info);
        Ok(())
    }

    fn sload(&mut self, address: Address, key: U256) -> Result<U256> {
        let checkpoint = self.internals.checkpoint();
        let result = (|| {
            let s = self
                .internals
                .sload(address, key)
                .map_err(|e| BasePrecompileError::Fatal(e.to_string()))?;

            // EIP-2929: warm base cost always charged
            self.deduct_gas(self.gas_params.warm_storage_read_cost())?;
            // dynamic cold penalty
            if s.is_cold {
                self.deduct_gas(self.gas_params.cold_storage_additional_cost())?;
            }

            Ok(s.data)
        })();

        if result.is_ok() {
            self.internals.checkpoint_commit();
        } else {
            self.internals.checkpoint_revert(checkpoint);
        }

        result
    }

    fn tload(&mut self, address: Address, key: U256) -> Result<U256> {
        self.deduct_gas(self.gas_params.warm_storage_read_cost())?;
        Ok(self.internals.tload(address, key))
    }

    fn sstore(&mut self, address: Address, key: U256, value: U256) -> Result<()> {
        if self.is_static {
            return Err(BasePrecompileError::StaticCallViolation);
        }
        // EIP-2200: if remaining gas is at or below the call stipend (2300), halt with
        // out-of-gas. This is the reentrancy sentry that Solidity's `.transfer()` relies on:
        // forwarding only 2300 gas guarantees the recipient cannot perform state-changing
        // SSTOREs. Without this guard, a warm-dirty rewrite (~200 gas) would succeed where
        // the EVM SSTORE opcode would have halted, breaking the 2300-gas invariant.
        if self.gas.remaining() <= self.gas_params.call_stipend() {
            return Err(BasePrecompileError::OutOfGas);
        }
        let checkpoint = self.internals.checkpoint();
        let result = (|| {
            let s = self
                .internals
                .sstore(address, key, value)
                .map_err(|e| BasePrecompileError::Fatal(e.to_string()))?;

            // EIP-2929: static warm base cost
            self.deduct_gas(self.gas_params.sstore_static_gas())?;
            // EIP-2929 + EIP-2200: dynamic cost (cold penalty + net-metering)
            self.deduct_gas(self.gas_params.sstore_dynamic_gas(true, &s.data, s.is_cold))?;
            // EIP-3529: net-metering refund
            self.refund_gas(self.gas_params.sstore_refund(true, &s.data));

            Ok(())
        })();

        if result.is_ok() {
            self.internals.checkpoint_commit();
        } else {
            self.internals.checkpoint_revert(checkpoint);
        }

        result
    }

    fn tstore(&mut self, address: Address, key: U256, value: U256) -> Result<()> {
        if self.is_static {
            return Err(BasePrecompileError::StaticCallViolation);
        }
        self.deduct_gas(self.gas_params.warm_storage_read_cost())?;
        self.internals.tstore(address, key, value);
        Ok(())
    }

    fn emit_event(&mut self, address: Address, event: LogData) -> Result<()> {
        if self.is_static {
            return Err(BasePrecompileError::StaticCallViolation);
        }
        let cost =
            LOG + self.gas_params.log_cost(event.topics().len() as u8, event.data.len() as u64);
        self.deduct_gas(cost)?;
        self.internals.log(Log { address, data: event });
        Ok(())
    }

    fn deduct_gas(&mut self, gas: u64) -> Result<()> {
        if !self.gas.record_regular_cost(gas) {
            return Err(BasePrecompileError::OutOfGas);
        }
        Ok(())
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
        self.gas.total_gas_spent()
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

    fn checkpoint(&mut self) -> JournalCheckpoint {
        self.internals.checkpoint()
    }

    fn checkpoint_commit(&mut self) {
        self.internals.checkpoint_commit();
    }

    fn checkpoint_revert(&mut self, checkpoint: JournalCheckpoint) {
        self.internals.checkpoint_revert(checkpoint);
    }

    fn metered_keccak256(&mut self, data: &[u8]) -> Result<B256> {
        let num_words =
            u64::try_from(data.len().div_ceil(32)).map_err(|_| BasePrecompileError::OutOfGas)?;
        let price = KECCAK256WORD
            .checked_mul(num_words)
            .and_then(|w| w.checked_add(KECCAK256))
            .ok_or(BasePrecompileError::OutOfGas)?;
        self.deduct_gas(price)?;
        Ok(keccak256(data))
    }
}

impl From<alloy_evm::EvmInternalsError> for BasePrecompileError {
    fn from(e: alloy_evm::EvmInternalsError) -> Self {
        Self::Fatal(e.to_string())
    }
}

#[cfg(test)]
mod tests {
    use alloy_evm::{EvmInternals, eth::EthEvmContext, precompiles::PrecompileInput};
    use alloy_primitives::{Address, U256};
    use revm::{
        context_interface::cfg::GasParams, database::EmptyDB, primitives::hardfork::SpecId,
        state::Bytecode,
    };

    use crate::{
        BasePrecompileError, hashmap::HashMapStorageProvider, provider::PrecompileStorageProvider,
    };

    fn amsterdam_provider() -> HashMapStorageProvider {
        let mut provider = HashMapStorageProvider::new(1);
        provider.set_gas_params(GasParams::new_spec(SpecId::AMSTERDAM));
        provider
    }

    /// The EIP-2200 stipend guard in [`super::EvmPrecompileStorageProvider::sstore`] compares
    /// `gas.remaining()` against `gas_params.call_stipend()`. This test verifies that the
    /// call stipend constant returned by `GasParams` is exactly 2300, as required by EIP-2200.
    ///
    /// Unit tests cannot directly instantiate [`super::EvmPrecompileStorageProvider`] because
    /// it requires a live EVM journal via `PrecompileInput`. The stipend guard is therefore not
    /// exercisable in isolation here. Full coverage of the guard at runtime is provided by the
    /// B20 fork tests that forward exactly 2300 gas into a stateful precompile call.
    #[test]
    fn eip_2200_stipend_guard_constant_is_2300() {
        let gas_params = GasParams::new_spec(SpecId::AMSTERDAM);
        assert_eq!(
            gas_params.call_stipend(),
            2300,
            "call_stipend must equal 2300 as required by EIP-2200"
        );
    }

    #[test]
    fn sstore_oog_reverts_local_journal_mutation() {
        let gas_params = GasParams::new_spec(SpecId::AMSTERDAM);
        let mut ctx = EthEvmContext::new(EmptyDB::default(), SpecId::AMSTERDAM);
        let address = Address::repeat_byte(0x42);
        let key = U256::from(7);
        let value = U256::from(99);

        {
            let input = PrecompileInput {
                data: &[],
                gas: gas_params
                    .call_stipend()
                    .saturating_add(gas_params.sstore_static_gas())
                    .saturating_add(1),
                reservoir: 0,
                caller: Address::ZERO,
                value: U256::ZERO,
                target_address: address,
                is_static: false,
                bytecode_address: address,
                internals: EvmInternals::from_context(&mut ctx),
            };
            let mut provider = super::EvmPrecompileStorageProvider::new(input, gas_params.clone());

            let err = provider.sstore(address, key, value).unwrap_err();

            assert_eq!(err, BasePrecompileError::OutOfGas);
        }

        {
            let input = PrecompileInput {
                data: &[],
                gas: u64::MAX,
                reservoir: 0,
                caller: Address::ZERO,
                value: U256::ZERO,
                target_address: address,
                is_static: false,
                bytecode_address: address,
                internals: EvmInternals::from_context(&mut ctx),
            };
            let mut provider = super::EvmPrecompileStorageProvider::new(input, gas_params.clone());

            assert_eq!(provider.sload(address, key).unwrap(), U256::ZERO);
            assert_eq!(
                provider.gas_used(),
                gas_params
                    .warm_storage_read_cost()
                    .saturating_add(gas_params.cold_storage_additional_cost())
            );
        }
    }

    /// An OOG `sload` must not leave the slot warmed in the journal.
    ///
    /// We give the provider exactly `warm_storage_read_cost - 1` gas so that the
    /// cold read fails (it cannot even afford the warm base cost). A second provider
    /// with unlimited gas then reads the same slot: if the journal still carries the
    /// spurious warm entry the second read would be charged only `warm_storage_read_cost`,
    /// but the slot was never successfully accessed so it must still be cold.
    #[test]
    fn sload_oog_does_not_warm_slot() {
        let gas_params = GasParams::new_spec(SpecId::AMSTERDAM);
        let address = Address::repeat_byte(0x77);
        let key = U256::from(5);

        let mut ctx = EthEvmContext::new(EmptyDB::default(), SpecId::AMSTERDAM);

        // First provider: gas just below warm_storage_read_cost → OOG on sload.
        {
            let input = PrecompileInput {
                data: &[],
                gas: gas_params.warm_storage_read_cost() - 1,
                reservoir: 0,
                caller: Address::ZERO,
                value: U256::ZERO,
                target_address: address,
                is_static: false,
                bytecode_address: address,
                internals: EvmInternals::from_context(&mut ctx),
            };
            let mut provider = super::EvmPrecompileStorageProvider::new(input, gas_params.clone());
            assert_eq!(provider.sload(address, key), Err(BasePrecompileError::OutOfGas));
        }

        // Second provider: unlimited gas. The slot must still be cold, so the full
        // cold read cost (warm_base + cold_additional) must be charged.
        {
            let input = PrecompileInput {
                data: &[],
                gas: u64::MAX,
                reservoir: 0,
                caller: Address::ZERO,
                value: U256::ZERO,
                target_address: address,
                is_static: false,
                bytecode_address: address,
                internals: EvmInternals::from_context(&mut ctx),
            };
            let mut provider = super::EvmPrecompileStorageProvider::new(input, gas_params.clone());
            assert_eq!(provider.sload(address, key).unwrap(), U256::ZERO);
            assert_eq!(
                provider.gas_used(),
                gas_params
                    .warm_storage_read_cost()
                    .saturating_add(gas_params.cold_storage_additional_cost()),
                "slot must still be cold after a failed OOG read"
            );
        }
    }

    /// `set_code` on a brand-new account must charge both `create_state_gas` and
    /// `code_deposit_state_gas` against the state-gas counter.
    #[test]
    fn set_code_new_account_charges_create_and_deposit_state_gas() {
        let mut provider = amsterdam_provider();
        let addr = Address::from([0x42u8; 20]);
        let code = Bytecode::new_raw([0x60u8, 0x00].as_ref().into());
        let code_len = code.len();
        let gas_params = GasParams::new_spec(SpecId::AMSTERDAM);

        provider.set_code(addr, code).unwrap();

        let expected = gas_params.create_state_gas() + gas_params.code_deposit_state_gas(code_len);
        assert!(expected > 0, "AMSTERDAM state gas must be non-zero");
        assert_eq!(provider.state_gas_used(), expected);
    }

    /// `set_code` on a balance-only account (nonzero balance, no code) must charge
    /// `code_deposit_state_gas` but not `create_state_gas`. This covers the attack where
    /// a caller prefunds a future token address before calling the factory.
    #[test]
    fn set_code_balance_only_account_charges_code_deposit_state_gas_only() {
        let mut provider = amsterdam_provider();
        let addr = Address::from([0x42u8; 20]);
        let code = Bytecode::new_raw([0x60u8, 0x00].as_ref().into());
        let code_len = code.len();
        let gas_params = GasParams::new_spec(SpecId::AMSTERDAM);

        // Simulate a prefunded account: has balance but no code.
        provider.set_balance(addr, U256::from(1u64));

        provider.set_code(addr, code).unwrap();

        let expected = gas_params.code_deposit_state_gas(code_len);
        assert_eq!(
            provider.state_gas_used(),
            expected,
            "balance-only account must pay code_deposit_state_gas but not create_state_gas"
        );
    }

    /// `set_code` on a prefunded account (balance > 0, no code) must charge the same regular gas
    /// as a fully empty account.
    #[test]
    fn set_code_prefunded_account_charges_same_gas_as_empty_account() {
        let addr = Address::from([0x43u8; 20]);
        let code = Bytecode::new_raw([0x60u8, 0x00].as_ref().into());

        let mut empty_provider = amsterdam_provider();
        empty_provider.set_code(addr, code.clone()).unwrap();
        let gas_for_empty = empty_provider.gas_deducted();

        let mut prefunded_provider = amsterdam_provider();
        prefunded_provider.set_balance(addr, U256::from(1u64));
        prefunded_provider.set_code(addr, code).unwrap();
        let gas_for_prefunded = prefunded_provider.gas_deducted();

        assert!(gas_for_empty > 0, "set_code must charge non-zero gas");
        assert_eq!(
            gas_for_empty, gas_for_prefunded,
            "prefunded account must pay identical regular gas to an empty account"
        );
    }

    /// `set_code` on an already-initialised account must NOT charge any additional
    /// state gas (the account and its metadata already exist in the trie).
    #[test]
    fn set_code_existing_account_skips_state_gas() {
        let mut provider = amsterdam_provider();
        let addr = Address::from([0x42u8; 20]);
        let code = Bytecode::new_raw([0x60u8, 0x00].as_ref().into());

        // First call creates the account and charges state gas.
        provider.set_code(addr, code.clone()).unwrap();
        let after_first = provider.state_gas_used();
        assert!(after_first > 0);

        // Second call updates an existing account; state gas must not increase.
        provider.set_code(addr, code).unwrap();
        assert_eq!(
            provider.state_gas_used(),
            after_first,
            "state_gas_used must not increase for an existing account"
        );
    }

    #[test]
    fn set_code_static_context_reverts_before_state_gas_or_code_mutation() {
        let mut provider = amsterdam_provider();
        provider.set_static(true);
        let addr = Address::from([0x42u8; 20]);
        let code = Bytecode::new_raw([0x60u8, 0x00].as_ref().into());

        let err = provider.set_code(addr, code).unwrap_err();

        assert_eq!(err, BasePrecompileError::StaticCallViolation);
        assert_eq!(provider.state_gas_used(), 0);
        assert!(provider.get_account_info(addr).is_none());
    }
}
