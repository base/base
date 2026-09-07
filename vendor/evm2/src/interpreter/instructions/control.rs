use core::hint::cold_path;

use evm2_macros::instruction;

use crate::{
    EvmTypesHost,
    interpreter::{
        InstrStop, Result, Word,
        private::{GasInstructionCx, InstructionCx},
    },
    utils::{word_to_usize, word_to_usize_saturated},
};

#[instruction]
pub(crate) fn stop() -> Result {
    cold_path();
    Err(InstrStop::Stop)
}

#[instruction]
pub(crate) fn jump(cx: _, [target]: [Word]) -> Result {
    jump_inner(*target, &mut cx)
}

#[instruction]
pub(crate) fn jumpi(cx: _, [target, cond]: [Word]) -> Result {
    if !cond.is_zero() {
        jump_inner(*target, &mut cx)?;
    } else {
        unsafe { cx.pc.advance_unchecked(1) };
    };
}

#[inline(always)]
fn jump_inner<T: EvmTypesHost>(target: Word, cx: &mut InstructionCx<'_, '_, '_, T>) -> Result {
    let target = word_to_usize_saturated(target);
    if !cx.state.bytecode().is_valid_jumpdest(target) {
        cold_path();
        return Err(InstrStop::InvalidJump);
    }
    unsafe { cx.pc.set_unchecked(cx.state.bytecode(), target) };
    Ok(())
}

#[instruction]
pub(crate) fn pc(cx: _) -> out {
    *out = Word::from(cx.state.bytecode().pc_offset(*cx.pc));
}

#[instruction(dynamic_gas)]
pub(crate) fn gas(cx: _) -> out {
    *out = Word::from(cx.gas.remaining());
}

#[instruction]
pub(crate) fn jumpdest() {}

#[instruction(dynamic_gas)]
pub(crate) fn r#return(cx: _, [offset, len]: [Word]) -> Result {
    return_inner(cx, offset, len, InstrStop::Return)
}

#[instruction(dynamic_gas)]
pub(crate) fn revert(cx: _, [offset, len]: [Word]) -> Result {
    return_inner(cx, offset, len, InstrStop::Revert)
}

#[inline]
fn return_inner<T: EvmTypesHost>(
    cx: GasInstructionCx<'_, '_, '_, T>,
    offset: &Word,
    len: &Word,
    result: InstrStop,
) -> Result {
    let len = word_to_usize(*len)?;
    let output = if len != 0 {
        let offset = word_to_usize(*offset)?;
        let Some(end) = offset.checked_add(len) else {
            return Err(InstrStop::MemoryOOG);
        };
        if end > u32::MAX as usize {
            return Err(InstrStop::MemoryLimitOOG);
        }
        cx.state.resize_memory(cx.gas, offset, len)?;
        offset as u32..end as u32
    } else {
        0..0
    };
    cx.state.set_output(output);
    Err(result)
}

#[instruction]
pub(crate) fn invalid() -> Result {
    cold_path();
    Err(InstrStop::InvalidOpcode)
}

#[cfg(test)]
mod tests {
    use alloc::vec::Vec;
    use core::assert_matches;

    use crate::{
        interpreter::{InstrStop, Word, op},
        test_utils::{RunConfig, push, run, run_stack},
    };

    #[test]
    fn stop_opcode() {
        let interp = run(RunConfig::new([op::STOP]));
        assert_matches!(interp.err, InstrStop::Stop);

        let interp = run(RunConfig::new([op::STOP, op::INVALID]));
        assert_matches!(interp.err, InstrStop::Stop);
    }

    #[test]
    fn invalid_opcode() {
        let interp = run(RunConfig::new([op::INVALID]));
        assert_matches!(interp.err, InstrStop::InvalidOpcode);

        let interp = run(RunConfig::new([0x0c]));
        assert_matches!(interp.err, InstrStop::InvalidOpcode);
    }

    #[test]
    fn jump_opcode() {
        let interp = run(RunConfig::new([op::PUSH1, 0x03, op::JUMP, op::JUMPDEST, op::STOP]));
        assert_matches!(interp.err, InstrStop::Stop);

        let interp = run(RunConfig::new([op::PUSH1, 0x00, op::JUMP, op::JUMPDEST, op::STOP]));
        assert_matches!(interp.err, InstrStop::InvalidJump);

        let mut code = Vec::new();
        push(&mut code, Word::MAX);
        code.push(op::JUMP);
        let interp = run(RunConfig::new(code));
        assert_matches!(interp.err, InstrStop::InvalidJump);

        let interp =
            run(RunConfig::new([op::PUSH1, 0x04, op::JUMP, op::STOP, op::JUMPDEST, op::STOP]));
        assert_matches!(interp.err, InstrStop::Stop);
    }

    #[test]
    fn jumpi_opcode() {
        let interp = run(RunConfig::new([
            op::PUSH1,
            0x01,
            op::PUSH1,
            0x06,
            op::JUMPI,
            op::STOP,
            op::JUMPDEST,
            op::STOP,
        ]));
        assert_matches!(interp.err, InstrStop::Stop);

        let interp = run(RunConfig::new([
            op::PUSH1,
            0x00,
            op::PUSH1,
            0x06,
            op::JUMPI,
            op::JUMPDEST,
            op::STOP,
        ]));
        assert_matches!(interp.err, InstrStop::Stop);

        let interp =
            run(RunConfig::new([op::PUSH1, 0x01, op::PUSH1, 0x05, op::JUMPI, op::STOP, op::STOP]));
        assert_matches!(interp.err, InstrStop::InvalidJump);

        let mut code = Vec::new();
        push(&mut code, 1);
        push(&mut code, Word::MAX);
        code.push(op::JUMPI);
        let interp = run(RunConfig::new(code));
        assert_matches!(interp.err, InstrStop::InvalidJump);
    }

    #[test]
    fn pc_opcode() {
        let interp = run(RunConfig::new([op::PC, op::JUMPDEST, op::STOP]));
        assert_matches!(interp.err, InstrStop::Stop);
        assert_eq!(interp.stack(), [0]);

        let interp = run(RunConfig::new([op::JUMPDEST, op::PC, op::STOP]));
        assert_matches!(interp.err, InstrStop::Stop);
        assert_eq!(interp.stack(), [Word::from(1)]);
    }

    #[test]
    fn gas_opcode() {
        let interp = run(RunConfig::new([op::GAS, op::STOP]));
        assert_matches!(interp.err, InstrStop::Stop);
        assert_eq!(interp.stack().len(), 1);
        assert!(interp.stack()[0] < Word::from(10_000));

        let interp = run(RunConfig::new([op::GAS, op::GAS, op::STOP]));
        assert_matches!(interp.err, InstrStop::Stop);
        assert_eq!(interp.stack().len(), 2);
        assert!(interp.stack()[1] < interp.stack()[0]);
    }

    #[test]
    fn jumpdest_opcode() {
        let interp = run(RunConfig::new([op::JUMPDEST, op::STOP]));
        assert_matches!(interp.err, InstrStop::Stop);
        assert!(interp.stack().is_empty());

        let interp = run(RunConfig::new([op::JUMPDEST, op::JUMPDEST, op::STOP]));
        assert_matches!(interp.err, InstrStop::Stop);
    }

    #[test]
    fn return_opcode() {
        let mut interp = run_stack([0, 0], op::RETURN);
        assert_matches!(interp.err, InstrStop::Return);
        assert!(interp.memory(0, 0).is_empty());
        assert!(interp.output().is_empty());

        let mut interp = run_stack([0, 1], op::RETURN);
        assert_matches!(interp.err, InstrStop::Return);
        assert_eq!(interp.memory(0, 1), [0]);
        assert_eq!(interp.output(), [0]);

        let mut code = Vec::new();
        push(&mut code, Word::from(0xab));
        push(&mut code, Word::from(2));
        code.push(op::MSTORE8);
        push(&mut code, Word::from(3));
        push(&mut code, Word::from(2));
        code.push(op::RETURN);
        let interp = run(RunConfig::new(code));
        assert_matches!(interp.err, InstrStop::Return);
        assert_eq!(interp.output(), [0xab, 0, 0]);

        let interp = run_stack([Word::from(0), Word::MAX], op::RETURN);
        assert_matches!(interp.err, InstrStop::InvalidOperandOOG);

        let interp = run_stack([Word::from(u32::MAX), Word::from(1)], op::RETURN);
        assert_matches!(interp.err, InstrStop::MemoryLimitOOG);
    }

    #[test]
    fn revert_opcode() {
        let mut interp = run_stack([0, 0], op::REVERT);
        assert_matches!(interp.err, InstrStop::Revert);
        assert!(interp.memory(0, 0).is_empty());
        assert!(interp.output().is_empty());

        let mut interp = run_stack([2, 3], op::REVERT);
        assert_matches!(interp.err, InstrStop::Revert);
        assert_eq!(interp.memory(2, 3), [0, 0, 0]);
        assert_eq!(interp.output(), [0, 0, 0]);

        let mut code = Vec::new();
        push(&mut code, Word::from(0xcd));
        push(&mut code, Word::from(4));
        code.push(op::MSTORE8);
        push(&mut code, Word::from(2));
        push(&mut code, Word::from(4));
        code.push(op::REVERT);
        let interp = run(RunConfig::new(code));
        assert_matches!(interp.err, InstrStop::Revert);
        assert_eq!(interp.output(), [0xcd, 0]);

        let interp = run_stack([Word::from(0), Word::MAX], op::REVERT);
        assert_matches!(interp.err, InstrStop::InvalidOperandOOG);

        let interp = run_stack([Word::from(u32::MAX), Word::from(1)], op::REVERT);
        assert_matches!(interp.err, InstrStop::MemoryLimitOOG);
    }
}
