use alloc::string::{String, ToString};

use auto_impl::auto_impl;

use crate::{
    Address, AddressSet, Bytes, CallInputs, Cfg, ContextTr,
    CryptoPrecompileOutput as PrecompileOutput, CryptoPrecompileStatus as PrecompileStatus, Gas,
    InstructionResult, InterpreterResult, JournalTr, LocalContextTr, PrecompileSpecId, Precompiles,
    hardfork::SpecId,
};

/// Provider for precompiled contracts in the EVM.
#[auto_impl(&mut, Box)]
pub trait PrecompileProvider<CTX: ContextTr> {
    /// The output type returned by precompile execution.
    type Output;

    /// Sets the spec id and returns true if the spec id was changed. Initial call to set_spec will always return true.
    ///
    /// Returns `true` if precompile addresses should be injected into the journal.
    fn set_spec(&mut self, spec: <CTX::Cfg as Cfg>::Spec) -> bool;

    /// Run the precompile.
    fn run(
        &mut self,
        context: &mut CTX,
        inputs: &CallInputs,
    ) -> Result<Option<Self::Output>, String>;

    /// Get the warm addresses.
    fn warm_addresses(&self) -> &AddressSet;

    /// Check if the address is a precompile.
    fn contains(&self, address: &Address) -> bool {
        self.warm_addresses().contains(address)
    }
}

/// The [`PrecompileProvider`] for ethereum precompiles.
#[derive(Debug)]
pub struct EthPrecompiles {
    /// Contains precompiles for the current spec.
    pub precompiles: &'static Precompiles,
    /// Current spec. None means that spec was not set yet.
    pub spec: SpecId,
}

impl EthPrecompiles {
    /// Create a new precompile provider with the given spec.
    pub fn new(spec: SpecId) -> Self {
        Self { precompiles: Precompiles::new(PrecompileSpecId::from_spec_id(spec)), spec }
    }

    /// Returns addresses of the precompiles.
    pub const fn warm_addresses(&self) -> &AddressSet {
        self.precompiles.addresses_set()
    }

    /// Returns whether the address is a precompile.
    pub fn contains(&self, address: &Address) -> bool {
        self.precompiles.contains(address)
    }
}

impl Clone for EthPrecompiles {
    fn clone(&self) -> Self {
        Self { precompiles: self.precompiles, spec: self.spec }
    }
}

/// Converts a [`PrecompileOutput`] into an [`InterpreterResult`] for a call frame
/// with `gas_limit` regular gas.
///
/// Maps precompile status to the corresponding instruction result:
/// - `Success` -> [`InstructionResult::Return`]
/// - `Revert` -> [`InstructionResult::Revert`]
/// - `Halt(OOG)` -> [`InstructionResult::PrecompileOOG`]
/// - `Halt(other)` -> [`InstructionResult::PrecompileError`]
///
/// A precompile that reports more gas than it was given is downgraded to
/// [`InstructionResult::PrecompileOOG`]. Anything but a success or revert consumes
/// all regular gas and returns no output bytes.
pub fn precompile_output_to_interpreter_result(
    output: PrecompileOutput,
    gas_limit: u64,
) -> InterpreterResult {
    // A precompile lying about its usage must not leave the frame with gas it
    // never had: charging more regular gas than the limit is an OOG halt.
    let result = if output.gas_used > gas_limit {
        InstructionResult::PrecompileOOG
    } else {
        match &output.status {
            PrecompileStatus::Success => InstructionResult::Return,
            PrecompileStatus::Revert => InstructionResult::Revert,
            PrecompileStatus::Halt(reason) if reason.is_oog() => InstructionResult::PrecompileOOG,
            PrecompileStatus::Halt(_) => InstructionResult::PrecompileError,
        }
    };

    // Gas used, refund, state gas (with its spilled portion, so a later rollback
    // credits it back to regular gas per EIP-8037) and the reservoir all come from
    // the precompile's own accounting.
    let mut gas = Gas::new(gas_limit);
    *gas.tracker_mut() = output.to_gas_tracker(gas_limit);

    // Only a success or revert returns output bytes and keeps its unspent gas.
    if result.is_halt() {
        gas.spend_all();
        return InterpreterResult::new(result, Bytes::new(), gas);
    }

    InterpreterResult::new(result, output.bytes, gas)
}

impl<CTX: ContextTr> PrecompileProvider<CTX> for EthPrecompiles {
    type Output = InterpreterResult;

    fn set_spec(&mut self, spec: <CTX::Cfg as Cfg>::Spec) -> bool {
        let spec = spec.into();
        // generate new precompiles only on new spec
        if spec == self.spec {
            return false;
        }
        self.precompiles = Precompiles::new(PrecompileSpecId::from_spec_id(spec));
        self.spec = spec;
        true
    }

    fn run(
        &mut self,
        context: &mut CTX,
        inputs: &CallInputs,
    ) -> Result<Option<InterpreterResult>, String> {
        let Some(precompile) = self.precompiles.get(&inputs.bytecode_address) else {
            return Ok(None);
        };

        let output = precompile
            .execute(&inputs.input.as_bytes(context), inputs.gas_limit, inputs.reservoir)
            .map_err(|e| e.to_string())?;

        // If this is a top-level precompile call (depth == 1), persist the error message
        // into the local context so it can be returned as output in the final result.
        // Only do this for non-OOG halt errors.
        if let Some(halt_reason) = output.halt_reason() {
            if !halt_reason.is_oog() && context.journal().depth() == 1 {
                context.local_mut().set_precompile_error_context(halt_reason.to_string());
            }
        }

        let result = precompile_output_to_interpreter_result(output, inputs.gas_limit);
        Ok(Some(result))
    }

    fn warm_addresses(&self) -> &AddressSet {
        Self::warm_addresses(self)
    }

    fn contains(&self, address: &Address) -> bool {
        Self::contains(self, address)
    }
}
