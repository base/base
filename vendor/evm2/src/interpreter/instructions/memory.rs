use evm2_macros::instruction;

use crate::{interpreter::Word, utils::word_to_usize};

#[instruction(dynamic_gas)]
pub(crate) fn mload(cx: _, [offset]: [Word]) -> Result<out> {
    let offset = word_to_usize(*offset)?;
    cx.state.resize_memory(cx.gas, offset, 32)?;
    *out = cx.state.memory().get_word(offset);
}

#[instruction(dynamic_gas)]
pub(crate) fn mstore(cx: _, [offset, value]: [Word]) -> Result {
    let offset = word_to_usize(*offset)?;
    cx.state.resize_memory(cx.gas, offset, 32)?;
    cx.state.memory().set(offset, &value.to_be_bytes::<32>());
}

#[instruction(dynamic_gas)]
pub(crate) fn mstore8(cx: _, [offset, value]: [Word]) -> Result {
    let offset = word_to_usize(*offset)?;
    cx.state.resize_memory(cx.gas, offset, 1)?;
    cx.state.memory().set(offset, &[value.byte(0)]);
}

#[instruction]
pub(crate) fn msize(cx: _) -> out {
    *out = Word::from(cx.state.memory().len());
}

#[instruction(dynamic_gas)]
pub(crate) fn mcopy(cx: _, [dst, src, len]: [Word]) -> Result {
    let len = word_to_usize(*len)?;
    cx.gas.spend(cx.state.gas_params().mcopy_cost(len))?;
    if len != 0 {
        let dst = word_to_usize(*dst)?;
        let src = word_to_usize(*src)?;
        cx.state.resize_memory(cx.gas, dst.max(src), len)?;
        cx.state.memory().copy(dst, src, len);
    };
}

#[cfg(test)]
mod tests {
    use alloc::vec::Vec;
    use core::assert_matches;

    use crate::{
        ExecutionConfig, SpecId, Version,
        env::TxEnvExt,
        interpreter::{InstrStop, Interpreter, MessageExt, Word, op},
        test_utils::{RunConfig, TestHost, TestTypes, legacy_bytecode, push, run, run_stack},
        version::GasId,
    };

    #[test]
    fn mload_opcode() {
        let value = Word::from(0xfeed);
        let mut code = Vec::new();
        push(&mut code, value);
        push(&mut code, 0);
        code.push(op::MSTORE);
        push(&mut code, 0);
        code.push(op::MLOAD);
        code.push(op::STOP);

        let mut interp = run(RunConfig::new(code));
        assert_matches!(interp.err, InstrStop::Stop);
        assert_eq!(interp.stack(), [value]);
        assert_eq!(interp.memory(30, 2), [0xfe, 0xed]);

        let interp = run_stack([Word::MAX], op::MLOAD);
        assert_matches!(interp.err, InstrStop::InvalidOperandOOG);
    }

    #[test]
    fn mstore_opcode() {
        let value = Word::from(0xfeed);
        let mut code = Vec::new();
        push(&mut code, value);
        push(&mut code, Word::from(8));
        code.push(op::MSTORE);
        code.push(op::MSIZE);
        code.push(op::STOP);

        let mut interp = run(RunConfig::new(code));
        assert_matches!(interp.err, InstrStop::Stop);
        assert_eq!(interp.stack(), [Word::from(64)]);
        assert_eq!(interp.memory(38, 2), [0xfe, 0xed]);

        let interp = run_stack([Word::MAX, Word::from(0)], op::MSTORE);
        assert_matches!(interp.err, InstrStop::InvalidOperandOOG);
    }

    #[test]
    fn mstore_respects_version_memory_limit() {
        let mut version = Version::new(SpecId::OSAKA);
        version.memory_limit = 64;
        let config = ExecutionConfig::<TestTypes>::for_spec_and_version(SpecId::OSAKA, version);
        let mut code = Vec::new();
        push(&mut code, Word::ZERO);
        push(&mut code, Word::from(64));
        code.push(op::MSTORE);
        code.push(op::STOP);

        let tx_env = TxEnvExt::default();
        let message =
            MessageExt { gas_limit: 10_000, code: legacy_bytecode(code), ..MessageExt::default() };
        let mut interp = Interpreter::<TestTypes>::new(&tx_env, &message);
        let mut host = TestHost::default();
        let err = interp.run(&config, &mut host);

        assert_matches!(err, InstrStop::MemoryLimitOOG);
        assert_eq!(interp.memory().len(), 0);
    }

    #[test]
    fn mstore_respects_version_memory_gas_params() {
        let mut version = Version::new(SpecId::OSAKA);
        version.gas_params[GasId::MemoryLinearCost] = 9;
        let config = ExecutionConfig::<TestTypes>::for_spec_and_version(SpecId::OSAKA, version);
        let mut code = Vec::new();
        push(&mut code, Word::ZERO);
        push(&mut code, Word::ZERO);
        code.push(op::MSTORE);
        code.push(op::STOP);

        let tx_env = TxEnvExt::default();
        let message =
            MessageExt { gas_limit: 17, code: legacy_bytecode(code), ..MessageExt::default() };
        let mut interp = Interpreter::<TestTypes>::new(&tx_env, &message);
        let mut host = TestHost::default();
        let err = interp.run(&config, &mut host);

        assert_matches!(err, InstrStop::MemoryOOG);
        assert_eq!(interp.memory().len(), 0);
    }

    #[test]
    fn mstore8_opcode() {
        let mut code = Vec::new();
        push(&mut code, Word::from(0x01ab));
        push(&mut code, Word::from(4));
        code.push(op::MSTORE8);
        push(&mut code, Word::from(4));
        code.push(op::MLOAD);
        code.push(op::STOP);

        let mut interp = run(RunConfig::new(code));
        assert_matches!(interp.err, InstrStop::Stop);
        assert_eq!(interp.memory(4, 1), [0xab]);
        assert_eq!(interp.stack()[0] >> 248, Word::from(0xab));

        let interp = run_stack([Word::MAX, Word::from(0)], op::MSTORE8);
        assert_matches!(interp.err, InstrStop::InvalidOperandOOG);
    }

    #[test]
    fn msize_opcode() {
        let interp = run(RunConfig::new([op::MSIZE, op::STOP]));
        assert_matches!(interp.err, InstrStop::Stop);
        assert_eq!(interp.stack(), [0]);

        let mut code = Vec::new();
        push(&mut code, 0);
        push(&mut code, Word::from(33));
        code.push(op::MSTORE);
        code.push(op::MSIZE);
        code.push(op::STOP);
        let interp = run(RunConfig::new(code));
        assert_matches!(interp.err, InstrStop::Stop);
        assert_eq!(interp.stack(), [Word::from(96)]);
    }

    #[test]
    fn mcopy_opcode() {
        let value = Word::from(0x1234);
        let mut code = Vec::new();
        push(&mut code, value);
        push(&mut code, 0);
        code.push(op::MSTORE);
        push(&mut code, Word::from(32));
        push(&mut code, 0);
        push(&mut code, Word::from(32));
        code.push(op::MCOPY);
        push(&mut code, Word::from(32));
        code.push(op::MLOAD);
        code.push(op::STOP);

        let interp = run(RunConfig::new(code));
        assert_matches!(interp.err, InstrStop::Stop);
        assert_eq!(interp.stack(), [value]);

        let mut code = Vec::new();
        push(&mut code, 0);
        push(&mut code, Word::from(1));
        push(&mut code, 0);
        code.push(op::MCOPY);
        code.push(op::MSIZE);
        code.push(op::STOP);
        let interp = run(RunConfig::new(code));
        assert_matches!(interp.err, InstrStop::Stop);
        assert_eq!(interp.stack(), [0]);

        let interp = run_stack([Word::MAX, Word::MAX, Word::from(0)], op::MCOPY);
        assert_matches!(interp.err, InstrStop::Stop);

        let interp = run_stack([Word::MAX, Word::from(0), Word::from(1)], op::MCOPY);
        assert_matches!(interp.err, InstrStop::InvalidOperandOOG);
    }

    #[test]
    fn mcopy_charges_dynamic_gas() {
        let mut code = Vec::new();
        push(&mut code, Word::ZERO);
        push(&mut code, 0);
        code.push(op::MSTORE);
        push(&mut code, Word::from(32));
        push(&mut code, 0);
        push(&mut code, 0);
        code.extend([op::MCOPY, op::STOP]);

        let interp = run(RunConfig::new(code).spec(SpecId::CANCUN).gas_limit(26));

        assert_matches!(interp.err, InstrStop::OutOfGas);
    }
}
