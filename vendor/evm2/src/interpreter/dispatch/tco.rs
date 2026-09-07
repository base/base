use core::hint::cold_path;

use super::{InspectMode, run_state};
use crate::{
    EvmConfig, EvmTypesHost,
    interpreter::{
        InstrStop, Interpreter, InterpreterState, Pc, Result, Stack, gas::RemainingGas,
        private::InstructionImplFn,
    },
};

/// Tail instruction function pointer.
type TailInstrFn<T> = extern_table!(
    fn(
        pc: Pc,
        stack: Stack<'_>,
        remaining_gas: RemainingGas,
        state: &mut InterpreterState<'_, '_, T>,
        instructions: *const (),
    )
);

/// Tail instruction dispatch table.
type TailInstrTable<T> = [TailInstrFn<T>; 256];

pub(super) type RawInstrFn<T> = TailInstrFn<T>;

pub(super) type RawInstrTable<T> = TailInstrTable<T>;

#[inline(always)]
pub(in crate::interpreter) fn run<T: EvmTypesHost>(
    interpreter: &mut Interpreter<'_, '_, T>,
    instructions: &RawInstrTable<T>,
) -> InstrStop {
    let remaining_gas = RemainingGas::new(interpreter.gas.remaining());
    let (state, pc, stack) = run_state(interpreter);
    let op = pc.op();
    let instr = instructions[op as usize];
    instr(pc, stack, remaining_gas, state, (instructions as *const RawInstrTable<T>).cast());
    state.result().unwrap_err()
}

extern_table! {
    pub(super) fn dispatch<
        T: EvmTypesHost,
        C: EvmConfig<T>,
        M: InspectMode<T>,
        const OP: u8,
    >(
        mut pc: Pc,
        mut stack: Stack<'_>,
        mut remaining_gas: RemainingGas,
        state: &mut InterpreterState<'_, '_, T>,
        instructions: *const (),
    ) {
        let instruction = C::OPCODE_CONFIG.instruction(OP);
        let instr: InstructionImplFn<T> = instruction.instr;
        let dynamic_gas = instruction.dynamic_gas;
        if M::INSPECT {
            M::step(state, pc, stack.len);
            if state.result().is_err() {
                cold_path();
                tail_return!(tail_call_restore(pc, stack, remaining_gas, state, instructions));
            }
        }
        if let Err(e) = pre_step::<T, C>(&mut remaining_gas, OP) {
            cold_path();
            state.set_result(Err(e));
            if M::INSPECT {
                state.gas_mut().set_remaining(remaining_gas.get());
                M::step_end(state, pc, stack.len);
            }
            tail_return!(tail_call_restore(pc, stack, remaining_gas, state, instructions));
        }
        if M::INSPECT || dynamic_gas {
            state.gas_mut().set_remaining(remaining_gas.get());
        }
        let r = instr(&mut pc, stack.as_mut(), state);
        if dynamic_gas {
            remaining_gas.set(state.gas_mut().remaining());
        }
        if let Err(e) = r {
            cold_path();
            state.set_result(Err(e));
            if M::INSPECT {
                M::step_end(state, pc, stack.len);
            }
            tail_return!(tail_call_restore(pc, stack, remaining_gas, state, instructions));
        }
        super::inc_pc(&mut pc, OP);
        if M::INSPECT {
            M::step_end(state, pc, stack.len);
            if state.result().is_err() {
                cold_path();
                tail_return!(tail_call_restore(pc, stack, remaining_gas, state, instructions));
            }
        }
        // SAFETY: `instructions` is a pointer to a `TailInstrTable`.
        let instructions_t = unsafe { &*instructions.cast::<TailInstrTable<T>>() };
        let instr = instructions_t[pc.op() as usize];
        tail_return!(instr(pc, stack, remaining_gas, state, instructions));
    }
}

extern_table! {
    #[inline(never)]
    #[cold]
    fn tail_call_restore<T: EvmTypesHost>(
        pc: Pc,
        stack: Stack<'_>,
        remaining_gas: RemainingGas,
        state: &mut InterpreterState<'_, '_, T>,
        _instructions: *const (),
    ) {
        state.gas_mut().set_remaining(remaining_gas.get());
        state.set_pc_stack_len(pc.as_ptr(), stack.len);
        debug_assert!(state.result().is_err());
        // Exits by returning normally.
    }
}

#[inline(always)]
const fn pre_step<T: EvmTypesHost, C: EvmConfig<T>>(
    remaining_gas: &mut RemainingGas,
    opcode: u8,
) -> Result {
    remaining_gas.spend(C::OPCODE_CONFIG.static_gas(opcode) as _)
}
