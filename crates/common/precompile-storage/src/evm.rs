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
        let PrecompileInput { gas, caller, is_static, internals, .. } = input;

        let block_number = internals.block_env().number().to::<u64>();
        let timestamp = internals.block_env().timestamp();
        let chain_id = internals.chain_id();
        let beneficiary = internals.block_env().beneficiary();

        Self {
            internals,
            caller,
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
        let code_len = code.len();

        // EIP-3541 / Yellow Paper G_codedeposit: 200 gas per byte of deployed bytecode.
        self.deduct_gas(self.gas_params.code_deposit_cost(code_len))?;

        // For new (empty) accounts charge the CREATE equivalent costs (Yellow Paper G_create).
        let is_new_account = {
            let state_load = self
                .internals
                .load_account(address)
                .map_err(|e| BasePrecompileError::Fatal(e.to_string()))?;
            state_load.data.info.is_empty()
        };

        if is_new_account {
            // Yellow Paper G_create: base cost for creating a new contract account.
            self.deduct_gas(self.gas_params.create_cost())?;
            // Yellow Paper G_sha3 + G_sha3word: cost of computing the stored code hash.
            let num_words = code_len.div_ceil(32) as u64;
            self.deduct_gas(KECCAK256.saturating_add(KECCAK256WORD.saturating_mul(num_words)))?;
            // EIP-8037: both state gas charges are gated on is_new_account.
            // create_state_gas covers the new account entry in the state trie.
            // code_deposit_state_gas covers the new code object. Replacing code on an
            // existing account is not a state-creating operation in the EIP-8037 model —
            // the code slot already occupies a trie node — so it is intentionally excluded.
            // In practice, precompile set_code is only called during factory token creation,
            // where the target address is always a fresh account.
            self.deduct_state_gas(self.gas_params.create_state_gas())?;
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
    }

    fn tload(&mut self, address: Address, key: U256) -> Result<U256> {
        self.deduct_gas(self.gas_params.warm_storage_read_cost())?;
        Ok(self.internals.tload(address, key))
    }

    fn sstore(&mut self, address: Address, key: U256, value: U256) -> Result<()> {
        if self.is_static {
            return Err(BasePrecompileError::StaticCallViolation);
        }
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

    fn caller(&self) -> Address {
        self.caller
    }

    fn checkpoint(&mut self) -> JournalCheckpoint {
        self.internals.checkpoint()
    }

    fn checkpoint_commit(&mut self, _checkpoint: JournalCheckpoint) {
        // alloy-evm's checkpoint_commit pops the top checkpoint; the arg is unused.
        self.internals.checkpoint_commit();
    }

    fn checkpoint_revert(&mut self, checkpoint: JournalCheckpoint) {
        self.internals.checkpoint_revert(checkpoint);
    }

    fn keccak256(&mut self, data: &[u8]) -> Result<B256> {
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
    use alloy_primitives::Address;
    use revm::{context_interface::cfg::GasParams, primitives::hardfork::SpecId, state::Bytecode};

    use crate::{hashmap::HashMapStorageProvider, provider::PrecompileStorageProvider};

    fn amsterdam_provider() -> HashMapStorageProvider {
        let mut provider = HashMapStorageProvider::new(1);
        provider.set_gas_params(GasParams::new_spec(SpecId::AMSTERDAM));
        provider
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
}
