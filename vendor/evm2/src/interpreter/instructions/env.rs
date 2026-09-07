use alloy_primitives::B256;
use evm2_macros::instruction;

use crate::{
    EvmTypesHost,
    evm::AccountLoad,
    interpreter::{Gas, Host, InstrStop, Memory, Result, Word, private::GasInstructionCx},
    utils::{
        address_to_word, b256_to_word, word_to_address, word_to_usize, word_to_usize_saturated,
    },
    version::GasParams,
};

fn load_account<T: EvmTypesHost>(
    cx: &mut GasInstructionCx<'_, '_, '_, T>,
    addr: Word,
    load_code: bool,
) -> Result<AccountLoad> {
    let cold_load_gas = cx.state.gas_params().cold_account_additional_cost();
    let skip_cold_load = cx.gas.remaining() < cold_load_gas;
    let account_address = word_to_address(addr);
    let account = cx.state.host().load_account(&account_address, load_code, skip_cold_load)?;
    if account.is_cold {
        cx.gas.spend(cold_load_gas)?;
    }
    Ok(account)
}

#[inline]
fn copy_data(
    gas: &mut Gas,
    memory: &mut Memory,
    gas_params: &GasParams,
    memory_offset: &Word,
    data_offset: &Word,
    len: usize,
    data: &[u8],
) -> Result {
    if len != 0 {
        let memory_offset = word_to_usize(*memory_offset)?;
        memory.resize_evm(gas, gas_params, memory_offset, len)?;
        memory.set_data(memory_offset, word_to_usize_saturated(*data_offset), len, data);
    }
    Ok(())
}

#[instruction]
pub(crate) fn address(cx: _) -> out {
    *out = address_to_word(&cx.state.message().destination);
}

#[instruction(dynamic_gas)]
pub(crate) fn balance(cx: _, [addr]: [Word]) -> Result<out> {
    *out = load_account(&mut cx, *addr, false)?.balance;
}

#[instruction]
pub(crate) fn origin(cx: _) -> out {
    *out = address_to_word(&cx.state.tx().origin);
}

#[instruction]
pub(crate) fn caller(cx: _) -> out {
    *out = address_to_word(&cx.state.message().caller);
}

#[instruction]
pub(crate) fn callvalue(cx: _) -> out {
    *out = cx.state.message().value;
}

#[instruction]
pub(crate) fn calldataload(cx: _, [offset]: [Word]) -> out {
    let offset = word_to_usize_saturated(*offset);
    let input = cx.state.message().input.as_ref();
    let mut word = B256::ZERO;
    if offset < input.len() {
        let len = 32.min(input.len() - offset);
        word[..len].copy_from_slice(&input[offset..offset + len]);
    }
    *out = b256_to_word(word);
}

#[instruction]
pub(crate) fn calldatasize(cx: _) -> out {
    *out = Word::from(cx.state.message().input.len());
}

#[instruction(dynamic_gas)]
pub(crate) fn calldatacopy(cx: _, [memory_offset, data_offset, len]: [Word]) -> Result {
    let len = word_to_usize(*len)?;
    cx.gas.spend(cx.state.gas_params().copy_cost(len))?;
    let input = cx.state.message().input.as_ref();
    let gas_params = cx.state.gas_params();
    copy_data(cx.gas, &mut cx.state.0.memory, gas_params, memory_offset, data_offset, len, input)
}

#[instruction]
pub(crate) fn codesize(cx: _) -> out {
    *out = Word::from(cx.state.bytecode().len());
}

#[instruction(dynamic_gas)]
pub(crate) fn codecopy(cx: _, [memory_offset, code_offset, len]: [Word]) -> Result {
    let len = word_to_usize(*len)?;
    cx.gas.spend(cx.state.gas_params().copy_cost(len))?;
    let data = cx.state.0.bytecode.original_byte_slice();
    let gas_params = cx.state.gas_params();
    copy_data(cx.gas, &mut cx.state.0.memory, gas_params, memory_offset, code_offset, len, data)
}

#[instruction]
pub(crate) fn gasprice(cx: _) -> out {
    *out = cx.state.tx().gas_price;
}

#[instruction(dynamic_gas)]
pub(crate) fn extcodesize(cx: _, [addr]: [Word]) -> Result<out> {
    *out = Word::from(load_account(&mut cx, *addr, true)?.code.len());
}

#[instruction(dynamic_gas)]
pub(crate) fn extcodehash(cx: _, [addr]: [Word]) -> Result<out> {
    let account = load_account(&mut cx, *addr, false)?;
    *out = if account.is_empty { Word::ZERO } else { b256_to_word(account.code_hash) };
}

#[instruction(dynamic_gas)]
pub(crate) fn extcodecopy(cx: _, [addr, memory_offset, code_offset, len]: [Word]) -> Result {
    let len = word_to_usize(*len)?;
    cx.gas.spend(cx.state.gas_params().extcodecopy_cost(len))?;
    let memory_offset = if len != 0 {
        let memory_offset = word_to_usize(*memory_offset)?;
        cx.state.resize_memory(cx.gas, memory_offset, len)?;
        memory_offset
    } else {
        0
    };
    let code = load_account(&mut cx, *addr, true)?.code;
    cx.state.0.memory.set_data(
        memory_offset,
        word_to_usize_saturated(*code_offset),
        len,
        code.original_byte_slice(),
    );
    Ok(())
}

#[instruction]
pub(crate) fn returndatasize(cx: _) -> Result<out> {
    *out = Word::from(cx.state.return_data().len());
}

#[instruction(dynamic_gas)]
pub(crate) fn returndatacopy(cx: _, [memory_offset, data_offset, len]: [Word]) -> Result {
    let len = word_to_usize(*len)?;
    let data_offset = word_to_usize_saturated(*data_offset);
    if data_offset.saturating_add(len) > cx.state.return_data().len() {
        return Err(InstrStop::OutOfOffset);
    }

    cx.gas.spend(cx.state.gas_params().copy_cost(len))?;
    let data = &cx.state.0.return_data;
    let data_offset = Word::from(data_offset);
    let gas_params = cx.state.gas_params();
    copy_data(cx.gas, &mut cx.state.0.memory, gas_params, memory_offset, &data_offset, len, data)
}

#[cfg(test)]
mod tests {
    use alloc::{vec, vec::Vec};
    use core::assert_matches;

    use alloy_primitives::{Address, B256, Bytes};

    use crate::{
        SpecId,
        env::TxEnvExt,
        interpreter::{InstrStop, Message, MessageExt, Word, op},
        test_utils::{
            RunConfig, TestHost, TestTypes, assert_stack, neg, push, run, run_stack, stack_code,
        },
        utils::{address_to_word, b256_to_word},
    };

    fn test_message() -> Message<TestTypes> {
        MessageExt { gas_limit: 10_000, ..MessageExt::default() }
    }

    #[test]
    fn address_opcode() {
        let address = Address::from([0x11; 20]);
        let mut host = TestHost::default();
        let message = MessageExt { destination: address, ..test_message() };
        let interp = run(RunConfig::new([op::ADDRESS, op::STOP]).host(&mut host).message(message));
        assert_matches!(interp.err, InstrStop::Stop);
        assert_eq!(interp.stack(), [address_to_word(&address)]);
    }

    #[test]
    fn balance_opcode() {
        assert_stack!(BALANCE(0xbeef), 0xbeef);
        assert_stack!(BALANCE(0), 0);
        assert_stack!(BALANCE(neg(1)), address_to_word(&Address::from([0xff; 20])));
    }

    #[test]
    fn balance_cold_account_cost() {
        let mut host = TestHost { is_cold: true, ..TestHost::default() };
        let interp = run(RunConfig::new([op::PUSH1, 0xbe, op::BALANCE, op::STOP])
            .host(&mut host)
            .spec(SpecId::BERLIN));
        assert_matches!(interp.err, InstrStop::Stop);
        assert_eq!(interp.stack(), [Word::from(0xbe)]);
        assert_eq!(interp.gas_remaining(), 7_397);
    }

    #[test]
    fn balance_cold_account_skip_oog() {
        let mut host = TestHost { is_cold: true, ..TestHost::default() };
        let interp = run(RunConfig::new([op::PUSH1, 0xbe, op::BALANCE, op::STOP])
            .host(&mut host)
            .spec(SpecId::BERLIN)
            .gas_limit(103));
        assert_matches!(interp.err, InstrStop::OutOfGas);
    }

    #[test]
    fn origin_opcode() {
        let origin = Address::from([0x22; 20]);
        let mut host = TestHost::default();
        let tx_env = TxEnvExt { origin, ..TxEnvExt::default() };
        let interp = run(RunConfig::new([op::ORIGIN, op::STOP]).host(&mut host).tx_env(tx_env));
        assert_matches!(interp.err, InstrStop::Stop);
        assert_eq!(interp.stack(), [address_to_word(&origin)]);
    }

    #[test]
    fn caller_opcode() {
        let caller = Address::from([0x33; 20]);
        let mut host = TestHost::default();
        let message = MessageExt { caller, ..test_message() };
        let interp = run(RunConfig::new([op::CALLER, op::STOP]).host(&mut host).message(message));
        assert_matches!(interp.err, InstrStop::Stop);
        assert_eq!(interp.stack(), [address_to_word(&caller)]);
    }

    #[test]
    fn callvalue_opcode() {
        let mut host = TestHost::default();
        let message = MessageExt { value: Word::from(0xbeef), ..test_message() };
        let interp =
            run(RunConfig::new([op::CALLVALUE, op::STOP]).host(&mut host).message(message));
        assert_matches!(interp.err, InstrStop::Stop);
        assert_eq!(interp.stack(), [Word::from(0xbeef)]);
    }

    #[test]
    fn calldataload_opcode() {
        let input = Bytes::from(Vec::from([1_u8, 2, 3]));
        let mut host = TestHost::default();
        let message = MessageExt { input, ..test_message() };

        let interp = run(RunConfig::new([op::PUSH0, op::CALLDATALOAD, op::STOP])
            .host(&mut host)
            .message(message.clone()));
        let mut expected = [0_u8; 32];
        expected[..3].copy_from_slice(&[1, 2, 3]);
        assert_matches!(interp.err, InstrStop::Stop);
        assert_eq!(interp.stack(), [Word::from_be_bytes(expected)]);

        let interp = run(RunConfig::new([op::PUSH1, 0x20, op::CALLDATALOAD, op::STOP])
            .host(&mut host)
            .message(message));
        assert_matches!(interp.err, InstrStop::Stop);
        assert_eq!(interp.stack(), [0]);
    }

    #[test]
    fn calldatasize_opcode() {
        let input = Bytes::from(Vec::from([1_u8, 2, 3, 4]));
        let mut host = TestHost::default();
        let message = MessageExt { input, ..test_message() };
        let interp =
            run(RunConfig::new([op::CALLDATASIZE, op::STOP]).host(&mut host).message(message));
        assert_matches!(interp.err, InstrStop::Stop);
        assert_eq!(interp.stack(), [Word::from(4)]);
    }

    #[test]
    fn calldatacopy_opcode() {
        let input = Bytes::from(Vec::from([0xaa_u8, 0xbb, 0xcc]));
        let mut host = TestHost::default();
        let message = MessageExt { input, ..test_message() };
        let mut code = Vec::new();
        push(&mut code, 2);
        push(&mut code, 1);
        push(&mut code, 0);
        code.push(op::CALLDATACOPY);
        push(&mut code, 0);
        code.push(op::MLOAD);
        code.push(op::STOP);

        let interp = run(RunConfig::new(code).host(&mut host).message(message.clone()));
        let mut expected = [0_u8; 32];
        expected[..2].copy_from_slice(&[0xbb, 0xcc]);
        assert_matches!(interp.err, InstrStop::Stop);
        assert_eq!(interp.stack(), [Word::from_be_bytes(expected)]);

        let interp = run(RunConfig::new(stack_code(
            [Word::MAX, Word::MAX, Word::from(0)],
            op::CALLDATACOPY,
        ))
        .host(&mut host)
        .message(message));
        assert_matches!(interp.err, InstrStop::Stop);
    }

    #[test]
    fn codesize_opcode() {
        let interp = run(RunConfig::new([op::CODESIZE, op::STOP]));
        assert_matches!(interp.err, InstrStop::Stop);
        assert_eq!(interp.stack(), [Word::from(2)]);

        let interp = run(RunConfig::new([op::PUSH1, 0x00, op::CODESIZE, op::STOP]));
        assert_matches!(interp.err, InstrStop::Stop);
        assert_eq!(interp.stack(), [Word::from(0), Word::from(4)]);
    }

    #[test]
    fn codecopy_opcode() {
        let mut code = Vec::new();
        push(&mut code, Word::from(2));
        push(&mut code, Word::from(5));
        push(&mut code, 0);
        code.push(op::CODECOPY);
        push(&mut code, 0);
        code.push(op::MLOAD);
        code.push(op::STOP);

        let interp = run(RunConfig::new(code));
        let mut expected = [0u8; 32];
        expected[..2].copy_from_slice(&[0, op::CODECOPY]);
        assert_matches!(interp.err, InstrStop::Stop);
        assert_eq!(interp.stack(), [Word::from_be_bytes(expected)]);

        let mut code = Vec::new();
        push(&mut code, Word::from(1));
        push(&mut code, Word::from(usize::MAX));
        push(&mut code, 0);
        code.push(op::CODECOPY);
        push(&mut code, 0);
        code.push(op::MLOAD);
        code.push(op::STOP);
        let interp = run(RunConfig::new(code));
        assert_matches!(interp.err, InstrStop::Stop);
        assert_eq!(interp.stack(), [0]);

        let interp = run_stack([Word::MAX, Word::MAX, Word::from(0)], op::CODECOPY);
        assert_matches!(interp.err, InstrStop::Stop);

        let interp = run_stack([Word::MAX, Word::from(0), Word::from(1)], op::CODECOPY);
        assert_matches!(interp.err, InstrStop::InvalidOperandOOG);
    }

    #[test]
    fn gasprice_opcode() {
        let mut host = TestHost::default();
        let tx_env = TxEnvExt { gas_price: Word::from(0x1234), ..TxEnvExt::default() };
        let interp = run(RunConfig::new([op::GASPRICE, op::STOP]).host(&mut host).tx_env(tx_env));
        assert_matches!(interp.err, InstrStop::Stop);
        assert_eq!(interp.stack(), [Word::from(0x1234)]);
    }

    #[test]
    fn extcodesize_opcode() {
        let mut host = TestHost { code: Bytes::from(vec![0; 0x42]), ..TestHost::default() };
        let interp =
            run(RunConfig::new([op::PUSH1, 0xbe, op::EXTCODESIZE, op::STOP]).host(&mut host));
        assert_matches!(interp.err, InstrStop::Stop);
        assert_eq!(interp.stack(), [Word::from(0x42)]);
    }

    #[test]
    fn extcodecopy_opcode() {
        let mut host =
            TestHost { code: Bytes::from_static(&[0xaa, 0xbb, 0xcc]), ..TestHost::default() };
        let mut code = Vec::new();
        push(&mut code, 2);
        push(&mut code, 1);
        push(&mut code, 0);
        push(&mut code, 0xbeef);
        code.push(op::EXTCODECOPY);
        push(&mut code, 0);
        code.push(op::MLOAD);
        code.push(op::STOP);

        let interp = run(RunConfig::new(code).host(&mut host));
        let mut expected = [0_u8; 32];
        expected[..2].copy_from_slice(&[0xbb, 0xcc]);
        assert_matches!(interp.err, InstrStop::Stop);
        assert_eq!(interp.stack(), [Word::from_be_bytes(expected)]);

        let mut code = Vec::new();
        push(&mut code, 4);
        push(&mut code, 2);
        push(&mut code, 0);
        push(&mut code, 0xbeef);
        code.push(op::EXTCODECOPY);
        push(&mut code, 0);
        code.push(op::MLOAD);
        code.push(op::STOP);
        let interp = run(RunConfig::new(code).host(&mut host));
        let mut expected = [0_u8; 32];
        expected[..1].copy_from_slice(&[0xcc]);
        assert_matches!(interp.err, InstrStop::Stop);
        assert_eq!(interp.stack(), [Word::from_be_bytes(expected)]);

        let interp = run(RunConfig::new(stack_code(
            [Word::from(0xbeef), Word::MAX, Word::MAX, Word::from(0)],
            op::EXTCODECOPY,
        ))
        .host(&mut host));
        assert_matches!(interp.err, InstrStop::Stop);

        let interp = run(RunConfig::new(stack_code(
            [Word::from(0xbeef), Word::MAX, Word::from(0), Word::from(1)],
            op::EXTCODECOPY,
        ))
        .host(&mut host));
        assert_matches!(interp.err, InstrStop::InvalidOperandOOG);
    }

    #[test]
    fn returndatasize_opcode() {
        let interp = run(RunConfig::new([op::RETURNDATASIZE, op::STOP])
            .spec(SpecId::BYZANTIUM)
            .return_data(Bytes::from_static(&[0xaa, 0xbb, 0xcc])));
        assert_matches!(interp.err, InstrStop::Stop);
        assert_eq!(interp.stack(), [Word::from(3)]);

        let interp = run(RunConfig::new([op::RETURNDATASIZE]).spec(SpecId::FRONTIER));
        assert_matches!(interp.err, InstrStop::InvalidOpcode);
    }

    #[test]
    fn returndatacopy_opcode() {
        let mut code = Vec::new();
        push(&mut code, 2);
        push(&mut code, 1);
        push(&mut code, 0);
        code.push(op::RETURNDATACOPY);
        push(&mut code, 0);
        code.push(op::MLOAD);
        code.push(op::STOP);

        let interp = run(RunConfig::new(code).return_data(Bytes::from_static(&[0xaa, 0xbb, 0xcc])));
        let mut expected = [0_u8; 32];
        expected[..2].copy_from_slice(&[0xbb, 0xcc]);
        assert_matches!(interp.err, InstrStop::Stop);
        assert_eq!(interp.stack(), [Word::from_be_bytes(expected)]);

        let interp = run(RunConfig::new(stack_code(
            [Word::from(0), Word::from(3), Word::from(0)],
            op::RETURNDATACOPY,
        ))
        .spec(SpecId::BYZANTIUM)
        .return_data(Bytes::from_static(&[0xaa, 0xbb, 0xcc])));
        assert_matches!(interp.err, InstrStop::Stop);

        let interp = run(RunConfig::new(stack_code(
            [Word::from(0), Word::from(4), Word::from(0)],
            op::RETURNDATACOPY,
        ))
        .spec(SpecId::BYZANTIUM)
        .return_data(Bytes::from_static(&[0xaa, 0xbb, 0xcc])));
        assert_matches!(interp.err, InstrStop::OutOfOffset);

        let interp = run(RunConfig::new(stack_code(
            [Word::MAX, Word::from(0), Word::from(1)],
            op::RETURNDATACOPY,
        ))
        .spec(SpecId::BYZANTIUM)
        .return_data(Bytes::from_static(&[0xaa])));
        assert_matches!(interp.err, InstrStop::InvalidOperandOOG);

        let interp = run(RunConfig::new(stack_code(
            [Word::from(0), Word::from(0), Word::from(0)],
            op::RETURNDATACOPY,
        ))
        .spec(SpecId::FRONTIER));
        assert_matches!(interp.err, InstrStop::InvalidOpcode);
    }

    #[test]
    fn extcodehash_opcode() {
        let hash = B256::with_last_byte(0x77);
        let mut host = TestHost { code_hash: hash, ..TestHost::default() };
        let interp = run(RunConfig::new([op::PUSH1, 0xbe, op::EXTCODEHASH, op::STOP])
            .host(&mut host)
            .spec(SpecId::PETERSBURG));
        assert_matches!(interp.err, InstrStop::Stop);
        assert_eq!(interp.stack(), [b256_to_word(hash)]);

        let interp = run(RunConfig::new([op::PUSH1, 0xbe, op::EXTCODEHASH, op::STOP])
            .host(&mut host)
            .spec(SpecId::BYZANTIUM));
        assert_matches!(interp.err, InstrStop::InvalidOpcode);
    }
}
