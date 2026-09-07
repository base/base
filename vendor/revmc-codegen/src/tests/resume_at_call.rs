// Tests for resume_at persistence in call_with_interpreter (49274dbb).
//
// When JIT code encounters a CALL instruction, it suspends execution and
// returns `InterpreterAction::NewFrame`. After the callee completes, the
// caller must resume from the instruction *after* the CALL — not from PC=0.
//
// Without the fix, `call_with_interpreter` did not persist `ecx.resume_at`
// back into the interpreter's bytecode PC, causing re-execution from the
// beginning on every re-entry.

use revm_bytecode::opcode as op;
use revm_interpreter::{
    CallInput, FrameInput, Gas, InputsImpl, InstructionResult, Interpreter, InterpreterAction,
    InterpreterResult, SharedMemory, interpreter::ExtBytecode,
};
use revm_primitives::{Bytes, U256};

use super::{
    DEF_ADDR, DEF_CALLER, DEF_CD, DEF_GAS_LIMIT, DEF_SPEC, DEF_VALUE, TestHost,
    insert_call_outcome_test,
};
use crate::{Backend, EvmCompiler};

matrix_tests!(call_then_push = |compiler| run_call_then_push(compiler));
matrix_tests!(call_then_return = |compiler| run_call_then_return(compiler));
matrix_tests!(call_returndatasize = |compiler| run_call_returndatasize(compiler));
matrix_tests!(
    call_pop_push_sload_stack_len = |compiler| run_call_pop_push_sload_stack_len(compiler)
);

/// Contract: PUSH args → CALL → PUSH1 0x42 → STOP
///
/// After the CALL returns, execution must continue with PUSH1 0x42 (not restart
/// from PUSH args). We verify by checking the stack after the second call to
/// `call_with_interpreter`.
fn run_call_then_push<B: Backend>(compiler: &mut EvmCompiler<B>) {
    #[rustfmt::skip]
    let bytecode: &[u8] = &[
        // Set up CALL arguments (7 stack items)
        op::PUSH1, 0,    // ret length
        op::PUSH1, 0,    // ret offset
        op::PUSH1, 0,    // args length
        op::PUSH1, 0,    // args offset
        op::PUSH1, 0,    // value = 0
        op::PUSH1, 0x69, // address
        op::GAS,         // gas (all remaining)
        op::CALL,        // suspends here with NewFrame
        // -- after CALL returns, execution resumes here --
        op::PUSH1, 0x42, // push marker value
        op::STOP,
    ];

    unsafe { compiler.clear() }.unwrap();
    compiler.inspect_stack(true);
    let f = unsafe { compiler.jit("resume_call", bytecode, DEF_SPEC) }.unwrap();

    // First call: should suspend at CALL with NewFrame
    let mut host = TestHost::new();
    let input = InputsImpl {
        target_address: DEF_ADDR,
        bytecode_address: None,
        caller_address: DEF_CALLER,
        input: CallInput::Bytes(Bytes::from_static(DEF_CD)),
        call_value: DEF_VALUE,
    };
    let bytecode_obj = revm_bytecode::Bytecode::new_raw(Bytes::copy_from_slice(bytecode));
    let ext_bytecode = ExtBytecode::new(bytecode_obj);
    let mut interpreter =
        Interpreter::new(SharedMemory::new(), ext_bytecode, input, false, DEF_SPEC, DEF_GAS_LIMIT);

    let action = unsafe { f.call_with_interpreter(&mut interpreter, &mut host) };

    // Should get NewFrame(Call(...))
    let return_memory_offset = match &action {
        InterpreterAction::NewFrame(FrameInput::Call(call_inputs)) => {
            Some(call_inputs.return_memory_offset.clone())
        }
        other => panic!("expected NewFrame(Call), got {other:?}"),
    };

    // Simulate the callee completing successfully with no output
    let call_result = InterpreterResult {
        result: InstructionResult::Stop,
        output: Bytes::new(),
        gas: Gas::new(0),
    };
    insert_call_outcome_test(&mut interpreter, call_result, return_memory_offset);

    // Second call: should resume after CALL, execute PUSH1 0x42, STOP
    let action = unsafe { f.call_with_interpreter(&mut interpreter, &mut host) };

    match &action {
        InterpreterAction::Return(result) => {
            assert_eq!(
                result.result,
                InstructionResult::Stop,
                "expected Stop after resume, got {:?}",
                result.result
            );
            // Stack should have: [call_success_indicator(1), 0x42]
            // The CALL pushed success=1, then PUSH1 0x42
            assert_eq!(interpreter.stack.len(), 2, "stack should have 2 items");
            assert_eq!(
                interpreter.stack.data()[1],
                U256::from(0x42),
                "top of stack should be 0x42 (the marker value pushed after CALL)"
            );
        }
        other => panic!("expected Return after resume, got {other:?}"),
    }
}

/// Contract: PUSH args → CALL → PUSH1 32 → PUSH0 → RETURN
///
/// After the CALL returns, execution should RETURN 32 zero bytes.
/// Without the resume_at fix, it would re-enter from the beginning and try
/// to CALL again, never reaching the RETURN.
fn run_call_then_return<B: Backend>(compiler: &mut EvmCompiler<B>) {
    #[rustfmt::skip]
    let bytecode: &[u8] = &[
        // CALL arguments
        op::PUSH1, 0,    // ret length
        op::PUSH1, 0,    // ret offset
        op::PUSH1, 0,    // args length
        op::PUSH1, 0,    // args offset
        op::PUSH1, 0,    // value = 0
        op::PUSH1, 0x69, // address
        op::GAS,         // gas
        op::CALL,        // suspends
        // -- resume point --
        op::POP,          // pop call success indicator
        op::PUSH1, 32,   // return size
        op::PUSH0,       // return offset
        op::RETURN,       // should return 32 zero bytes
    ];

    unsafe { compiler.clear() }.unwrap();
    compiler.inspect_stack(true);
    let f = unsafe { compiler.jit("resume_return", bytecode, DEF_SPEC) }.unwrap();

    let mut host = TestHost::new();
    let input = InputsImpl {
        target_address: DEF_ADDR,
        bytecode_address: None,
        caller_address: DEF_CALLER,
        input: CallInput::Bytes(Bytes::from_static(DEF_CD)),
        call_value: DEF_VALUE,
    };
    let bytecode_obj = revm_bytecode::Bytecode::new_raw(Bytes::copy_from_slice(bytecode));
    let ext_bytecode = ExtBytecode::new(bytecode_obj);
    let mut interpreter =
        Interpreter::new(SharedMemory::new(), ext_bytecode, input, false, DEF_SPEC, DEF_GAS_LIMIT);

    // First call: suspends at CALL
    let action = unsafe { f.call_with_interpreter(&mut interpreter, &mut host) };
    let return_memory_offset = match &action {
        InterpreterAction::NewFrame(FrameInput::Call(call_inputs)) => {
            Some(call_inputs.return_memory_offset.clone())
        }
        other => panic!("expected NewFrame(Call), got {other:?}"),
    };

    // Simulate callee returning
    let call_result = InterpreterResult {
        result: InstructionResult::Stop,
        output: Bytes::new(),
        gas: Gas::new(0),
    };
    insert_call_outcome_test(&mut interpreter, call_result, return_memory_offset);

    // Second call: should resume, POP, PUSH1 32, PUSH0, RETURN
    let action = unsafe { f.call_with_interpreter(&mut interpreter, &mut host) };

    match &action {
        InterpreterAction::Return(result) => {
            assert_eq!(result.result, InstructionResult::Return);
            assert_eq!(result.output.len(), 32, "expected 32-byte return output");
        }
        other => panic!("expected Return after resume, got {other:?}"),
    }
}

/// Contract: CALL → RETURNDATASIZE → PUSH0 → MSTORE → PUSH1 32 → PUSH0 → RETURN
///
/// After the CALL returns with 7 bytes of data, RETURNDATASIZE must reflect
/// the actual return data length (7). The value is stored to memory and returned.
///
/// This verifies that DSE does NOT eliminate RETURNDATASIZE when its output is
/// live, and that the builtin correctly reads return_data after suspend/resume.
fn run_call_returndatasize<B: Backend>(compiler: &mut EvmCompiler<B>) {
    #[rustfmt::skip]
    let bytecode: &[u8] = &[
        // CALL arguments
        op::PUSH1, 0,    // ret length
        op::PUSH1, 0,    // ret offset
        op::PUSH1, 0,    // args length
        op::PUSH1, 0,    // args offset
        op::PUSH1, 0,    // value = 0
        op::PUSH1, 0x69, // address
        op::GAS,         // gas
        op::CALL,        // suspends
        // -- resume point --
        op::POP,              // pop call success
        op::RETURNDATASIZE,   // push return data length
        op::PUSH0,            // dest offset
        op::MSTORE,           // store to memory
        op::PUSH1, 32,        // return size
        op::PUSH0,            // return offset
        op::RETURN,
    ];

    unsafe { compiler.clear() }.unwrap();
    compiler.inspect_stack(true);
    let f = unsafe { compiler.jit("resume_rds", bytecode, DEF_SPEC) }.unwrap();

    let mut host = TestHost::new();
    let input = InputsImpl {
        target_address: DEF_ADDR,
        bytecode_address: None,
        caller_address: DEF_CALLER,
        input: CallInput::Bytes(Bytes::from_static(DEF_CD)),
        call_value: DEF_VALUE,
    };
    let bytecode_obj = revm_bytecode::Bytecode::new_raw(Bytes::copy_from_slice(bytecode));
    let ext_bytecode = ExtBytecode::new(bytecode_obj);
    let mut interpreter =
        Interpreter::new(SharedMemory::new(), ext_bytecode, input, false, DEF_SPEC, DEF_GAS_LIMIT);

    // First call: suspends at CALL
    let action = unsafe { f.call_with_interpreter(&mut interpreter, &mut host) };
    let return_memory_offset = match &action {
        InterpreterAction::NewFrame(FrameInput::Call(call_inputs)) => {
            Some(call_inputs.return_memory_offset.clone())
        }
        other => panic!("expected NewFrame(Call), got {other:?}"),
    };

    // Simulate callee returning 7 bytes of data.
    let return_data = Bytes::from_static(&[0xDE, 0xAD, 0xBE, 0xEF, 0xCA, 0xFE, 0x42]);
    let call_result = InterpreterResult {
        result: InstructionResult::Stop,
        output: return_data,
        gas: Gas::new(0),
    };
    insert_call_outcome_test(&mut interpreter, call_result, return_memory_offset);

    // Second call: resume → POP → RETURNDATASIZE → MSTORE → RETURN
    let action = unsafe { f.call_with_interpreter(&mut interpreter, &mut host) };

    match &action {
        InterpreterAction::Return(result) => {
            assert_eq!(result.result, InstructionResult::Return);
            assert_eq!(result.output.len(), 32, "expected 32-byte return output");
            // Memory word should contain RETURNDATASIZE = 7.
            let value = U256::from_be_slice(&result.output);
            assert_eq!(
                value,
                U256::from(7),
                "RETURNDATASIZE should be 7 after CALL with 7-byte return data"
            );
        }
        other => panic!("expected Return after resume, got {other:?}"),
    }
}

/// Regression test for stale `len.addr` after `POP, PUSH1(noop), SLOAD`.
///
/// When a section contains `POP` (stores `len = start-1`), then a noop `PUSH`
/// (restores `section_len_offset` to 0 without storing), followed by a net-zero
/// builtin like `SLOAD`, the `len.addr` store was skipped because both
/// `section_len_offset` and `diff` were 0. This left `len.addr` stale at
/// `start-1`, causing the next section head to load an incorrect stack length.
///
/// The test pushes a marker value (0xBEEF) below the CALL arguments. After
/// resume, the sequence `POP, PUSH1 5, SLOAD` triggers the bug, then a
/// `JUMP → JUMPDEST` creates a new section that reloads `len.addr`. With the
/// bug, the stack length is off by 1, causing the marker to be read from the
/// wrong position.
fn run_call_pop_push_sload_stack_len<B: Backend>(compiler: &mut EvmCompiler<B>) {
    // Trigger: `POP, PUSH1(noop), SLOAD` in the resume section, where SLOAD
    // is the last non-noop instruction before a JUMPDEST section head.
    //
    // POP stores `len.addr = start - 1`. The noop PUSH1 resets
    // `section_len_offset` to 0. SLOAD (net-zero) then sees `diff == 0 &&
    // section_len_offset == 0` and skips the `len.addr` store, leaving it
    // stale at `start - 1`. The JUMPDEST loads the stale value and
    // misaligns all subsequent stack accesses.
    //
    // The JUMPDEST at pc=21 is made a reachable jump target (and therefore a
    // section head) via a JUMP at pc=36 in unreachable code after RETURN.
    const JUMPDEST_PC: u8 = 21;
    #[rustfmt::skip]
    let bytecode: &[u8] = &[
        // Push marker below CALL args.
        op::PUSH2, 0xBE, 0xEF,          // pc=0
        // CALL arguments (7 stack items).
        op::PUSH1, 0,                    // pc=3: ret length
        op::PUSH1, 0,                    // pc=5: ret offset
        op::PUSH1, 0,                    // pc=7: args length
        op::PUSH1, 0,                    // pc=9: args offset
        op::PUSH1, 0,                    // pc=11: value
        op::PUSH1, 0x69,                 // pc=13: address
        op::GAS,                         // pc=15: gas
        op::CALL,                        // pc=16: suspends
        // -- resume section --
        op::POP,                         // pc=17: pop call result
        op::PUSH1, 0x05,                 // pc=18: noop (SLOAD key)
        op::SLOAD,                       // pc=20: net-0 builtin → falls through
        // -- new section head (also targeted by JUMP at pc=35) --
        op::JUMPDEST,                    // pc=21: reloads stale len.addr
        // Stack should be [marker, sload_result].
        op::POP,                         // pc=22: pop sload_result
        // Stack: [marker]
        op::PUSH0,                       // pc=23: offset = 0
        op::MSTORE,                      // pc=24: mem[0..32] = marker
        op::PUSH1, 32,                   // pc=25: return size
        op::PUSH0,                       // pc=27: return offset
        op::RETURN,                      // pc=28
        // Unreachable: a JUMP that targets the JUMPDEST, making it a
        // reachable jump target in the CFG even though this path is dead.
        op::JUMPDEST,                    // pc=29: prevents "no valid predecessor" pruning
        op::PUSH1, 0,                    // pc=30: dummy push (for stack depth ≥ 2)
        op::PUSH1, 0,                    // pc=32: dummy push
        op::PUSH1, JUMPDEST_PC,          // pc=34
        op::JUMP,                        // pc=36: targets pc=21
    ];

    unsafe { compiler.clear() }.unwrap();
    compiler.inspect_stack(true);
    let f = unsafe { compiler.jit("pop_push_sload", bytecode, DEF_SPEC) }.unwrap();

    let mut host = TestHost::new();
    let input = InputsImpl {
        target_address: DEF_ADDR,
        bytecode_address: None,
        caller_address: DEF_CALLER,
        input: CallInput::Bytes(Bytes::from_static(DEF_CD)),
        call_value: DEF_VALUE,
    };
    let bytecode_obj = revm_bytecode::Bytecode::new_raw(Bytes::copy_from_slice(bytecode));
    let ext_bytecode = ExtBytecode::new(bytecode_obj);
    let mut interpreter =
        Interpreter::new(SharedMemory::new(), ext_bytecode, input, false, DEF_SPEC, DEF_GAS_LIMIT);

    // First call: suspends at CALL.
    let action = unsafe { f.call_with_interpreter(&mut interpreter, &mut host) };
    let return_memory_offset = match &action {
        InterpreterAction::NewFrame(FrameInput::Call(call_inputs)) => {
            Some(call_inputs.return_memory_offset.clone())
        }
        other => panic!("expected NewFrame(Call), got {other:?}"),
    };

    // Simulate callee returning successfully.
    let call_result = InterpreterResult {
        result: InstructionResult::Stop,
        output: Bytes::new(),
        gas: Gas::new(0),
    };
    insert_call_outcome_test(&mut interpreter, call_result, return_memory_offset);

    // Second call: resume → POP → PUSH1 → SLOAD → JUMPDEST → POP → MSTORE → RETURN.
    let action = unsafe { f.call_with_interpreter(&mut interpreter, &mut host) };

    match &action {
        InterpreterAction::Return(result) => {
            assert_eq!(
                result.result,
                InstructionResult::Return,
                "expected Return, got {:?}",
                result.result
            );
            assert_eq!(result.output.len(), 32, "expected 32-byte return output");
            let value = U256::from_be_slice(&result.output);
            assert_eq!(
                value,
                U256::from(0xBEEF),
                "returned value should be the marker 0xBEEF; \
                 stale len.addr causes the JUMPDEST section to misalign the stack"
            );
        }
        other => panic!("expected Return after resume, got {other:?}"),
    }
}
