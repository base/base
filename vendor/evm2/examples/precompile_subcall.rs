//! Custom precompile that performs an interpreter subcall.

use alloy_primitives::{Address, Bytes, U256};
use evm2::{
    BaseEvmTypes, Evm, Precompiles, SpecId,
    bytecode::Bytecode,
    env::{BlockEnvExt, TxEnvExt},
    evm::{AccountInfo, InMemoryDB, precompile::PrecompileOutput},
    interpreter::{GasTracker, Host, InstrStop, Message, MessageExt, MessageKind, Word, op},
    precompiles::{Precompile, PrecompileError, PrecompileHalt, PrecompileId, PrecompileResult},
    registry::TxRegistry,
};

const PARENT: Address = Address::with_last_byte(0xaa);
const CUSTOM_PRECOMPILE: Address = Address::with_last_byte(0x42);
const SUBCALL_TARGET: Address = Address::with_last_byte(0xca);

fn main() {
    let mut evm = evm_with_custom_precompile();
    let mut message = MessageExt {
        kind: MessageKind::Call,
        gas_limit: 200_000,
        destination: PARENT,
        code: Bytecode::new_legacy(parent_code()),
        code_address: PARENT,
        ..MessageExt::default()
    };

    let result = Host::execute_message(&mut evm, &TxEnvExt::default(), &mut message);
    assert_eq!(result.stop, InstrStop::Return);
    assert_eq!(result.output.len(), 32);

    let returned = Word::from_be_slice(result.output.as_ref());
    assert_eq!(returned, Word::from(42));

    println!(
        "custom precompile staticcalled {SUBCALL_TARGET:?} and returned {returned} \
         using {} gas",
        result.gas.spent(),
    );
}

fn evm_with_custom_precompile() -> Evm<'static, BaseEvmTypes> {
    let mut database = InMemoryDB::default();
    database.insert_account_info(
        &SUBCALL_TARGET,
        AccountInfo::default().with_code(Bytecode::new_legacy(subcall_target_code())),
    );

    let mut precompiles = Precompiles::base(SpecId::OSAKA);
    precompiles.as_map_mut().insert(Precompile::new(
        CUSTOM_PRECOMPILE,
        PrecompileId::custom("STATICCALL_EXAMPLE"),
        staticcall_precompile,
    ));

    Evm::new(SpecId::OSAKA, BlockEnvExt::default(), TxRegistry::new(), database, precompiles)
}

fn staticcall_precompile(
    evm: &mut Evm<'_, BaseEvmTypes>,
    message: &Message,
    gas: &mut GasTracker,
) -> PrecompileResult {
    gas.spend(100)?;

    let loaded = Host::load_account(evm, &SUBCALL_TARGET, true, false)?;
    let child_gas_limit = gas.remaining().min(50_000);
    gas.spend(child_gas_limit)?;

    let mut child = MessageExt {
        kind: MessageKind::StaticCall,
        depth: message.depth.saturating_add(1),
        gas_limit: child_gas_limit,
        reservoir: gas.reservoir(),
        destination: SUBCALL_TARGET,
        caller: message.destination,
        input: message.input.clone(),
        value: U256::ZERO,
        code: loaded.code,
        code_address: SUBCALL_TARGET,
        caller_is_static: message.caller_is_static
            || matches!(message.kind, MessageKind::StaticCall),
        ..MessageExt::default()
    };

    let result = Host::execute_message(evm, &TxEnvExt::default(), &mut child);
    gas.merge_child_gas(result.gas, result.stop);

    match result.stop {
        stop if stop.is_success() => Ok(PrecompileOutput::new(result.output)),
        stop if stop.is_revert() => Err(PrecompileError::Revert(result.output)),
        stop if stop.is_fatal() => Err(stop.into()),
        stop => Err(PrecompileHalt::Other(format!("subcall halted with {stop:?}").into()).into()),
    }
}

const fn subcall_target_code() -> Bytes {
    Bytes::from_static(&[
        // Store U256(42) at memory offset 0.
        op::PUSH1,
        42,
        op::PUSH0,
        op::MSTORE,
        // Return memory[0..32].
        op::PUSH1,
        32,
        op::PUSH0,
        op::RETURN,
    ])
}

const fn parent_code() -> Bytes {
    Bytes::from_static(&[
        // CALL out size: copy 32 return bytes into memory.
        op::PUSH1,
        32,
        // CALL out offset: memory offset 0.
        op::PUSH0,
        // CALL input size: no calldata.
        op::PUSH0,
        // CALL input offset: memory offset 0.
        op::PUSH0,
        // CALL value: zero, so this is a plain CALL into the precompile.
        op::PUSH0,
        // CALL target: CUSTOM_PRECOMPILE = 0x0000000000000000000000000000000000000042.
        op::PUSH20,
        0,
        0,
        0,
        0,
        0,
        0,
        0,
        0,
        0,
        0,
        0,
        0,
        0,
        0,
        0,
        0,
        0,
        0,
        0,
        0x42,
        // CALL gas: 80,000 = 0x013880.
        op::PUSH3,
        0x01,
        0x38,
        0x80,
        op::CALL,
        // Drop the CALL success flag; the example asserts the final output below.
        op::POP,
        // Return memory[0..32], which now contains the precompile's subcall output.
        op::PUSH1,
        32,
        op::PUSH0,
        op::RETURN,
    ])
}
