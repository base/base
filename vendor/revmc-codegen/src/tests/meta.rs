use revm_bytecode::opcode as op;
use revm_interpreter::InstructionResult;
use revm_primitives::{U256, hardfork::SpecId};

use super::with_evm_context;
use crate::{Backend, EvmCompiler};

// Also tests multiple functions in the same module.
matrix_tests!(
    translate_then_compile = |compiler| {
        let bytecode: &[u8] = &[];
        let spec_id = SpecId::CANCUN;
        compiler.gas_metering(false);
        let gas_id = compiler.translate("test1", bytecode, spec_id).unwrap();
        compiler.gas_metering(true);
        let no_gas_id = compiler.translate("test2", bytecode, spec_id).unwrap();
        let gas_fn = unsafe { compiler.jit_function(gas_id) }.unwrap();
        let no_gas_fn = unsafe { compiler.jit_function(no_gas_id) }.unwrap();
        with_evm_context(bytecode, spec_id, |ecx, stack, stack_len| {
            let r = unsafe { gas_fn.call(stack, stack_len, ecx) };
            assert_eq!(r, InstructionResult::Stop);
            let r = unsafe { no_gas_fn.call(stack, stack_len, ecx) };
            assert_eq!(r, InstructionResult::Stop);
        });
    }
);

// Tests that calling `clear_ir` between two compilations works: the first function remains
// callable after the IR is cleared, and the second function compiles and runs correctly.
matrix_tests!(
    clear_ir_between_compiles = |compiler| {
        use revm_bytecode::opcode as op;

        let spec_id = SpecId::CANCUN;
        compiler.inspect_stack(true);

        // First function: PUSH1 42, STOP.
        let bytecode1: &[u8] = &[op::PUSH1, 42];
        let f1 = unsafe { compiler.jit("clear_ir_1", bytecode1, spec_id) }.unwrap();

        compiler.clear_ir().unwrap();

        // Second function: PUSH1 42, PUSH1 0, MSTORE, PUSH1 1, PUSH1 2, ADD, STOP.
        // Uses MSTORE to exercise a builtin being re-declared in the new module.
        let bytecode2: &[u8] =
            &[op::PUSH1, 42, op::PUSH1, 0, op::MSTORE, op::PUSH1, 1, op::PUSH1, 2, op::ADD];
        let f2 = unsafe { compiler.jit("clear_ir_2", bytecode2, spec_id) }.unwrap();

        // First function still works after clear_ir + second compilation.
        with_evm_context(bytecode1, spec_id, |ecx, stack, stack_len| {
            let r = unsafe { f1.call(stack, stack_len, ecx) };
            assert_eq!(r, InstructionResult::Stop);
            assert_eq!(*stack_len, 1);
            assert_eq!(unsafe { stack.as_slice(*stack_len) }[0].to_u256(), U256::from(42));
        });

        // Second function works.
        with_evm_context(bytecode2, spec_id, |ecx, stack, stack_len| {
            let r = unsafe { f2.call(stack, stack_len, ecx) };
            assert_eq!(r, InstructionResult::Stop);
            assert_eq!(*stack_len, 1);
            assert_eq!(unsafe { stack.as_slice(*stack_len) }[0].to_u256(), U256::from(3));
        });
    }
);

/// Simple bytecode: `PUSH1 <value>, STOP`.
fn push_stop(value: u8) -> [u8; 3] {
    [op::PUSH1, value, op::STOP]
}

/// JIT-compile, call, and verify a PUSH1+STOP function returns the expected value.
fn jit_and_verify<B: Backend>(
    compiler: &mut EvmCompiler<B>,
    name: &str,
    code: &[u8],
    expected: U256,
) -> B::FuncId {
    compiler.inspect_stack(true);
    let id = compiler.translate(name, code, super::DEF_SPEC).unwrap();
    let f = unsafe { compiler.jit_function(id) }.unwrap();

    with_evm_context(code, super::DEF_SPEC, |ecx, stack, stack_len| {
        let r = unsafe { f.call(stack, stack_len, ecx) };
        assert_eq!(r, InstructionResult::Stop, "{name}: unexpected return");
        assert_eq!(*stack_len, 1, "{name}: expected 1 stack element");
        assert_eq!(
            unsafe { stack.as_slice(*stack_len) }[0].to_u256(),
            expected,
            "{name}: wrong value"
        );
    });
    id
}

// Free a single committed function, then compile and run a new one.
matrix_tests!(
    free_single = |compiler| {
        let code = push_stop(0x42);
        let id = jit_and_verify(compiler, "f1", &code, U256::from(0x42));

        unsafe { compiler.free_function(id) }.unwrap();

        // Compile a new function after freeing — the module should still be usable.
        compiler.clear_ir().unwrap();
        let code2 = push_stop(0x69);
        jit_and_verify(compiler, "f2", &code2, U256::from(0x69));
    }
);

// Free all functions via `clear`, then compile new ones.
matrix_tests!(
    free_all = |compiler| {
        let code = push_stop(0x10);
        jit_and_verify(compiler, "g1", &code, U256::from(0x10));

        unsafe { compiler.clear() }.unwrap();

        let code2 = push_stop(0x20);
        jit_and_verify(compiler, "g2", &code2, U256::from(0x20));
    }
);

// Repeated clear/recompile cycles. Regression test for resource tracker UAF:
// `JITDylib::clear` must run before dropping `ResourceTracker` handles.
matrix_tests!(
    repeated_clear_recompile = |compiler| {
        for i in 0..1000u32 {
            let code = push_stop((i & 0xFF) as u8);
            jit_and_verify(compiler, "f", &code, U256::from(i & 0xFF));
            unsafe { compiler.clear() }.unwrap();
        }
    }
);

// Free one function, then free all remaining via `clear`.
matrix_tests!(
    free_single_then_clear = |compiler| {
        let code_a = push_stop(0xAA);
        let id_a = jit_and_verify(compiler, "h1", &code_a, U256::from(0xAA));

        compiler.clear_ir().unwrap();
        let code_b = push_stop(0xBB);
        jit_and_verify(compiler, "h2", &code_b, U256::from(0xBB));

        // Free only the first function.
        unsafe { compiler.free_function(id_a) }.unwrap();

        // Clear everything.
        unsafe { compiler.clear() }.unwrap();

        // Compile again.
        let code_c = push_stop(0xCC);
        jit_and_verify(compiler, "h3", &code_c, U256::from(0xCC));
    }
);

// Compile multiple functions, free them individually.
matrix_tests!(
    free_multiple_individually = |compiler| {
        let code_a = push_stop(0x11);
        let id_a = jit_and_verify(compiler, "m1", &code_a, U256::from(0x11));

        compiler.clear_ir().unwrap();
        let code_b = push_stop(0x22);
        let id_b = jit_and_verify(compiler, "m2", &code_b, U256::from(0x22));

        // Free both.
        unsafe { compiler.free_function(id_a) }.unwrap();
        unsafe { compiler.free_function(id_b) }.unwrap();

        // Compile again.
        compiler.clear_ir().unwrap();
        let code_c = push_stop(0x33);
        jit_and_verify(compiler, "m3", &code_c, U256::from(0x33));
    }
);

// Verify that jit_memory_usage() reflects live JIT code: non-zero after compilation,
// decreasing after free_function, and back to baseline after freeing all.
#[cfg(feature = "llvm")]
#[test]
fn jit_memory_usage_tracking() {
    use revmc_llvm::jit_memory_usage;

    use super::with_jit_compiler;

    with_jit_compiler(revmc_backend::OptimizationLevel::default(), |compiler| {
        let baseline = jit_memory_usage().map(|u| u.total_bytes()).unwrap_or(0);

        let code_a = push_stop(0x42);
        let id_a = jit_and_verify(compiler, "mem_a", &code_a, U256::from(0x42));
        let after_a = jit_memory_usage().unwrap().total_bytes();
        assert!(after_a > baseline, "expected memory increase after first compile");

        compiler.clear_ir().unwrap();
        let code_b = push_stop(0x69);
        let id_b = jit_and_verify(compiler, "mem_b", &code_b, U256::from(0x69));
        let after_b = jit_memory_usage().unwrap().total_bytes();
        assert!(after_b > after_a, "expected memory increase after second compile");

        unsafe { compiler.free_function(id_a) }.unwrap();
        let after_free_a = jit_memory_usage().unwrap().total_bytes();
        assert!(after_free_a < after_b, "expected memory decrease after freeing first function");

        unsafe { compiler.free_function(id_b) }.unwrap();
        let after_free_b = jit_memory_usage().unwrap().total_bytes();
        assert!(
            after_free_b <= after_free_a,
            "expected memory decrease after freeing second function"
        );
    });
}

// Reuse the same symbol name after freeing. If ORC resources weren't actually
// removed, re-defining the symbol would fail with a duplicate symbol error.
matrix_tests!(
    free_reuse_name = |compiler| {
        let code_a = push_stop(0x42);
        let id = jit_and_verify(compiler, "reuse", &code_a, U256::from(0x42));

        unsafe { compiler.free_function(id) }.unwrap();
        compiler.clear_ir().unwrap();

        // Same name, different value.
        let code_b = push_stop(0x69);
        jit_and_verify(compiler, "reuse", &code_b, U256::from(0x69));
    }
);
