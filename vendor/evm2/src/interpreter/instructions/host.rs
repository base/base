use alloy_primitives::{B256, Bytes, Log, LogData};
use evm2_macros::instruction;

use crate::{
    EvmFeatures, EvmTypesHost,
    interpreter::{Host, InstrStop, InterpreterState, Result, StackMut, private::GasInstructionCx},
    utils::word_to_usize,
    version::GasId,
};

#[inline]
const fn require_non_staticcall<T: EvmTypesHost>(state: &InterpreterState<'_, '_, T>) -> Result {
    if state.is_static() {
        return Err(InstrStop::StateChangeDuringStaticCall);
    }
    Ok(())
}

#[instruction(dynamic_gas)]
pub(crate) fn sload(cx: _, [key]: [Word]) -> Result<out> {
    // EIP-2929: SLOAD pays the warm read cost as static opcode gas, then only
    // charges the additional cold cost when the slot was not already warm. Avoid
    // touching the host/database if the frame cannot afford that cold surcharge.
    let additional_cold_cost = cx.state.gas_params().get(GasId::ColdStorageAdditionalCost).into();
    let skip_cold_load =
        cx.state.feature(EvmFeatures::EIP2929) && cx.gas.remaining() < additional_cold_cost;
    let destination = &cx.state.message().destination;
    let load = cx.state.host().sload(destination, key, skip_cold_load)?;
    if load.is_cold {
        cx.gas.spend(additional_cold_cost)?;
    }
    *out = load.value;
}

#[instruction(dynamic_gas)]
pub(crate) fn sstore(cx: _, [key, value]: [Word]) -> Result {
    require_non_staticcall(cx.state)?;
    let is_eip2200 = cx.state.feature(EvmFeatures::EIP2200);

    // EIP-2200: SSTORE may not execute with only the value-transfer stipend left. This
    // check happens before any gas is charged or host storage is touched.
    if is_eip2200 && cx.gas.remaining() <= cx.state.gas_params().get(GasId::CallStipend).into() {
        return Err(InstrStop::ReentrancySentryOOG);
    }

    // Frontier through Petersburg charge a fixed SSTORE_RESET static cost. Istanbul
    // (EIP-2200) turns this into the SLOAD-equivalent cost, and Berlin (EIP-2929)
    // makes that the warm storage read cost.
    cx.gas.spend(cx.state.gas_params().get(GasId::SstoreStatic).into())?;

    // EIP-2929: avoid performing a cold storage load if the frame cannot afford the
    // additional cold-load charge. The host performs the write and returns
    // original/present/new values for net metering.
    let skip_cold_load = cx.state.feature(EvmFeatures::EIP2929)
        && cx.gas.remaining() < cx.state.gas_params().get(GasId::ColdStorageAdditionalCost).into();
    let destination = &cx.state.message().destination;
    let state_load = cx.state.host().sstore(destination, key, value, skip_cold_load)?;

    // EIP-2200 net gas metering depends on original, present, and new slot values:
    // clean slots pay set/reset costs, dirty slots generally only pay the load cost,
    // and reset-to-original transitions are handled through refunds.
    cx.gas.spend(cx.state.gas_params().sstore_dynamic_gas(is_eip2200, &state_load))?;

    // EIP-8037 / Amsterdam: creating a new storage slot (original == present == 0,
    // new != 0) also consumes state gas from the reservoir before spilling into
    // execution gas.
    if cx.state.feature(EvmFeatures::EIP8037) {
        cx.gas.spend_state(cx.state.gas_params().sstore_state_gas(&state_load))?;

        // EIP-8037: a 0→x→0 storage restoration returns the state gas charged for
        // the initial 0→x transition directly to the reservoir, rather than
        // routing it through the capped refund counter below.
        let refill = cx.state.gas_params().sstore_state_gas_refill(&state_load);
        cx.gas.refill_reservoir(refill);
    }

    // EIP-2200 and EIP-3529 refund rules, including negative refund adjustments for
    // dirty nonzero slots and London's reduced clearing-slot refund via gas params.
    cx.gas.record_refund(cx.state.gas_params().sstore_refund(is_eip2200, &state_load));
    Ok(())
}

#[instruction]
pub(crate) fn tload(cx: _, [key]: [Word]) -> out {
    let destination = &cx.state.message().destination;
    *out = cx.state.host().tload(destination, key);
}

#[instruction]
pub(crate) fn tstore(cx: _, [key, value]: [Word]) -> Result {
    require_non_staticcall(cx.state)?;
    let destination = &cx.state.message().destination;
    cx.state.host().tstore(destination, key, value);
}

#[instruction(no_stack_preamble, dynamic_gas)]
pub(crate) fn log<const N: usize>(cx: _) -> Result {
    log_common(cx, stack, N)
}

#[inline(never)]
fn log_common<T: EvmTypesHost>(
    cx: GasInstructionCx<'_, '_, '_, T>,
    mut stack: StackMut<'_>,
    n: usize,
) -> Result {
    require_non_staticcall(cx.state)?;
    let [offset, len] = stack.popn()?;
    let len = word_to_usize(len)?;
    cx.gas.spend(cx.state.gas_params().log_cost(n as u8, len))?;

    let data = if len == 0 {
        Bytes::new()
    } else {
        let offset = word_to_usize(offset)?;
        cx.state.resize_memory(cx.gas, offset, len)?;
        Bytes::copy_from_slice(cx.state.memory().slice(offset, len))
    };

    let topics = stack.popn_dyn(n)?.map(|topic| B256::from(topic.to_be_bytes::<32>())).collect();
    let destination = cx.state.message().destination;
    let emitted_log = Log {
        address: destination,
        // SAFETY: `log` is only dispatched for LOG0 through LOG4.
        data: unsafe { LogData::new(topics, data).unwrap_unchecked() },
    };
    cx.state.host().log(emitted_log);
    Ok(())
}

#[cfg(test)]
mod tests {
    use alloc::vec::Vec;
    use core::assert_matches;

    use alloy_primitives::{Address, B256, Bytes};

    use crate::{
        SpecId,
        interpreter::{InstrStop, MessageExt, MessageKind, Word, op},
        storage_key::StorageKey,
        test_utils::{RunConfig, TestHost, push, run},
    };

    #[test]
    fn sload_opcode() {
        let mut host = TestHost::default();
        host.storage.insert(StorageKey::new(Address::ZERO, Word::from(1)), Word::from(0xbeef));

        let mut code = Vec::new();
        push(&mut code, 1);
        code.extend([op::SLOAD, op::STOP]);
        let interp = run(RunConfig::new(code).host(&mut host));
        assert_matches!(interp.err, InstrStop::Stop);
        assert_eq!(interp.stack(), [Word::from(0xbeef)]);

        let mut code = Vec::new();
        push(&mut code, 2);
        code.extend([op::SLOAD, op::STOP]);
        let interp = run(RunConfig::new(code).host(&mut host));
        assert_matches!(interp.err, InstrStop::Stop);
        assert_eq!(interp.stack(), [0]);
    }

    #[test]
    fn sstore_opcode() {
        let mut host = TestHost::default();
        let mut code = Vec::new();
        push(&mut code, 0xbeef);
        push(&mut code, 1);
        code.push(op::SSTORE);
        push(&mut code, 1);
        code.extend([op::SLOAD, op::STOP]);

        let interp = run(RunConfig::new(code).host(&mut host).gas_limit(30_000));
        assert_matches!(interp.err, InstrStop::Stop);
        assert_eq!(interp.stack(), [Word::from(0xbeef)]);
        assert_eq!(
            host.storage.get(&StorageKey::new(Address::ZERO, Word::from(1))),
            Some(&Word::from(0xbeef))
        );
    }

    #[test]
    fn sstore_staticcall_check() {
        let mut host = TestHost::default();
        let mut code = Vec::new();
        push(&mut code, 0xbeef);
        push(&mut code, 1);
        code.extend([op::SSTORE, op::STOP]);

        let interp = run(RunConfig::new(code).host(&mut host).staticcall());
        assert_matches!(interp.err, InstrStop::StateChangeDuringStaticCall);
        assert!(interp.stack().is_empty());
        assert_eq!(host.storage.get(&StorageKey::new(Address::ZERO, Word::from(1))), None);
    }

    #[test]
    fn sstore_staticcall_message_kind_check() {
        let mut host = TestHost::default();
        let mut code = Vec::new();
        push(&mut code, 0xbeef);
        push(&mut code, 1);
        code.extend([op::SSTORE, op::STOP]);
        let message =
            MessageExt { kind: MessageKind::StaticCall, gas_limit: 10_000, ..Default::default() };

        let interp = run(RunConfig::new(code).host(&mut host).message(message));

        assert_matches!(interp.err, InstrStop::StateChangeDuringStaticCall);
        assert!(interp.stack().is_empty());
        assert_eq!(host.storage.get(&StorageKey::new(Address::ZERO, Word::from(1))), None);
    }

    #[test]
    fn sstore_stipend_check() {
        let mut host = TestHost::default();
        let mut code = Vec::new();
        push(&mut code, 0xbeef);
        push(&mut code, 1);
        code.extend([op::SSTORE, op::STOP]);

        let interp =
            run(RunConfig::new(code).host(&mut host).spec(SpecId::ISTANBUL).gas_limit(2306));
        assert_matches!(interp.err, InstrStop::ReentrancySentryOOG);
        assert_eq!(host.storage.get(&StorageKey::new(Address::ZERO, Word::from(1))), None);
    }

    #[test]
    fn sstore_static_gas() {
        let mut host = TestHost::default();
        let interp = run(RunConfig::new([op::PUSH1, 0, op::PUSH1, 0, op::SSTORE, op::STOP])
            .host(&mut host)
            .spec(SpecId::FRONTIER)
            .gas_limit(6000));
        assert_matches!(interp.err, InstrStop::Stop);
        assert_eq!(interp.gas_remaining(), 994);
    }

    #[test]
    fn sstore_noop_uses_warm_load_gas() {
        let mut host = TestHost::default();
        let interp = run(RunConfig::new([op::PUSH1, 0, op::PUSH1, 0, op::SSTORE, op::STOP])
            .host(&mut host)
            .spec(SpecId::BERLIN)
            .gas_limit(3000));

        assert_matches!(interp.err, InstrStop::Stop);
        assert_eq!(interp.gas_remaining(), 2894);
    }

    #[test]
    fn sstore_dirty_slot_write_only_pays_load_cost() {
        let mut host = TestHost::default();
        let mut code = Vec::new();
        push(&mut code, 1);
        push(&mut code, 0);
        code.push(op::SSTORE);
        push(&mut code, 2);
        push(&mut code, 0);
        code.extend([op::SSTORE, op::STOP]);

        let interp =
            run(RunConfig::new(code).host(&mut host).spec(SpecId::BERLIN).gas_limit(50_000));

        assert_matches!(interp.err, InstrStop::Stop);
        assert_eq!(interp.gas_remaining(), 29_888);
        assert_eq!(interp.gas_refunded(), 0);
    }

    #[test]
    fn sstore_reset_to_original_records_refund() {
        let mut host = TestHost::default();
        host.storage.insert(StorageKey::new(Address::ZERO, Word::from(0)), Word::from(5));
        let mut code = Vec::new();
        push(&mut code, 7);
        push(&mut code, 0);
        code.push(op::SSTORE);
        push(&mut code, 5);
        push(&mut code, 0);
        code.extend([op::SSTORE, op::STOP]);

        let interp =
            run(RunConfig::new(code).host(&mut host).spec(SpecId::BERLIN).gas_limit(50_000));

        assert_matches!(interp.err, InstrStop::Stop);
        assert_eq!(interp.gas_remaining(), 46_988);
        assert_eq!(interp.gas_refunded(), 2_800);
    }

    #[test]
    fn sstore_amsterdam_new_slot_charges_state_gas() {
        let mut host = TestHost::default();
        let interp = run(RunConfig::new([op::PUSH1, 1, op::PUSH1, 0, op::SSTORE, op::STOP])
            .host(&mut host)
            .spec(SpecId::AMSTERDAM)
            .gas_limit(200_000));

        assert_matches!(interp.err, InstrStop::Stop);
        // Regular: PUSH1(3)·2 + SSTORE warm (100 static + EIP-8038 STORAGE_WRITE
        // 10_000 set) = 10_106.
        // State gas (64 × 1530 = 97_920) spills into execution gas (no reservoir).
        assert_eq!(interp.state_gas_spent(), 97_920);
        assert_eq!(interp.gas_remaining(), 200_000 - 10_106 - 97_920);
    }

    #[test]
    fn tload_opcode() {
        let mut host = TestHost::default();
        host.transient_storage
            .insert(StorageKey::new(Address::ZERO, Word::from(1)), Word::from(0xcafe));

        let mut code = Vec::new();
        push(&mut code, 1);
        code.extend([op::TLOAD, op::STOP]);
        let interp = run(RunConfig::new(code).host(&mut host).spec(SpecId::CANCUN));
        assert_matches!(interp.err, InstrStop::Stop);
        assert_eq!(interp.stack(), [Word::from(0xcafe)]);

        let interp = run(RunConfig::new([op::PUSH1, 0, op::TLOAD, op::STOP])
            .host(&mut host)
            .spec(SpecId::SHANGHAI));
        assert_matches!(interp.err, InstrStop::InvalidOpcode);
        assert_eq!(interp.stack(), [0]);
    }

    #[test]
    fn tstore_opcode() {
        let mut host = TestHost::default();
        let mut code = Vec::new();
        push(&mut code, 0xcafe);
        push(&mut code, 1);
        code.push(op::TSTORE);
        push(&mut code, 1);
        code.extend([op::TLOAD, op::STOP]);

        let interp = run(RunConfig::new(code).host(&mut host).spec(SpecId::CANCUN));
        assert_matches!(interp.err, InstrStop::Stop);
        assert_eq!(interp.stack(), [Word::from(0xcafe)]);
        assert_eq!(
            host.transient_storage.get(&StorageKey::new(Address::ZERO, Word::from(1))),
            Some(&Word::from(0xcafe))
        );

        let interp = run(RunConfig::new([op::PUSH1, 0, op::PUSH1, 0, op::TSTORE, op::STOP])
            .host(&mut host)
            .spec(SpecId::SHANGHAI));
        assert_matches!(interp.err, InstrStop::InvalidOpcode);
        assert_eq!(interp.stack(), [0, 0]);
    }

    #[test]
    fn tstore_staticcall_check() {
        let mut host = TestHost::default();
        let mut code = Vec::new();
        push(&mut code, 0xcafe);
        push(&mut code, 1);
        code.extend([op::TSTORE, op::STOP]);

        let interp = run(RunConfig::new(code).host(&mut host).spec(SpecId::CANCUN).staticcall());
        assert_matches!(interp.err, InstrStop::StateChangeDuringStaticCall);
        assert!(interp.stack().is_empty());
        assert_eq!(
            host.transient_storage.get(&StorageKey::new(Address::ZERO, Word::from(1))),
            None
        );
    }

    fn log_code<const N: usize>(offset: usize, len: usize, topics: [Word; N]) -> Vec<u8> {
        let mut code = Vec::new();
        push(&mut code, Word::from(0xbeef));
        push(&mut code, 0);
        code.push(op::MSTORE);
        for topic in topics.into_iter().rev() {
            push(&mut code, topic);
        }
        push(&mut code, len);
        push(&mut code, offset);
        code.push(op::LOG0 + N as u8);
        code.push(op::STOP);
        code
    }

    #[test]
    fn log0_opcode() {
        let mut host = TestHost::default();
        let address = Address::from([0x11; 20]);
        let message = MessageExt { destination: address, gas_limit: 10_000, ..Default::default() };
        let interp = run(RunConfig::new(log_code(30, 2, [])).host(&mut host).message(message));
        assert_matches!(interp.err, InstrStop::Stop);
        assert!(interp.stack().is_empty());
        assert_eq!(host.logs.len(), 1);
        assert_eq!(host.logs[0].address, address);
        assert!(host.logs[0].topics().is_empty());
        assert_eq!(host.logs[0].data.data, Bytes::from_static(&[0xbe, 0xef]));
    }

    #[test]
    fn log1_opcode() {
        let mut host = TestHost::default();
        let interp = run(RunConfig::new(log_code(30, 2, [Word::from(1)])).host(&mut host));
        assert_matches!(interp.err, InstrStop::Stop);
        assert_eq!(host.logs[0].topics(), &[B256::from(Word::from(1).to_be_bytes::<32>())]);
        assert_eq!(host.logs[0].data.data, Bytes::from_static(&[0xbe, 0xef]));
    }

    #[test]
    fn log2_opcode() {
        let mut host = TestHost::default();
        let interp =
            run(RunConfig::new(log_code(30, 0, [Word::from(1), Word::from(2)])).host(&mut host));
        assert_matches!(interp.err, InstrStop::Stop);
        assert_eq!(
            host.logs[0].topics(),
            &[
                B256::from(Word::from(1).to_be_bytes::<32>()),
                B256::from(Word::from(2).to_be_bytes::<32>()),
            ]
        );
        assert!(host.logs[0].data.data.is_empty());
    }

    #[test]
    fn log3_opcode() {
        let mut host = TestHost::default();
        let interp =
            run(RunConfig::new(log_code(30, 1, [Word::from(1), Word::from(2), Word::from(3)]))
                .host(&mut host));
        assert_matches!(interp.err, InstrStop::Stop);
        assert_eq!(host.logs[0].topics().len(), 3);
        assert_eq!(host.logs[0].data.data, Bytes::from_static(&[0xbe]));
    }

    #[test]
    fn log4_opcode() {
        let mut host = TestHost::default();
        let interp = run(RunConfig::new(log_code(
            30,
            1,
            [Word::from(1), Word::from(2), Word::from(3), Word::from(4)],
        ))
        .host(&mut host));
        assert_matches!(interp.err, InstrStop::Stop);
        assert_eq!(host.logs[0].topics().len(), 4);
        assert_eq!(host.logs[0].data.data, Bytes::from_static(&[0xbe]));
    }

    #[test]
    fn log_staticcall_check() {
        let mut host = TestHost::default();
        let interp = run(RunConfig::new(log_code(30, 2, [])).host(&mut host).staticcall());
        assert_matches!(interp.err, InstrStop::StateChangeDuringStaticCall);
        assert!(host.logs.is_empty());
    }

    #[test]
    fn log_memory_oog() {
        let mut host = TestHost::default();
        let mut code = Vec::new();
        push(&mut code, 1);
        push(&mut code, Word::MAX);
        code.extend([op::LOG0, op::STOP]);
        let interp = run(RunConfig::new(code).host(&mut host));
        assert_matches!(interp.err, InstrStop::InvalidOperandOOG);
        assert!(host.logs.is_empty());
    }
}
