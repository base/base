//! [`PrecompileProvider`] for FPVM-accelerated OP Stack precompiles.

use alloc::{boxed::Box, string::String, vec::Vec};
use alloy_primitives::{Address, Bytes};
use op_revm::{
    precompiles::{fjord, granite, isthmus},
    OpSpecId,
};
use revm::{
    context::{Cfg, ContextTr},
    handler::{EthPrecompiles, PrecompileProvider},
    interpreter::{CallInput, CallInputs, Gas, InstructionResult, InterpreterResult},
    precompile::{Precompile as PrecompileWithAddress, PrecompileError, Precompiles},
    primitives::hardfork::SpecId,
};
use revm_precompile::{bn254, kzg_point_evaluation, secp256k1, secp256r1};

mod custom;
pub use custom::CustomCrypto;

mod factory;
pub use factory::ZkvmOpEvmFactory;

/// Get the ZKVM-accelerated precompiles.
fn get_precompiles() -> Vec<PrecompileWithAddress> {
    vec![
        bn254::add::ISTANBUL,
        bn254::mul::ISTANBUL,
        bn254::pair::ISTANBUL,
        secp256k1::ECRECOVER,
        secp256r1::P256VERIFY,
        kzg_point_evaluation::POINT_EVALUATION,
    ]
}

/// Get the cycle tracker name for a precompile address.
/// Returns None if the address is not a tracked precompile.
#[cfg(target_os = "zkvm")]
#[inline]
fn get_precompile_tracker_name(address: &Address) -> Option<&'static str> {
    // Compare against actual precompile constants
    if *address == *bn254::add::ISTANBUL.address() {
        Some("bn-add")
    } else if *address == *bn254::mul::ISTANBUL.address() {
        Some("bn-mul")
    } else if *address == *bn254::pair::ISTANBUL.address() {
        Some("bn-pair")
    } else if *address == *secp256k1::ECRECOVER.address() {
        Some("ec-recover")
    } else if *address == *secp256r1::P256VERIFY.address() {
        Some("p256-verify")
    } else if *address == *kzg_point_evaluation::POINT_EVALUATION.address() {
        Some("kzg-eval")
    } else {
        None
    }
}

/// The ZKVM-cycle-tracking precompiles.
#[derive(Debug)]
pub struct OpZkvmPrecompiles {
    /// The default [`EthPrecompiles`] provider.
    inner: EthPrecompiles,
    /// The [`OpSpecId`] of the precompiles.
    spec: OpSpecId,
}

impl OpZkvmPrecompiles {
    /// Create a new precompile provider with the given [`OpSpecId`].
    #[inline]
    pub fn new_with_spec(spec: OpSpecId) -> Self {
        let precompiles = match spec {
            spec @ (OpSpecId::BEDROCK |
            OpSpecId::REGOLITH |
            OpSpecId::CANYON |
            OpSpecId::ECOTONE) => Precompiles::new(spec.into_eth_spec().into()).clone(),
            OpSpecId::FJORD => fjord().clone(),
            OpSpecId::GRANITE | OpSpecId::HOLOCENE => granite().clone(),
            OpSpecId::ISTHMUS | OpSpecId::INTEROP | OpSpecId::OSAKA | OpSpecId::JOVIAN => {
                isthmus().clone()
            }
        };
        let mut precompiles_owned = precompiles.clone();
        precompiles_owned.extend(get_precompiles());
        let precompiles = Box::leak(Box::new(precompiles_owned));

        Self { inner: EthPrecompiles { precompiles, spec: SpecId::default() }, spec }
    }
}

impl<CTX> PrecompileProvider<CTX> for OpZkvmPrecompiles
where
    CTX: ContextTr<Cfg: Cfg<Spec = OpSpecId>>,
{
    type Output = InterpreterResult;

    #[inline]
    fn set_spec(&mut self, spec: <CTX::Cfg as Cfg>::Spec) -> bool {
        if spec == self.spec {
            return false;
        }
        *self = Self::new_with_spec(spec);
        true
    }

    #[inline]
    fn run(
        &mut self,
        context: &mut CTX,
        inputs: &CallInputs,
    ) -> Result<Option<Self::Output>, String> {
        let mut result = InterpreterResult {
            result: InstructionResult::Return,
            gas: Gas::new(inputs.gas_limit),
            output: Bytes::new(),
        };

        use revm::context::LocalContextTr;
        // NOTE: this snippet is refactored from the revm source code.
        // See https://github.com/bluealloy/revm/blob/9bc0c04fda0891e0e8d2e2a6dfd0af81c2af18c4/crates/handler/src/precompile_provider.rs#L111-L122.
        let shared_buffer;
        let input_bytes = match &inputs.input {
            CallInput::SharedBuffer(range) => {
                shared_buffer = context.local().shared_memory_buffer_slice(range.clone());
                shared_buffer.as_deref().unwrap_or(&[])
            }
            CallInput::Bytes(bytes) => bytes.0.iter().as_slice(),
        };

        // Priority:
        // 1. If the precompile has an accelerated version, use that.
        // 2. If the precompile is not accelerated, use the default version.
        // 3. If the precompile is not found, return None.
        let output = if let Some(precompile) = self.inner.precompiles.get(&inputs.bytecode_address)
        {
            // Track cycles for accelerated precompiles
            #[cfg(target_os = "zkvm")]
            let tracker_name = get_precompile_tracker_name(&inputs.bytecode_address);

            #[cfg(target_os = "zkvm")]
            if let Some(name) = tracker_name {
                println!("cycle-tracker-report-start: precompile-{}", name);
            }

            let result = precompile.execute(input_bytes, inputs.gas_limit);

            #[cfg(target_os = "zkvm")]
            if let Some(name) = tracker_name {
                println!("cycle-tracker-report-end: precompile-{}", name);
            }

            result
        } else {
            return Ok(None);
        };

        match output {
            Ok(output) => {
                let underflow = result.gas.record_cost(output.gas_used);
                assert!(underflow, "Gas underflow is not possible");
                result.result = InstructionResult::Return;
                result.output = output.bytes;
            }
            Err(PrecompileError::Fatal(e)) => return Err(e),
            Err(e) => {
                result.result = if e.is_oog() {
                    InstructionResult::PrecompileOOG
                } else {
                    InstructionResult::PrecompileError
                };
            }
        }

        Ok(Some(result))
    }

    #[inline]
    fn warm_addresses(&self) -> Box<impl Iterator<Item = Address>> {
        self.inner.warm_addresses()
    }

    #[inline]
    fn contains(&self, address: &Address) -> bool {
        self.inner.contains(address)
    }
}

#[cfg(test)]
mod test {
    use super::*;
    use alloy_primitives::U256;
    use op_revm::{DefaultOp as _, OpContext};
    use revm::{
        database::EmptyDB,
        handler::PrecompileProvider,
        interpreter::{CallInput, CallScheme, CallValue},
        Context,
    };

    type TestContext = OpContext<EmptyDB>;

    /// Creates a [`CallInputs`] with `bytecode_address` set to the given address
    /// and `target_address` set to zero, simulating a DELEGATECALL scenario.
    fn create_call_inputs(address: Address, input: Bytes, gas_limit: u64) -> CallInputs {
        CallInputs {
            input: CallInput::Bytes(input),
            gas_limit,
            bytecode_address: address,
            target_address: Address::ZERO, // Simulates DELEGATECALL context
            caller: Address::ZERO,
            value: CallValue::Transfer(U256::ZERO),
            scheme: CallScheme::Call,
            is_static: false,
            return_memory_offset: 0..0,
            known_bytecode: None,
        }
    }

    fn create_test_context() -> TestContext {
        Context::op().with_db(EmptyDB::new())
    }

    /// Test that precompiles are looked up by `bytecode_address`, not `target_address`.
    /// This is critical for DELEGATECALL scenarios where these addresses differ.
    #[test]
    fn test_precompile_lookup_uses_bytecode_address() {
        let mut ctx = create_test_context();
        let mut precompiles = OpZkvmPrecompiles::new_with_spec(OpSpecId::BEDROCK);

        // SHA256 precompile at address 0x02
        let sha256_addr = revm::precompile::u64_to_address(2);

        // Create inputs where bytecode_address != target_address (DELEGATECALL scenario)
        let call_inputs = create_call_inputs(sha256_addr, Bytes::from_static(b"test"), u64::MAX);

        // Verify target_address is different from bytecode_address
        assert_ne!(call_inputs.bytecode_address, call_inputs.target_address);

        // Should find the precompile via bytecode_address
        let result = precompiles.run(&mut ctx, &call_inputs).unwrap();
        assert!(result.is_some(), "Precompile should be found via bytecode_address");

        let interpreter_result = result.unwrap();
        assert_eq!(interpreter_result.result, InstructionResult::Return);
        assert!(!interpreter_result.output.is_empty());
    }

    /// Test that a non-existent precompile returns None.
    #[test]
    fn test_run_nonexistent_precompile() {
        let mut ctx = create_test_context();
        let mut precompiles = OpZkvmPrecompiles::new_with_spec(OpSpecId::BEDROCK);

        let fake_addr = Address::from_slice(&[0xFFu8; 20]);
        let call_inputs = create_call_inputs(fake_addr, Bytes::new(), u64::MAX);

        let result = precompiles.run(&mut ctx, &call_inputs).unwrap();
        assert!(result.is_none());
    }

    /// Test out-of-gas handling for precompiles.
    #[test]
    fn test_run_out_of_gas() {
        let mut ctx = create_test_context();
        let mut precompiles = OpZkvmPrecompiles::new_with_spec(OpSpecId::BEDROCK);

        let sha256_addr = revm::precompile::u64_to_address(2);
        let call_inputs = create_call_inputs(sha256_addr, Bytes::from_static(b"test"), 0);

        let result = precompiles.run(&mut ctx, &call_inputs).unwrap();
        assert!(result.is_some());

        let interpreter_result = result.unwrap();
        assert_eq!(interpreter_result.result, InstructionResult::PrecompileOOG);
    }

    /// Test SharedBuffer input handling.
    #[test]
    fn test_run_with_shared_buffer_empty() {
        let mut ctx = create_test_context();
        let mut precompiles = OpZkvmPrecompiles::new_with_spec(OpSpecId::BEDROCK);

        let sha256_addr = revm::precompile::u64_to_address(2);
        let call_inputs = CallInputs {
            input: CallInput::SharedBuffer(0..0),
            gas_limit: u64::MAX,
            bytecode_address: sha256_addr,
            target_address: Address::ZERO,
            caller: Address::ZERO,
            value: CallValue::Transfer(U256::ZERO),
            scheme: CallScheme::Call,
            is_static: false,
            return_memory_offset: 0..0,
            known_bytecode: None,
        };

        let result = precompiles.run(&mut ctx, &call_inputs).unwrap();
        assert!(result.is_some());
    }
}
