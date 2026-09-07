use crate::{
    EvmConfig, EvmTypesHost,
    interpreter::{InterpreterState, Pc, Stack, gas::Gas},
};

pub(super) type LoopState = ();

/// Unpacked instruction return value.
type InstrFnRet = (Pc, usize);

/// Unpacked instruction function pointer.
pub(in crate::interpreter::dispatch) type RawInstrFn<T> = extern_table!(
    fn(pc: Pc, stack: Stack<'_>, state: &mut InterpreterState<'_, '_, T>) -> InstrFnRet
);

#[inline(always)]
pub(super) fn dispatch_loop_call<T: EvmTypesHost>(
    instr: RawInstrFn<T>,
    pc: Pc,
    stack: Stack<'_>,
    state: &mut InterpreterState<'_, '_, T>,
    _loop_state: &mut LoopState,
) -> (Pc, usize) {
    instr(pc, stack, state)
}

#[inline(always)]
pub(super) const fn loop_state(_gas: &Gas) -> LoopState {}

#[inline(always)]
pub(super) const fn finish_loop(_gas: &mut Gas, _loop_state: LoopState) {}

#[inline(always)]
pub(super) const fn sync_loop_state<T: EvmTypesHost>(
    _state: &mut InterpreterState<'_, '_, T>,
    _loop_state: LoopState,
) {
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
        state: &mut InterpreterState<'_, '_, T>,
    ) -> InstrFnRet {
        let (pc, ()) = super::dispatch_inner::<T, C, M, ()>(pc, stack.as_mut(), (), state, OP);
        (pc, stack.len)
    }
}
