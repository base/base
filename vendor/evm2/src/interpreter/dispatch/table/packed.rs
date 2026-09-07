use crate::{
    EvmConfig, EvmTypesHost,
    constants::STACK_LIMIT,
    interpreter::{
        InterpreterState, Pc, Result, Stack,
        gas::{Gas, RemainingGas},
    },
};

pub(super) type LoopState = RemainingGas;

/// Packed instruction return value.
pub(crate) type InstrFnRet = (PackedPc, u64);

/// Packed instruction function pointer.
pub(in crate::interpreter::dispatch) type RawInstrFn<T> = extern_table!(
    fn(
        pc: Pc,
        stack: Stack<'_>,
        remaining_gas: RemainingGas,
        state: &mut InterpreterState<'_, '_, T>,
    ) -> InstrFnRet
);

#[inline(always)]
pub(super) fn dispatch_loop_call<T: EvmTypesHost>(
    instr: RawInstrFn<T>,
    pc: Pc,
    stack: Stack<'_>,
    state: &mut InterpreterState<'_, '_, T>,
    remaining_gas: &mut LoopState,
) -> (Pc, usize) {
    let (next_pc, gas_spent) = instr(pc, stack, *remaining_gas, state);
    *remaining_gas = RemainingGas::new(remaining_gas.get().wrapping_sub(gas_spent));
    next_pc.unpack()
}

#[inline(always)]
pub(super) const fn loop_state(gas: &Gas) -> LoopState {
    RemainingGas::new(gas.remaining())
}

#[inline(always)]
pub(super) const fn finish_loop(gas: &mut Gas, remaining_gas: LoopState) {
    gas.set_remaining(remaining_gas.get());
}

#[inline(always)]
pub(super) const fn sync_loop_state<T: EvmTypesHost>(
    state: &mut InterpreterState<'_, '_, T>,
    loop_state: LoopState,
) {
    state.gas_mut().set_remaining(loop_state.get());
}

impl super::DispatchGas for RemainingGas {
    #[inline(always)]
    fn pre_step<T: EvmTypesHost, C: EvmConfig<T>>(
        &mut self,
        _state: &mut InterpreterState<'_, '_, T>,
        op: u8,
    ) -> Result {
        self.spend(C::OPCODE_CONFIG.static_gas(op) as _)
    }

    #[inline(always)]
    fn sync_before_exec<T: EvmTypesHost>(
        &self,
        state: &mut InterpreterState<'_, '_, T>,
        dynamic_gas: bool,
    ) {
        if dynamic_gas {
            state.gas_mut().set_remaining(self.get());
        }
    }

    #[inline(always)]
    fn sync_after_exec<T: EvmTypesHost>(
        &mut self,
        state: &mut InterpreterState<'_, '_, T>,
        dynamic_gas: bool,
    ) {
        if dynamic_gas {
            self.set(state.gas_mut().remaining());
        }
    }
}

extern_table! {
    pub(in crate::interpreter::dispatch) fn dispatch<
        T: EvmTypesHost,
        C: EvmConfig<T>,
        M: super::InspectMode<T>,
        const OP: u8,
    >(
        pc: Pc,
        mut stack: Stack<'_>,
        remaining_gas: RemainingGas,
        state: &mut InterpreterState<'_, '_, T>,
    ) -> InstrFnRet {
        let initial_remaining_gas = remaining_gas;
        let (pc, remaining_gas) =
            super::dispatch_inner::<T, C, M, RemainingGas>(
                pc,
                stack.as_mut(),
                remaining_gas,
                state,
                OP,
            );
        (
            PackedPc::new(pc, stack.len),
            initial_remaining_gas.get().wrapping_sub(remaining_gas.get()),
        )
    }
}

const STACK_LEN_BITS: u32 = STACK_LIMIT.ilog2() + 1;
const STACK_LEN_SHIFT: u32 = usize::BITS - STACK_LEN_BITS;
const PC_MASK: usize = (1 << STACK_LEN_SHIFT) - 1;

const _: () = assert!(usize::BITS == 64);
const _: () = assert!(STACK_LIMIT < (1 << STACK_LEN_BITS));

/// Program counter with stack length packed into its upper bits.
#[derive(Clone, Copy)]
#[repr(transparent)]
pub(crate) struct PackedPc(usize);

impl PackedPc {
    #[inline(always)]
    fn new(pc: Pc, stack_len: usize) -> Self {
        Self((stack_len << STACK_LEN_SHIFT) | pc.as_ptr() as usize)
    }

    #[inline(always)]
    const fn unpack(self) -> (Pc, usize) {
        let pc = (self.0 & PC_MASK) as *const u8;
        (Pc::new(pc), self.0 >> STACK_LEN_SHIFT)
    }
}
