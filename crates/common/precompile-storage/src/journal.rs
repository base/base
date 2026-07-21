//! Gas-free EVM-journal-backed [`PrecompileStorageProvider`].
//!
//! [`JournalStorageProvider`] wraps a live alloy-evm [`EvmInternals`] journal and
//! implements [`PrecompileStorageProvider`] without metering any gas. It exists
//! for the *enshrined* EIP-8130 execution path, which runs at block-executor
//! level (around, not inside, an EVM call) and meters its own gas via the
//! EIP-8130 intrinsic schedule. Using the metered [`EvmPrecompileStorageProvider`]
//! there would double-count storage gas.
//!
//! Reads and writes go through the same [`EvmInternals`] journal that EVM opcodes
//! use, so they observe state committed earlier in the block and persist to the
//! state the subsequent EVM calls and the final commit see.
//!
//! [`EvmPrecompileStorageProvider`]: crate::EvmPrecompileStorageProvider

use alloc::string::ToString;

use alloy_evm::EvmInternals;
use alloy_primitives::{Address, B256, Log, LogData, U256};
use revm::{
    // `Block` is imported only to bring its trait methods (e.g. `block_env().number()`)
    // into scope; `as _` keeps it anonymous since it is never named directly.
    context::{Block as _, journaled_state::JournalCheckpoint},
    primitives::keccak256,
    state::{AccountInfo, Bytecode},
};

use crate::{
    error::{BasePrecompileError, Result},
    provider::{PrecompileStorageProvider, validate_loaded_code_presence},
};

/// Gas-free [`PrecompileStorageProvider`] backed by a live EVM journal.
///
/// Construct from an [`EvmInternals`] borrowed from the block-execution context
/// (e.g. `EvmInternals::from_context(evm.ctx_mut())`) and pass `&mut self` to
/// [`StorageCtx::enter`](crate::StorageCtx::enter) so `#[contract]` storage types
/// read and write the real EVM journal. All gas accounting methods are no-ops:
/// [`gas_limit`](PrecompileStorageProvider::gas_limit) reports [`u64::MAX`] and
/// nothing is ever deducted.
#[derive(Debug)]
pub struct JournalStorageProvider<'a> {
    internals: EvmInternals<'a>,
    caller: Address,
    block_number: u64,
    timestamp: U256,
    chain_id: u64,
    beneficiary: Address,
    origin: Address,
}

impl<'a> JournalStorageProvider<'a> {
    /// Build a gas-free provider over `internals`, attributing storage access to
    /// `caller`. Block metadata (number, timestamp, chain id, beneficiary,
    /// origin) is snapshotted from the journal's block/transaction environment.
    pub fn new(internals: EvmInternals<'a>, caller: Address) -> Self {
        // Truncating to `u64` is safe: EVM block numbers are bounded far below
        // `u64::MAX` and `block_env().number()` is only `U256` for ABI uniformity.
        // (Unlike the block *timestamp*, which the EIP-8130 executor converts with
        // a checked `try_into` because it feeds consensus-critical expiry checks,
        // the block number here only backs the `block_number()` getter.)
        let block_number = internals.block_env().number().to::<u64>();
        let timestamp = internals.block_env().timestamp();
        let chain_id = internals.chain_id();
        let beneficiary = internals.block_env().beneficiary();
        let origin = internals.tx_origin();

        Self { internals, caller, block_number, timestamp, chain_id, beneficiary, origin }
    }
}

impl PrecompileStorageProvider for JournalStorageProvider<'_> {
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
        self.internals
            .set_code(address, code)
            .map_err(|e| BasePrecompileError::Fatal(e.to_string()))
    }

    fn with_account_info(
        &mut self,
        address: Address,
        f: &mut dyn FnMut(&AccountInfo),
    ) -> Result<()> {
        // Pass the journal's borrowed `AccountInfo` straight to the callback. The
        // callback cannot re-enter the provider (it has no `self` access), so the
        // outstanding borrow is safe and we avoid cloning the full `AccountInfo`
        // (including a potentially large `code` `Bytecode`) on every read.
        let state_load = self
            .internals
            .load_account(address)
            .map_err(|e| BasePrecompileError::Fatal(e.to_string()))?;
        f(&state_load.data.info);
        Ok(())
    }

    fn with_account_code(&mut self, address: Address, f: &mut dyn FnMut(&Bytecode)) -> Result<()> {
        // `load_account_code` resolves code from the database into the journal.
        // This provider deliberately charges no gas for that account access.
        let state_load = self
            .internals
            .load_account_code(address)
            .map_err(|e| BasePrecompileError::Fatal(e.to_string()))?;
        let expected_hash = *state_load.data.code_hash();
        let code = state_load.data.code().ok_or_else(|| {
            BasePrecompileError::Fatal(
                "account code unavailable after successful journal load".to_string(),
            )
        })?;
        validate_loaded_code_presence(expected_hash, code)?;
        f(code);
        Ok(())
    }

    fn sload(&mut self, address: Address, key: U256) -> Result<U256> {
        let s = self
            .internals
            .sload(address, key)
            .map_err(|e| BasePrecompileError::Fatal(e.to_string()))?;
        Ok(s.data)
    }

    fn tload(&mut self, address: Address, key: U256) -> Result<U256> {
        Ok(self.internals.tload(address, key))
    }

    fn sstore(&mut self, address: Address, key: U256, value: U256) -> Result<()> {
        // Writing only mutates the storage trie; it deliberately does not touch
        // nonce/balance/code. Code-less enshrined system accounts that hold
        // persistent storage (e.g. the 2D `NonceManager`) are instead made
        // EIP-161-non-empty out of band by a one-byte code stub planted at the
        // Cobalt transition (see `ensure_eip8130_system_accounts`), so their
        // storage survives end-of-block state clearing without a per-write guard.
        self.internals
            .sstore(address, key, value)
            .map_err(|e| BasePrecompileError::Fatal(e.to_string()))?;

        Ok(())
    }

    fn tstore(&mut self, address: Address, key: U256, value: U256) -> Result<()> {
        self.internals.tstore(address, key, value);
        Ok(())
    }

    fn emit_event(&mut self, address: Address, event: LogData) -> Result<()> {
        self.internals.log(Log { address, data: event });
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
        Ok(keccak256(data))
    }
}

#[cfg(test)]
mod tests {
    use alloy_evm::{EvmInternals, eth::EthEvmContext};
    use alloy_primitives::{Address, U256};
    use revm::{database::EmptyDB, primitives::hardfork::SpecId, state::Bytecode};

    use super::JournalStorageProvider;
    use crate::provider::PrecompileStorageProvider;

    const ADDR: Address = Address::repeat_byte(0x42);

    /// A persistent write is visible to a later read through the same journal,
    /// and neither charges any gas.
    #[test]
    fn sstore_then_sload_roundtrips_without_gas() {
        let mut ctx = EthEvmContext::new(EmptyDB::default(), SpecId::AMSTERDAM);
        let mut provider =
            JournalStorageProvider::new(EvmInternals::from_context(&mut ctx), Address::ZERO);

        let key = U256::from(7);
        let value = U256::from(99);
        provider.sstore(ADDR, key, value).unwrap();

        assert_eq!(provider.sload(ADDR, key).unwrap(), value);
        assert_eq!(provider.gas_used(), 0, "the gas-free provider must never charge gas");
        assert_eq!(provider.gas_limit(), u64::MAX);
        assert_eq!(provider.gas_refunded(), 0);
    }

    /// `set_code` persists and is reflected in the account's code hash, gas-free.
    #[test]
    fn set_code_persists_without_gas() {
        let mut ctx = EthEvmContext::new(EmptyDB::default(), SpecId::AMSTERDAM);
        let mut provider =
            JournalStorageProvider::new(EvmInternals::from_context(&mut ctx), Address::ZERO);

        let code = Bytecode::new_raw([0x60u8, 0x00].as_ref().into());
        let expected_hash = code.hash_slow();
        provider.set_code(ADDR, code).unwrap();

        let mut observed = None;
        provider.with_account_info(ADDR, &mut |info| observed = Some(info.code_hash)).unwrap();
        assert_eq!(observed, Some(expected_hash));
        assert_eq!(provider.gas_used(), 0);
    }

    /// Deducting gas is a no-op: it never fails and never advances the counter,
    /// even when asked for more than the (nominal) limit.
    #[test]
    fn gas_deduction_is_a_noop() {
        let mut ctx = EthEvmContext::new(EmptyDB::default(), SpecId::AMSTERDAM);
        let mut provider =
            JournalStorageProvider::new(EvmInternals::from_context(&mut ctx), Address::ZERO);

        provider.deduct_gas(1_000_000).unwrap();
        provider.deduct_state_gas(1_000_000).unwrap();
        provider.refund_gas(500);

        assert_eq!(provider.gas_used(), 0);
        assert_eq!(provider.state_gas_used(), 0);
        assert_eq!(provider.gas_refunded(), 0);
        assert!(!provider.is_static());
    }
}
