//! [`PrecompileProvider`] for FPVM-accelerated OP Stack precompiles.

use alloc::{boxed::Box, string::String};
use alloy_primitives::{Address, Bytes};
use op_revm::{
    precompiles::{fjord, granite, isthmus},
    OpSpecId,
};
use revm::{
    context::{Cfg, ContextTr},
    handler::{EthPrecompiles, PrecompileProvider},
    interpreter::{Gas, InputsImpl, InstructionResult, InterpreterResult},
    precompile::{bn128, PrecompileError, PrecompileResult, PrecompileWithAddress, Precompiles},
    primitives::hardfork::SpecId,
};

mod factory;
pub use factory::ZkvmOpEvmFactory;

/// Create an annotated precompile that simply tracks the cycle count of a precompile.
macro_rules! create_annotated_precompile {
    ($precompile:expr, $name:expr) => {
        PrecompileWithAddress($precompile.0, |input: &Bytes, gas_limit: u64| -> PrecompileResult {
            let precompile = $precompile.precompile();

            #[cfg(target_os = "zkvm")]
            println!(concat!("cycle-tracker-report-start: precompile-", $name));

            let result = precompile(input, gas_limit);

            #[cfg(target_os = "zkvm")]
            println!(concat!("cycle-tracker-report-end: precompile-", $name));

            result
        })
    };
}

/// Tuples of the original and annotated precompiles.
// TODO: Add kzg_point_evaluation once it has standard precompile support in revm-precompile 0.17.0.
const PRECOMPILES: &[(PrecompileWithAddress, PrecompileWithAddress)] = &[
    (bn128::add::ISTANBUL, create_annotated_precompile!(bn128::add::ISTANBUL, "bn-add")),
    (bn128::mul::ISTANBUL, create_annotated_precompile!(bn128::mul::ISTANBUL, "bn-mul")),
    (bn128::pair::ISTANBUL, create_annotated_precompile!(bn128::pair::ISTANBUL, "bn-pair")),
    (
        revm::precompile::secp256k1::ECRECOVER,
        create_annotated_precompile!(revm::precompile::secp256k1::ECRECOVER, "ec-recover"),
    ),
    (
        revm::precompile::secp256r1::P256VERIFY,
        create_annotated_precompile!(revm::precompile::secp256r1::P256VERIFY, "p256-verify"),
    ),
];

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
            OpSpecId::ISTHMUS | OpSpecId::INTEROP | OpSpecId::OSAKA => isthmus().clone(),
        };
        let mut precompiles_owned = precompiles.clone();
        precompiles_owned.extend(PRECOMPILES.iter().map(|p| p.1.clone()).take(1));
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
        _context: &mut CTX,
        address: &Address,
        inputs: &InputsImpl,
        _is_static: bool,
        gas_limit: u64,
    ) -> Result<Option<Self::Output>, String> {
        let mut result = InterpreterResult {
            result: InstructionResult::Return,
            gas: Gas::new(gas_limit),
            output: Bytes::new(),
        };

        // Priority:
        // 1. If the precompile has an accelerated version, use that.
        // 2. If the precompile is not accelerated, use the default version.
        // 3. If the precompile is not found, return None.
        let output = if let Some(precompile) = self.inner.precompiles.get(address) {
            (*precompile)(&inputs.input, gas_limit)
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
