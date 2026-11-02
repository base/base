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
    context::journaled_state::JournalCheckpoint,
    context_interface::cfg::GasParams,
    interpreter::gas::{Gas, KECCAK256, KECCAK256WORD, LOG},
    primitives::keccak256,
    state::{AccountInfo, Bytecode},
};

use crate::{
    error::{BasePrecompileError, Result},
    provider::{PrecompileStorageProvider, StorageFeatures, validate_loaded_code_presence},
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
    origin: Address,
    storage_features: StorageFeatures,
}

impl<'a> EvmPrecompileStorageProvider<'a> {
    /// Consume a [`PrecompileInput`] and build the provider.
    ///
    /// `gas_params` drives all EIP-2929/2200/3529 cost calculations.
    /// The caller must select the active fork's gas parameters.
    pub fn new(input: PrecompileInput<'a>, gas_params: GasParams) -> Self {
        Self::new_with_storage_features(input, gas_params, StorageFeatures::Legacy)
    }

    /// Consume a [`PrecompileInput`] and build the provider with fork-specific storage features.
    pub fn new_with_storage_features(
        input: PrecompileInput<'a>,
        gas_params: GasParams,
        storage_features: StorageFeatures,
    ) -> Self {
        let PrecompileInput { gas, reservoir, caller, value, is_static, internals, .. } = input;

        let block_number = internals.block_env().number().to::<u64>();
        let timestamp = internals.block_env().timestamp();
        let chain_id = internals.chain_id();
        let beneficiary = internals.block_env().beneficiary();
        let origin = internals.tx_origin();

        Self {
            internals,
            caller,
            call_value: value,
            gas: Gas::new_with_regular_gas_and_reservoir(gas, reservoir),
            gas_params,
            is_static,
            block_number,
            timestamp,
            chain_id,
            beneficiary,
            origin,
            storage_features,
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

    fn origin(&self) -> Address {
        self.origin
    }

    fn set_code(&mut self, address: Address, code: Bytecode) -> Result<()> {
        if self.is_static {
            return Err(BasePrecompileError::StaticCallViolation);
        }

        let code_len = code.len();

        // Code deposit is regular gas before Amsterdam, state gas thereafter.
        self.deduct_gas(self.gas_params.code_deposit_cost(code_len))?;
        self.deduct_state_gas(self.gas_params.code_deposit_state_gas(code_len))?;

        // Charge CREATE equivalent costs whenever code is written to an account that had no code,
        // regardless of its balance. A prefunded account (balance > 0, no code) passes the
        // factory's collision check (which only rejects accounts that already have code), but
        // must still pay G_create and the keccak hash cost.
        let is_new_code = {
            let state_load = self
                .internals
                .load_account(address)
                .map_err(|e| BasePrecompileError::Fatal(e.to_string()))?;
            state_load.data.info.is_empty_code_hash()
        };

        if is_new_code {
            // CREATE access is regular gas; Amsterdam metadata creation is state gas.
            self.deduct_gas(self.gas_params.create_cost())?;
            self.deduct_state_gas(self.gas_params.create_state_gas())?;
            // Yellow Paper G_sha3 + G_sha3word: cost of computing the stored code hash.
            let num_words = code_len.div_ceil(32) as u64;
            self.deduct_gas(KECCAK256.saturating_add(KECCAK256WORD.saturating_mul(num_words)))?;
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
        // load_account is sufficient: every caller only needs code_hash — never info.code.
        // code_hash is always eagerly populated; load_account_code would fetch bytecode from
        // the database unnecessarily.
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

    fn with_account_code(&mut self, address: Address, f: &mut dyn FnMut(&Bytecode)) -> Result<()> {
        // Load and clone the full bytecode before releasing the journal borrow.
        let (code, is_cold) = {
            let state_load = self
                .internals
                .load_account_code(address)
                .map_err(|e| BasePrecompileError::Fatal(e.to_string()))?;
            let expected_hash = *state_load.data.code_hash();
            let code = state_load.data.code().cloned().ok_or_else(|| {
                BasePrecompileError::Fatal(
                    "account code unavailable after successful EVM load".to_string(),
                )
            })?;
            validate_loaded_code_presence(expected_hash, &code)?;
            (code, state_load.is_cold)
        };

        // EIP-2929: charge the same account-access cost as `with_account_info`.
        self.deduct_gas(self.gas_params.warm_storage_read_cost())?;
        if is_cold {
            self.deduct_gas(self.gas_params.cold_account_additional_cost())?;
        }

        f(&code);
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

    fn tload_unmetered(&mut self, address: Address, key: U256) -> Result<U256> {
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
            // Match revm's SSTORE state charge and uncapped 0→x→0 refill.
            self.deduct_state_gas(self.gas_params.sstore_state_gas(&s.data))?;
            self.gas.refill_reservoir(self.gas_params.sstore_state_gas_refill(&s.data));
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
        if !self.gas.record_state_cost(gas) {
            return Err(BasePrecompileError::OutOfGas);
        }
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
        self.gas.tracker().state_gas_spent().max(0) as u64
    }

    fn gas_refunded(&self) -> i64 {
        self.gas.refunded()
    }

    fn reservoir(&self) -> u64 {
        self.gas.reservoir()
    }

    fn gas_tracker(&self) -> revm::interpreter::gas::GasTracker {
        *self.gas.tracker()
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

    fn checkpoint(&mut self) -> JournalCheckpoint {
        self.internals.checkpoint()
    }

    fn commit_latest_checkpoint(&mut self) {
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
    use alloy_primitives::{Address, Bytes, U256};
    use revm::{
        context_interface::cfg::GasParams, database::EmptyDB, primitives::hardfork::SpecId,
        state::Bytecode,
    };

    use crate::{
        BytesLikeHandler, Handler, StorageCtx,
        error::BasePrecompileError,
        hashmap::HashMapStorageProvider,
        provider::{PrecompileStorageProvider, StorageFeatures},
    };

    #[test]
    fn native_amsterdam_slot_refill_restores_each_funding_pool() {
        for reservoir in [0, 50_000, 200_000] {
            let mut ctx = EthEvmContext::new(EmptyDB::default(), SpecId::AMSTERDAM);
            let mut provider =
                make_evm_provider(&mut ctx, GasParams::new_spec(SpecId::AMSTERDAM), 200_000, false);
            provider.gas.tracker_mut().set_reservoir(reservoir);
            provider.sstore(Address::ZERO, U256::ZERO, U256::ONE).unwrap();
            let spill = 97_920u64.saturating_sub(reservoir);
            assert_eq!(
                provider.gas_used(),
                12_100 + spill,
                "fresh SSTORE charges regular access/write plus reservoir deficit"
            );
            assert_eq!(
                provider.gas_tracker().state_gas_spent(),
                97_920,
                "fresh slot charges exactly 64 bytes at CPSB 1530"
            );
            assert_eq!(
                provider.gas_tracker().state_gas_spilled(),
                spill,
                "only the unfunded state charge spills"
            );
            provider.sstore(Address::ZERO, U256::ZERO, U256::ZERO).unwrap();
            let out = StorageCtx::enter(&mut provider, |ctx| ctx.success_output(Bytes::new()));
            assert_eq!(
                out.gas_used, 12_200,
                "0→x→0 restores spilled state gas, not regular SSTORE charges"
            );
            assert_eq!(out.state_gas_used, 0, "restored slot has no net state charge");
            assert_eq!(out.state_gas_spilled, 0, "restoration repays the regular pool first");
            assert_eq!(
                out.reservoir, reservoir,
                "restoration returns the original reservoir in full"
            );
            assert_eq!(
                out.gas_refunded, 10_000,
                "regular restoration refund stays in the capped refund counter"
            );
        }
    }

    #[test]
    fn native_amsterdam_cross_frame_refill_preserves_signed_state_gas() {
        let mut ctx = EthEvmContext::new(EmptyDB::default(), SpecId::AMSTERDAM);
        {
            let mut parent =
                make_evm_provider(&mut ctx, GasParams::new_spec(SpecId::AMSTERDAM), 200_000, false);
            parent.sstore(Address::ZERO, U256::ZERO, U256::ONE).unwrap();
        }
        let mut child =
            make_evm_provider(&mut ctx, GasParams::new_spec(SpecId::AMSTERDAM), 200_000, false);
        child.sstore(Address::ZERO, U256::ZERO, U256::ZERO).unwrap();
        let out = StorageCtx::enter(&mut child, |ctx| ctx.success_output(Bytes::new()));
        assert_eq!(
            out.state_gas_used, -97_920,
            "a child can return state gas charged by its ancestor"
        );
        assert_eq!(out.state_gas_spilled, 0, "the child did not fund the ancestor's charge");
        assert_eq!(
            out.reservoir, 97_920,
            "the ancestor reconciles the returned reservoir with its spill"
        );
    }

    #[test]
    fn native_amsterdam_revert_and_halt_restore_reservoir_and_spill() {
        for halt in [false, true] {
            let mut ctx = EthEvmContext::new(EmptyDB::default(), SpecId::AMSTERDAM);
            let mut provider =
                make_evm_provider(&mut ctx, GasParams::new_spec(SpecId::AMSTERDAM), 200_000, false);
            provider.gas.tracker_mut().set_reservoir(50_000);
            provider.sstore(Address::ZERO, U256::ZERO, U256::ONE).unwrap();
            let output = StorageCtx::enter(&mut provider, |ctx| {
                if halt {
                    ctx.error_result(BasePrecompileError::OutOfGas).unwrap()
                } else {
                    ctx.revert_output(Bytes::new())
                }
            });
            assert_eq!(
                output.state_gas_used, 97_920,
                "failure output must retain state charge for frame rollback"
            );
            assert_eq!(
                output.state_gas_spilled, 47_920,
                "failure output must retain spill for frame rollback"
            );
            let mut child =
                revm::handler::precompile_output_to_interpreter_result(output.into(), 200_000);
            let mut parent = revm::interpreter::gas::GasTracker::new(200_000, 0, 50_000);
            revm::handler::handle_reservoir_remaining_gas(
                child.result,
                &mut parent,
                child.gas.tracker_mut(),
            );
            assert_eq!(
                parent.reservoir(),
                50_000,
                "both revert and halt restore the inherited reservoir"
            );
            assert_eq!(
                parent.remaining(),
                if halt { 0 } else { 187_900 },
                "revert returns spill and unused regular gas; halt consumes regular gas"
            );
            assert_eq!(parent.state_gas_spent(), 0, "failed frame contributes no state gas");
            assert_eq!(parent.state_gas_spilled(), 0, "failed frame contributes no spill");
            assert_eq!(parent.refunded(), 0, "failed frame contributes no refund");
        }
    }

    #[test]
    fn native_amsterdam_code_creation_charges_account_and_bytes() {
        let mut ctx = EthEvmContext::new(EmptyDB::default(), SpecId::AMSTERDAM);
        let mut provider =
            make_evm_provider(&mut ctx, GasParams::new_spec(SpecId::AMSTERDAM), 200_000, false);
        provider.gas.tracker_mut().set_reservoir(200_000);
        provider
            .set_code(Address::ZERO, Bytecode::new_legacy(Bytes::from_static(&[0xef])))
            .unwrap();
        let out = StorageCtx::enter(&mut provider, |ctx| ctx.success_output(Bytes::new()));
        assert_eq!(out.gas_used, 12_036, "CREATE access plus one-word keccak are regular gas");
        assert_eq!(
            out.state_gas_used, 185_130,
            "new contract metadata and one code byte cost (120+1)×1530"
        );
        assert_eq!(
            out.state_gas_spilled, 0,
            "a sufficient reservoir funds all code/account state gas"
        );
        assert_eq!(out.reservoir, 14_870, "unused code/account reservoir must be returned");
    }

    fn amsterdam_provider() -> HashMapStorageProvider {
        let mut provider = HashMapStorageProvider::new(1);
        provider.set_gas_params(GasParams::new_spec(SpecId::AMSTERDAM));
        provider
    }

    fn make_evm_provider<'a>(
        ctx: &'a mut EthEvmContext<EmptyDB>,
        gas_params: GasParams,
        gas: u64,
        is_static: bool,
    ) -> super::EvmPrecompileStorageProvider<'a> {
        make_evm_provider_with_features(ctx, gas_params, gas, is_static, StorageFeatures::Legacy)
    }

    fn make_evm_provider_with_features<'a>(
        ctx: &'a mut EthEvmContext<EmptyDB>,
        gas_params: GasParams,
        gas: u64,
        is_static: bool,
        storage_features: StorageFeatures,
    ) -> super::EvmPrecompileStorageProvider<'a> {
        let input = PrecompileInput {
            data: &[],
            gas,
            reservoir: 0,
            caller: Address::ZERO,
            value: U256::ZERO,
            target_address: Address::ZERO,
            is_static,
            bytecode_address: Address::ZERO,
            internals: EvmInternals::from_context(ctx),
        };
        super::EvmPrecompileStorageProvider::new_with_storage_features(
            input,
            gas_params,
            storage_features,
        )
    }

    /// EIP-2200 stipend boundary: `remaining == call_stipend` (2300) must block.
    #[test]
    fn sstore_oog_at_call_stipend_boundary() {
        let gas_params = GasParams::default();
        let mut ctx = EthEvmContext::new(EmptyDB::default(), SpecId::AMSTERDAM);
        let mut provider =
            make_evm_provider(&mut ctx, gas_params.clone(), gas_params.call_stipend(), false);

        assert_eq!(
            provider.sstore(Address::ZERO, U256::ZERO, U256::from(1u64)),
            Err(BasePrecompileError::OutOfGas),
        );
    }

    /// Below the stipend (2299): also blocked.
    #[test]
    fn sstore_oog_below_call_stipend() {
        let gas_params = GasParams::default();
        let mut ctx = EthEvmContext::new(EmptyDB::default(), SpecId::AMSTERDAM);
        let mut provider =
            make_evm_provider(&mut ctx, gas_params.clone(), gas_params.call_stipend() - 1, false);

        assert_eq!(
            provider.sstore(Address::ZERO, U256::ZERO, U256::from(1u64)),
            Err(BasePrecompileError::OutOfGas),
        );
    }

    /// Strictly above the stipend: guard passes, sstore completes.
    #[test]
    fn sstore_allowed_above_call_stipend() {
        let gas_params = GasParams::default();
        let mut ctx = EthEvmContext::new(EmptyDB::default(), SpecId::AMSTERDAM);
        // Large margin covers the stipend guard + cold sstore costs (~2600 gas).
        let gas = gas_params.call_stipend() + 1_000_000;
        let mut provider = make_evm_provider(&mut ctx, gas_params, gas, false);

        assert!(provider.sstore(Address::ZERO, U256::ZERO, U256::from(1u64)).is_ok());
    }

    #[test]
    fn cobalt_bytes_shrink_records_revm_storage_refund() {
        let gas_params = GasParams::new_spec(SpecId::OSAKA);
        let mut ctx = EthEvmContext::new(EmptyDB::default(), SpecId::OSAKA);
        let address = Address::repeat_byte(0x42);
        let base_slot = U256::from(10u64);
        let mut provider = make_evm_provider_with_features(
            &mut ctx,
            gas_params,
            1_000_000,
            false,
            StorageFeatures::Cobalt,
        );

        StorageCtx::enter(&mut provider, |storage| {
            let mut slot = BytesLikeHandler::<Bytes>::new(base_slot, address, storage);
            slot.write(Bytes::from(vec![0x11; 64])).unwrap();
            assert_eq!(storage.gas_refunded(), 0);

            slot.write(Bytes::from_static(b"short")).unwrap();
            assert!(storage.gas_refunded() > 0, "clearing long-data slots must record a refund");
        });
    }

    /// Static-call violation is checked before the stipend guard.
    #[test]
    fn sstore_static_violation_checked_before_stipend_guard() {
        let gas_params = GasParams::default();
        let mut ctx = EthEvmContext::new(EmptyDB::default(), SpecId::AMSTERDAM);
        // Gas at stipend boundary — both guards would fire, static takes priority.
        let mut provider =
            make_evm_provider(&mut ctx, gas_params.clone(), gas_params.call_stipend(), true);

        assert_eq!(
            provider.sstore(Address::ZERO, U256::ZERO, U256::from(1u64)),
            Err(BasePrecompileError::StaticCallViolation),
        );
    }

    #[test]
    fn unmetered_tload_does_not_expose_opcode_gas() {
        let gas_params = GasParams::new_spec(SpecId::AMSTERDAM);
        let warm_read_cost = gas_params.warm_storage_read_cost();
        let mut ctx = EthEvmContext::new(EmptyDB::default(), SpecId::AMSTERDAM);
        let mut provider = make_evm_provider(&mut ctx, gas_params, 1_000_000, false);

        assert_eq!(provider.tload_unmetered(Address::ZERO, U256::ZERO).unwrap(), U256::ZERO);
        assert_eq!(provider.gas_used(), 0);

        assert_eq!(provider.tload(Address::ZERO, U256::ZERO).unwrap(), U256::ZERO);
        assert_eq!(provider.gas_used(), warm_read_cost);
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

    /// `set_code` in a static context returns `StaticCallViolation` before any gas is charged.
    #[test]
    fn set_code_static_violation_before_gas_charge() {
        let gas_params = GasParams::default();
        let mut ctx = EthEvmContext::new(EmptyDB::default(), SpecId::AMSTERDAM);
        let gas = 1_000_000;
        let mut provider = make_evm_provider(&mut ctx, gas_params, gas, true);

        assert_eq!(
            provider.set_code(Address::ZERO, Bytecode::new_raw([0x60u8, 0x00].as_ref().into())),
            Err(BasePrecompileError::StaticCallViolation),
        );
        // No gas must have been consumed.
        assert_eq!(provider.gas_used(), 0);
    }

    /// `set_code` on a prefunded account (balance > 0, no code) must charge the same gas
    /// as a fully empty account. Before the fix, `is_empty()` returned false for prefunded
    /// accounts, silently skipping `G_create` and the keccak hash cost (~32 036 gas).
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
            "prefunded account must pay identical gas to an empty account"
        );
    }

    /// `set_code` on an account that already has code must replace it without error.
    #[test]
    fn set_code_existing_account_replaces_code() {
        let mut provider = amsterdam_provider();
        let addr = Address::from([0x42u8; 20]);
        let code1 = Bytecode::new_raw([0x60u8, 0x00].as_ref().into());
        let code2 = Bytecode::new_raw([0x60u8, 0x01].as_ref().into());
        let expected_hash = code2.hash_slow();

        provider.set_code(addr, code1).unwrap();
        provider.set_code(addr, code2).unwrap();

        assert_eq!(
            provider.get_account_info(addr).map(|i| i.code_hash),
            Some(expected_hash),
            "second set_code must replace the stored code hash"
        );
    }

    #[test]
    fn set_code_static_context_reverts_before_code_mutation() {
        let mut provider = amsterdam_provider();
        provider.set_static(true);
        let addr = Address::from([0x42u8; 20]);
        let code = Bytecode::new_raw([0x60u8, 0x00].as_ref().into());

        let err = provider.set_code(addr, code).unwrap_err();

        assert_eq!(err, BasePrecompileError::StaticCallViolation);
        assert!(provider.get_account_info(addr).is_none());
    }
}
