//! Inspector is a crate that provides a set of traits that allow inspecting the EVM execution.
//!
//! It is used to implement tracers that can be used to inspect the EVM execution.
//! Implementing inspection is optional and it does not effect the core execution.
#![cfg_attr(not(test), warn(unused_crate_dependencies))]
#![cfg_attr(not(feature = "std"), no_std)]

mod count_inspector;
#[cfg(feature = "tracer")]
mod eip3155;
#[cfg(feature = "tracer")]
pub use eip3155::CloneStack;
mod either;
mod gas;
/// Handler implementations for inspector integration.
pub mod handler;
mod inspect;
mod inspector;
mod mainnet_inspect;
mod noop;
/// Test inspector for testing EVM execution.
pub mod test_inspector;
mod traits;

#[cfg(test)]
mod inspector_tests;

/// Inspector implementations.
pub mod inspectors {
    #[cfg(feature = "tracer")]
    pub use super::eip3155::TracerEip3155;
    pub use super::gas::GasInspector;
}

pub use count_inspector::CountInspector;
pub use handler::{InspectorHandler, inspect_instructions};
pub use inspect::{InspectCommitEvm, InspectEvm, InspectSystemCallEvm};
pub use inspector::*;
pub use noop::NoOpInspector;
pub use revm_context as context;
pub use revm_database_interface as database_interface;
pub use revm_handler as evm_handler;
pub use revm_interpreter as interpreter;
pub use revm_primitives as primitives;
pub use revm_state as state;
pub use test_inspector::{InspectorEvent, InterpreterState, StepRecord, TestInspector};
pub use traits::*;

#[cfg(test)]
mod tests {
    use ::revm_handler::{MainBuilder, MainContext};
    use revm_context::{BlockEnv, CfgEnv, Context, Journal, TxEnv};
    use revm_database::{BENCH_CALLER, BENCH_TARGET, BenchmarkDB};
    use revm_interpreter::{InstructionResult, InterpreterTypes, interpreter::EthInterpreter};
    use revm_primitives::TxKind;
    use revm_state::{Bytecode, bytecode::opcode};

    use super::*;

    struct HaltInspector;
    impl<CTX, INTR: InterpreterTypes> Inspector<CTX, INTR> for HaltInspector {
        fn step(&mut self, interp: &mut revm_interpreter::Interpreter<INTR>, _context: &mut CTX) {
            interp.halt(InstructionResult::Stop);
        }
    }

    #[test]
    fn test_step_halt() {
        let bytecode = [opcode::INVALID];
        let r = run(&bytecode, HaltInspector);
        assert!(r.is_success());
    }

    fn run(
        bytecode: &[u8],
        inspector: impl Inspector<
            Context<BlockEnv, TxEnv, CfgEnv, BenchmarkDB, Journal<BenchmarkDB>, ()>,
            EthInterpreter,
        >,
    ) -> revm_context::result::ExecutionResult {
        let bytecode = Bytecode::new_raw(bytecode.to_vec().into());
        let ctx = Context::mainnet().with_db(BenchmarkDB::new_bytecode(bytecode));
        let mut evm = ctx.build_mainnet_with_inspector(inspector);
        evm.inspect_one_tx(
            TxEnv::builder()
                .caller(BENCH_CALLER)
                .kind(TxKind::Call(BENCH_TARGET))
                .gas_limit(21100)
                .build()
                .unwrap(),
        )
        .unwrap()
    }
}
