use alloy_primitives::{Address, B256, Bytes, Log};

use super::{GasTracker, InstrStop, Message, Result, Word};
use crate::{
    BaseEvmTypes, EvmFeatures, EvmTypesHost, SpecId,
    env::{BlockEnv, TxEnv},
    evm::{AccountLoad, SLoad, SStore, SelfDestructResult},
};

/// Result of executing a call/create message for an EVM type family.
pub type MessageResult<T = BaseEvmTypes> = MessageResultExt<<T as EvmTypesHost>::MessageResultExt>;

/// Result of executing a call/create message, parameterized by extension data.
///
/// [`Self::gas`] is normalized for the frame's [`stop`](Self::stop) reason by the executor
/// before it is returned, so applying a child result to its caller is just
/// [`GasTracker::merge_child_gas`]. Use [`Self::gas_remaining_after_final_refund`] or
/// [`Self::gas_used_after_final_refund`] for top-level transaction accounting.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct MessageResultExt<E = ()> {
    /// Interpreter stop reason.
    pub stop: InstrStop,
    /// Gas accounting for the child frame.
    pub gas: GasTracker,
    /// Return or revert output.
    pub output: Bytes,
    /// Created address for successful create messages.
    pub created_address: Option<Address>,
    /// EVM type-specific extension data.
    pub ext: E,
    #[doc(hidden)] // Not public API. Please use an existing constructor.
    pub _non_exhaustive: (),
}

impl<E> MessageResultExt<E> {
    /// Returns whether the message committed state changes.
    #[inline]
    pub const fn is_success(&self) -> bool {
        self.stop.is_success()
    }

    /// Returns the created address to push onto the parent frame's stack.
    ///
    /// Yields the created address as a stack word on success, or zero when the
    /// create failed (revert, halt, or an early-fail path that left no address).
    #[inline]
    pub fn created_address_for_parent(&self) -> Word {
        self.created_address
            .filter(|_| self.stop.is_success())
            .map(|address| Word::from_be_slice(address.as_slice()))
            .unwrap_or_default()
    }

    /// Calculates the final refund amount for a top-level transaction.
    #[inline]
    pub const fn final_refund(&self, gas_limit: u64, max_refund_quotient: u64) -> u64 {
        if self.gas.refunded() <= 0 {
            return 0;
        }
        let max_refund_quotient = if max_refund_quotient == 0 { 1 } else { max_refund_quotient };
        // EIP-8037: the unused state-gas reservoir is reimbursed to the caller, so it
        // is not part of the gas actually spent. The EIP-3529 refund cap is a fraction
        // of the gas the transaction truly consumed, so the reservoir must be excluded
        // — otherwise a large idle reservoir inflates `spent` and disables the cap.
        let spent =
            gas_limit.saturating_sub(self.gas.remaining()).saturating_sub(self.gas.reservoir());
        let refund = self.gas.refunded() as u64;
        let cap = spent / max_refund_quotient;
        if refund < cap { refund } else { cap }
    }

    /// Returns top-level gas remaining after applying the final refund cap.
    #[inline]
    pub const fn gas_remaining_after_final_refund(
        &self,
        gas_limit: u64,
        max_refund_quotient: u64,
    ) -> u64 {
        let refunded = self.final_refund(gas_limit, max_refund_quotient);
        // EIP-8037: the unused reservoir (already settled to its frame-start value
        // on failure by `rollback_state_gas`) is also reimbursed to the caller.
        let remaining =
            self.gas.remaining().saturating_add(self.gas.reservoir()).saturating_add(refunded);
        if remaining < gas_limit { remaining } else { gas_limit }
    }

    /// Returns top-level gas used after applying the final refund cap.
    #[inline]
    pub const fn gas_used_after_final_refund(
        &self,
        gas_limit: u64,
        max_refund_quotient: u64,
    ) -> u64 {
        gas_limit
            .saturating_sub(self.gas_remaining_after_final_refund(gas_limit, max_refund_quotient))
    }
}

/// External host operations.
pub trait Host<T: EvmTypesHost> {
    /// Returns the active base specification ID.
    fn spec_id(&self) -> SpecId;

    /// Returns the block environment.
    fn block_env(&mut self) -> &BlockEnv<T>;

    /// Loads account information.
    fn load_account(
        &mut self,
        address: &Address,
        load_code: bool,
        skip_cold_load: bool,
    ) -> Result<AccountLoad, InstrStop>;

    /// Returns whether an account is empty/non-existent for new-account gas checks.
    fn target_is_empty_for_new_account_gas(
        &mut self,
        address: &Address,
        features: EvmFeatures,
    ) -> Result<bool, InstrStop>;

    /// Returns a historical block hash.
    fn block_hash(&mut self, number: &Word) -> Result<B256, InstrStop>;

    /// Loads a persistent storage slot.
    fn sload(
        &mut self,
        address: &Address,
        key: &Word,
        skip_cold_load: bool,
    ) -> Result<SLoad, InstrStop>;

    /// Stores a persistent storage slot.
    fn sstore(
        &mut self,
        address: &Address,
        key: &Word,
        value: &Word,
        skip_cold_load: bool,
    ) -> Result<SStore, InstrStop>;

    /// Loads a transient storage slot.
    fn tload(&mut self, address: &Address, key: &Word) -> Word;

    /// Stores a transient storage slot.
    fn tstore(&mut self, address: &Address, key: &Word, value: &Word);

    /// Records an emitted log.
    fn log(&mut self, log: Log);

    /// Executes a message inside this host.
    fn execute_message(&mut self, tx_env: &TxEnv<T>, message: &mut Message<T>) -> MessageResult<T>;

    /// Registers the current contract for self-destruction.
    fn selfdestruct(
        &mut self,
        contract: &Address,
        target: &Address,
        skip_cold_load: bool,
    ) -> Result<SelfDestructResult, InstrStop>;
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::interpreter::GasTracker;

    #[test]
    fn final_refund_uses_configured_quotient() {
        let mut result =
            MessageResult::<BaseEvmTypes> { gas: GasTracker::new(100), ..Default::default() };
        result.gas.set_remaining(20);
        result.gas.set_refunded(60);

        assert_eq!(result.final_refund(100, 2), 40);
        assert_eq!(result.final_refund(100, 5), 16);
        assert_eq!(result.final_refund(100, 1), 60);
        assert_eq!(result.final_refund(100, 0), 60);
    }
}
